//! Wi-Fi provisioning for StreamBeWI: Improv Serial (ESP Web Tools) over the board serial bank,
//! on top of IOBEWI's `WifiManager` (portable policy). Credentials persist through ConfigSpace.
//! Nothing platform-specific lives here: transports, storage, button and reset are ports.

use crate::boot_policy::{BootMode, BootPolicy, ProvisionError, Recovery};
use alloc::{boxed::Box, string::ToString, vec::Vec};
use core::{future::Future, task::Poll};
use iobewi_board::{Reset, SerialBank};

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
use embedded_io_async::{Read, Write};
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

/// BOOT recovery persists provisioning first, then clears Wi-Fi and restarts.
/// A failed flag commit preserves credentials and does not request reset.
pub async fn recovery_watch<Btn: Wait, B: ConfigBackend, R: Reset>(
    mut button: Btn,
    boot: &BootPolicy<B>,
    reset: R,
) -> ! {
    loop {
        if button.wait_for_low().await.is_err() {
            Timer::after(Duration::from_millis(100)).await;
            continue;
        }
        if with_timeout(RECOVERY_HOLD, button.wait_for_high())
            .await
            .is_err()
        {
            match boot.recover().await {
                Ok(Recovery::Cleared) => {
                    log::info!("recovery: provisioning flag and Wi-Fi cleared; restarting");
                    reset.reset();
                }
                Ok(Recovery::ResidualCredentials(_)) => {
                    log::error!(
                        "recovery: provisioning flag saved, Wi-Fi clear FAILED; restarting with residual credentials"
                    );
                    reset.reset();
                }
                Err(_) => {
                    log::error!("recovery: flag commit FAILED; credentials unchanged, no reset")
                }
            }
            // A failed recovery needs a new press, not a busy retry on held BOOT.
            while button.wait_for_high().await.is_err() {
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

pub type Manager<T, B> = WifiManager<T, B>;

static IMPROV_COMMANDS: Channel<CriticalSectionRawMutex, (Port, ParsedCommand), 2> = Channel::new();

/// Index assigned while consuming the finite serial bank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Port(pub usize);

/// Independently owned transmit halves, in bank order.
pub struct Ports<W> {
    pub tx: Vec<W>,
}

pub fn take_ports<S: SerialBank>(mut bank: S) -> (Vec<S::Rx>, Ports<S::Tx>) {
    let mut rx = Vec::new();
    let mut tx = Vec::new();
    while let Some(serial) = bank.take_next() {
        rx.push(serial.rx);
        tx.push(serial.tx);
    }
    (rx, Ports { tx })
}

/// Poll each independently owned reader; an empty bank remains pending.
pub async fn readers<R: Read>(rx: Vec<R>) -> ! {
    let mut futures: Vec<_> = rx
        .into_iter()
        .enumerate()
        .map(|(index, rx)| Box::pin(improv_reader(Port(index), rx)))
        .collect();
    core::future::poll_fn(|cx| {
        for future in &mut futures {
            let _ = future.as_mut().poll(cx);
        }
        Poll::<()>::Pending
    })
    .await;
    unreachable!()
}

struct Reply<'a, W> {
    ports: &'a mut Ports<W>,
    port: Port,
}

impl<W: Write> Reply<'_, W> {
    async fn send(&mut self, frame: &[u8]) {
        if let Some(tx) = self.ports.tx.get_mut(self.port.0) {
            write_all(tx, frame).await;
        }
    }
}

async fn write_all<W: Write>(tx: &mut W, frame: &[u8]) {
    let mut rest = frame;
    while !rest.is_empty() {
        match tx.write(rest).await {
            Ok(0) => return,
            Ok(n) => rest = &rest[n..],
            Err(_) => return,
        }
    }
    let _ = tx.flush().await;
}

/// Reads one port (the one ESP Web Tools talks to) and forwards parsed Improv commands. Log
/// lines on the same wire are ignored by the parser (it resynchronises).
pub async fn improv_reader<R: Read>(port: Port, mut rx: R) -> ! {
    let mut parser = Parser::new();
    let mut buf = [0u8; 64];
    loop {
        match rx.read(&mut buf).await {
            Ok(0) => Timer::after(Duration::from_millis(10)).await,
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
pub async fn wifi_task<T, B, W>(
    mut manager: Manager<T, B>,
    mut ports: Ports<W>,
    network: &NetworkSignal,
    chip: &'static [u8],
    boot: &BootPolicy<B>,
) -> !
where
    T: WifiTransport<NetworkHandle = Stack<'static>>,
    T::Address: Display,
    B: ConfigBackend,
    B::Error: Debug,
    W: Write,
{
    let sleep = EmbassySleep;
    let mut observer = LogObserver { network };
    let mut state = State::Authorized;
    let mut flag_committed = boot.mode() == BootMode::MassStorage;

    loop {
        CONFIGURED.store(true, Ordering::Relaxed);
        *(&mut state) = if flag_committed && manager.is_online() {
            State::Provisioned
        } else {
            State::Authorized
        };
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
                handle(
                    command,
                    Reply {
                        ports: &mut ports,
                        port,
                    },
                    &mut state,
                    &mut manager,
                    chip,
                    boot,
                    &mut flag_committed,
                )
                .await;
            }
            Either::Second((port, command)) => {
                handle(
                    command,
                    Reply {
                        ports: &mut ports,
                        port,
                    },
                    &mut state,
                    &mut manager,
                    chip,
                    boot,
                    &mut flag_committed,
                )
                .await
            }
        }
    }
}

async fn handle<T, B, W>(
    command: ParsedCommand,
    mut tx: Reply<'_, W>,
    state: &mut State,
    manager: &mut Manager<T, B>,
    chip: &'static [u8],
    boot: &BootPolicy<B>,
    flag_committed: &mut bool,
) where
    T: WifiTransport<NetworkHandle = Stack<'static>>,
    T::Address: Display,
    B: ConfigBackend,
    B::Error: Debug,
    W: Write,
{
    match command {
        ParsedCommand::GetCurrentState => {
            tx.send(&improv::state_frame(*state)).await;
            // ESP Web Tools also awaits an RPC result when already provisioned.
            if *state == State::Provisioned {
                tx.send(&improv::rpc_response_frame(Command::GetCurrentState, &[]))
                    .await;
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
            tx.send(&improv::rpc_response_frame(Command::GetWifiNetworks, &[]))
                .await;
        }
        ParsedCommand::GetNetworkState => {
            let flags: &[u8] = if manager.is_online() { b"3" } else { b"2" };
            tx.send(&improv::rpc_response_frame(
                Command::GetNetworkState,
                &[flags],
            ))
            .await;
        }
        ParsedCommand::WifiSettings(settings) => {
            log::info!("improv: provisioning ssid={}", settings.ssid);
            *state = State::Provisioning;
            tx.send(&improv::state_frame(*state)).await;
            let result = boot
                .provision(manager, &settings.ssid, settings.password)
                .await;
            if result.is_ok() {
                *flag_committed = true;
                log::info!("improv: credentials and OTG flag saved; restart required for MSC");
                *state = State::Provisioned;
                tx.send(&improv::state_frame(*state)).await;
                tx.send(&improv::rpc_response_frame(Command::WifiSettings, &[]))
                    .await;
            } else {
                match result {
                    Err(ProvisionError::Flag(_)) => {
                        *flag_committed = false;
                        log::error!(
                            "improv: credentials saved but OTG flag commit FAILED; reprovision to retry"
                        );
                    }
                    _ => log::info!("improv: provisioning failed"),
                }
                *state = State::Authorized;
                tx.send(&improv::error_frame(ImprovError::UnableToConnect))
                    .await;
                tx.send(&improv::state_frame(*state)).await;
            }
        }
        ParsedCommand::Unsupported(command) => {
            log::info!("improv: unsupported command 0x{:02X}", command);
            tx.send(&improv::error_frame(ImprovError::UnknownRpc)).await;
        }
    }
}

#[cfg(test)]
#[path = "provisioning_tests.rs"]
mod tests;
