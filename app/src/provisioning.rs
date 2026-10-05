//! Wi-Fi provisioning for StreamBeWI: Improv Serial (ESP Web Tools) over up to two serial ports,
//! on top of IOBEWI's `WifiManager` (portable policy). Credentials persist through ConfigSpace.
//! Nothing platform-specific lives here: transports, storage, button and reset are ports.

use alloc::string::ToString;

use core::convert::Infallible;
use core::fmt::{Debug, Display};
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex},
    channel::Channel,
    signal::Signal,
};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_hal_async::digital::Wait;
use embedded_io_async::{ErrorType, Read, Write};
use improv_serial::{self as improv, Command, ImprovError, ParsedCommand, Parser, State};
use iobewi_config_space::ConfigBackend;
use iobewi_wifi_core::{WifiProvisioning, WifiTransport};
use iobewi_wifi_manager::{LinkObserver, MaintainError, Sleep, WifiManager};

struct EmbassySleep;

impl Sleep for EmbassySleep {
    async fn sleep_ms(&self, ms: u32) {
        Timer::after(Duration::from_millis(ms as u64)).await;
    }
}

/// Publishes the network stack once the link is up: the transport creates the stack lazily, so it
/// is only known after the first connection. Not `Send` (the stack is single-executor), hence a
/// local signal owned by `run` rather than a static.
pub type NetworkSignal = Signal<NoopRawMutex, Stack<'static>>;

struct LogObserver<'a> {
    network: &'a NetworkSignal,
}

impl LinkObserver<Stack<'static>> for LogObserver<'_> {
    fn link_down(&mut self) {
        log::info!("wifi: link down, reconnecting");
    }
    fn ready(&mut self, network: Stack<'static>) {
        log::info!("wifi: ready");
        self.network.signal(network);
    }
}

/// How long the recovery button must be held while running to forget the saved Wi-Fi network.
const RECOVERY_HOLD: Duration = Duration::from_secs(3);

/// True once a Wi-Fi network is saved (CONFIGURED); false while UNCONFIGURED.
static CONFIGURED: AtomicBool = AtomicBool::new(false);

pub fn is_configured() -> bool {
    CONFIGURED.load(Ordering::Relaxed)
}

/// Watches the active-low recovery button: held for `RECOVERY_HOLD`, it erases the saved Wi-Fi
/// network and restarts the device through `reset`, which brings it back to UNCONFIGURED.
pub async fn recovery_watch<Btn: Wait, B: ConfigBackend>(
    mut button: Btn,
    backend: B,
    reset: fn() -> !,
) -> ! {
    loop {
        if button.wait_for_low().await.is_err() {
            Timer::after(Duration::from_millis(100)).await;
            continue;
        }
        if with_timeout(RECOVERY_HOLD, button.wait_for_high()).await.is_err() {
            match backend.clear("wifi").await {
                Ok(_) => log::info!("provisioning: button held, Wi-Fi config erased; restarting"),
                Err(_) => log::info!("provisioning: button held, erase FAILED; restarting"),
            }
            reset();
        }
    }
}

pub type Manager<T, B> = WifiManager<T, B>;

static IMPROV_COMMANDS: Channel<CriticalSectionRawMutex, (Port, ParsedCommand), 2> = Channel::new();

/// A serial port Improv can be provisioned over. Boards expose a USB-UART bridge, a native
/// USB-Serial-JTAG, or both: both are served and each reply goes back on the port the request
/// came from.
#[derive(Clone, Copy)]
pub enum Port {
    A,
    B,
}

/// The transmit halves of every Improv port.
pub struct Ports<W1, W2> {
    pub a: W1,
    pub b: W2,
}

struct Reply<'a, W1, W2> {
    ports: &'a mut Ports<W1, W2>,
    port: Port,
}

impl<W1: Write, W2: Write> Reply<'_, W1, W2> {
    async fn send(&mut self, frame: &[u8]) {
        match self.port {
            Port::A => write_all(&mut self.ports.a, frame).await,
            Port::B => write_all(&mut self.ports.b, frame).await,
        }
    }
}

async fn write_all<W: Write>(tx: &mut W, frame: &[u8]) {
    let mut rest = frame;
    while !rest.is_empty() {
        match tx.write(rest).await {
            Ok(n) => rest = &rest[n..],
            Err(_) => return,
        }
    }
    let _ = tx.flush().await;
}

/// A serial port that does not exist on this board: never receives, discards everything.
pub struct NoSerial;

impl ErrorType for NoSerial {
    type Error = Infallible;
}

impl Read for NoSerial {
    async fn read(&mut self, _buf: &mut [u8]) -> Result<usize, Self::Error> {
        core::future::pending().await
    }
}

