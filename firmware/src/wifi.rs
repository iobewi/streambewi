//! Wi-Fi provisioning for StreamBeWI: Improv Serial (ESP Web Tools) on UART0 and USB-Serial-JTAG,
//! on top of IOBEWI's `WifiManager` (portable policy) and the IOBEWI ESP Wi-Fi driver, with
//! credentials persisted in ConfigSpace over NVS. Nothing is baked in at build time.

use alloc::string::ToString;

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_sync::{blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex}, channel::Channel, signal::Signal};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::{Read, Write};
use esp_hal::{
    Async,
    gpio::Input,
    uart::UartTx,
    usb::usb_serial_jtag::UsbSerialJtagTx,
};
use improv_serial::{self as improv, Command, ImprovError, ParsedCommand, Parser, State};
use iobewi_config_space::ConfigBackend;
use iobewi_esp_config_space::NvsConfigBackend;
use iobewi_wifi_core::WifiProvisioning;
use iobewi_wifi_manager::{LinkObserver, MaintainError, Sleep, WifiManager};

struct EmbassySleep;

impl Sleep for EmbassySleep {
    async fn sleep_ms(&self, ms: u32) {
        Timer::after(Duration::from_millis(ms as u64)).await;
    }
}

/// Publishes the network stack once the link is up: the driver creates the stack lazily, so it
/// is only known after the first connection. Not `Send` (the stack is single-executor), hence a
/// local signal owned by the composition root rather than a static.
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

/// How long BOOT must be held while running to forget the saved Wi-Fi network.
const RECOVERY_HOLD: Duration = Duration::from_secs(3);

/// True once a Wi-Fi network is saved (CONFIGURED); false while UNCONFIGURED.
static CONFIGURED: AtomicBool = AtomicBool::new(false);

pub fn is_configured() -> bool {
    CONFIGURED.load(Ordering::Relaxed)
}

/// Watches the (active-low) BOOT button: held for `RECOVERY_HOLD`, it erases the saved Wi-Fi
/// network and restarts, which brings the device back to UNCONFIGURED.
pub async fn recovery_watch(mut button: Input<'static>, backend: NvsConfigBackend) -> ! {
    loop {
        button.wait_for_low().await;
        if with_timeout(RECOVERY_HOLD, button.wait_for_high()).await.is_err() {
            match backend.clear("wifi").await {
                Ok(_) => log::info!("provisioning: BOOT held, Wi-Fi config erased; restarting"),
                Err(_) => log::info!("provisioning: BOOT held, erase FAILED; restarting"),
            }
            iobewi_esp_reset::software_reset();
        }
    }
}

/// Sockets reserved in the network stack: the HTTP stream, DNS and DHCP.
pub const NET_SOCKETS: usize = 3;

pub type Manager = WifiManager<iobewi_esp_wifi::WifiManager<NET_SOCKETS>, NvsConfigBackend>;

static IMPROV_COMMANDS: Channel<CriticalSectionRawMutex, (Port, ParsedCommand), 2> = Channel::new();

/// A serial port Improv can be provisioned over. Boards expose the USB-UART bridge (UART0),
/// the native USB-Serial-JTAG, or both: both are served and each reply goes back on the port
/// the request came from.
#[derive(Clone, Copy)]
pub enum Port {
    Uart,
    Jtag,
}

/// The transmit halves of every Improv port.
pub struct Ports {
    pub uart: UartTx<'static, Async>,
    pub jtag: UsbSerialJtagTx<'static, Async>,
}

struct Reply<'a> {
    ports: &'a mut Ports,
    port: Port,
}

impl Reply<'_> {
    async fn send(&mut self, frame: &[u8]) {
        match self.port {
            Port::Uart => write_all(&mut self.ports.uart, frame).await,
            Port::Jtag => write_all(&mut self.ports.jtag, frame).await,
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
/// A request pre-empts `maintain()`; it restarts afterwards (and `connect` is a no-op when the
/// link is already up on the same credentials).
pub async fn wifi_task(mut manager: Manager, mut ports: Ports, network: &NetworkSignal) -> ! {
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
                handle(command, Reply { ports: &mut ports, port }, &mut state, &mut manager).await;
            }
            Either::Second((port, command)) => {
                handle(command, Reply { ports: &mut ports, port }, &mut state, &mut manager).await
            }
        }
    }
}

async fn handle(
    command: ParsedCommand,
    mut tx: Reply<'_>,
    state: &mut State,
    manager: &mut Manager,
) {
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
                &[b"usb-radio-poc", b"0.2.0", b"ESP32-S3", b"usb-radio"],
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