impl Write for NoSerial {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        Ok(buf.len())
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Reads one port (the one ESP Web Tools talks to) and forwards parsed Improv commands. Log
/// lines on the same wire are ignored by the parser (it resynchronises).
pub async fn improv_reader<R: Read>(port: Port, mut rx: R) -> ! {
    let mut parser = Parser::new();
    let mut buf = [0u8; 64];
    loop {
        match rx.read(&mut buf).await {
            Ok(n) => {
                for &byte in &buf[..n] {
                    if let Some(command) = parser.feed(byte) {
                        IMPROV_COMMANDS.send((port, command)).await;
                    }
                }
            }
            Err(_) => Timer::after(Duration::from_millis(10)).await,
        }
    }
}

/// Owns the Wi-Fi manager: keeps the saved network connected and serves Improv requests.
/// A request pre-empts `maintain()`; it restarts afterwards (and the transport keeps a link that
/// is already up on the same credentials).
pub async fn wifi_task<T, B, W1, W2>(
    mut manager: Manager<T, B>,
    mut ports: Ports<W1, W2>,
    network: &NetworkSignal,
    chip: &'static [u8],
) -> !
where
    T: WifiTransport<NetworkHandle = Stack<'static>>,
    T::Address: Display,
    B: ConfigBackend,
    B::Error: Debug,
    W1: Write,
    W2: Write,
{
    let sleep = EmbassySleep;
    let mut observer = LogObserver { network };
    let mut state = State::Authorized;

    loop {
        CONFIGURED.store(true, Ordering::Relaxed);
        *(&mut state) = if manager.is_online() { State::Provisioned } else { State::Authorized };
        match select(
            manager.maintain(&sleep, &mut observer),
            IMPROV_COMMANDS.receive(),
        )
        .await
        {
            Either::First(MaintainError::NotProvisioned) => {
                CONFIGURED.store(false, Ordering::Relaxed);
                log::info!("wifi: no saved credentials; waiting for Improv provisioning");
                let (port, command) = IMPROV_COMMANDS.receive().await;
                handle(command, Reply { ports: &mut ports, port }, &mut state, &mut manager, chip).await;
            }
            Either::Second((port, command)) => {
                handle(command, Reply { ports: &mut ports, port }, &mut state, &mut manager, chip).await
            }
        }
    }
}

async fn handle<T, B, W1, W2>(
    command: ParsedCommand,
    mut tx: Reply<'_, W1, W2>,
    state: &mut State,
    manager: &mut Manager<T, B>,
    chip: &'static [u8],
) where
    T: WifiTransport<NetworkHandle = Stack<'static>>,
    T::Address: Display,
    B: ConfigBackend,
    B::Error: Debug,
    W1: Write,
    W2: Write,
{
    match command {
        ParsedCommand::GetCurrentState => {
            tx.send(&improv::state_frame(*state)).await;
            // ESP Web Tools also awaits an RPC result when already provisioned.
            if *state == State::Provisioned {
                tx.send(&improv::rpc_response_frame(Command::GetCurrentState, &[])).await;
            }
        }
        ParsedCommand::GetDeviceInfo => {
            let frame = improv::rpc_response_frame(
                Command::GetDeviceInfo,
                &[b"streambewi", b"0.2.0", chip, b"streambewi"],
            );
            tx.send(&frame).await;
        }
        ParsedCommand::GetWifiNetworks => {
            for network in WifiProvisioning::scan(manager).await {
                let signal = network.signal_strength.to_string();
                let secured: &[u8] = if network.secured { b"YES" } else { b"NO" };
                let frame = improv::rpc_response_frame(
                    Command::GetWifiNetworks,
                    &[network.ssid.as_bytes(), signal.as_bytes(), secured],
                );
                tx.send(&frame).await;
            }
            tx.send(&improv::rpc_response_frame(Command::GetWifiNetworks, &[])).await;
        }
        ParsedCommand::GetNetworkState => {
            let flags: &[u8] = if manager.is_online() { b"3" } else { b"2" };
            tx.send(&improv::rpc_response_frame(Command::GetNetworkState, &[flags])).await;
        }
        ParsedCommand::WifiSettings(settings) => {
            log::info!("improv: provisioning ssid={}", settings.ssid);
            *state = State::Provisioning;
            tx.send(&improv::state_frame(*state)).await;
            if WifiProvisioning::provision(manager, &settings.ssid, settings.password).await {
                log::info!("improv: provisioned, credentials saved");
                *state = State::Provisioned;
                tx.send(&improv::state_frame(*state)).await;
                tx.send(&improv::rpc_response_frame(Command::WifiSettings, &[])).await;
            } else {
                log::info!("improv: provisioning failed");
                *state = State::Authorized;
                tx.send(&improv::error_frame(ImprovError::UnableToConnect)).await;
                tx.send(&improv::state_frame(*state)).await;
            }
        }
        ParsedCommand::Unsupported(command) => {
            log::info!("improv: unsupported command 0x{:02X}", command);
            tx.send(&improv::error_frame(ImprovError::UnknownRpc)).await;
        }
    }
}
