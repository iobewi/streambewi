//! Wi-Fi for the POC: Improv Serial provisioning over USB-Serial-JTAG (ESP Web Tools) on top of IOBEWI's portable
//! `WifiManager`, with credentials persisted by `flash_config`. Nothing is baked in at build time.
//!
//! The ESP-specific parts (radio transport, UART) are adapters local to this POC because
//! IOBEWI's ESP adapters pin a different esp-hal than the rest of the POC.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::{Read, Write};
use esp_hal::{
    Async,
    gpio::Input,
    uart::UartTx,
    usb::usb_serial_jtag::UsbSerialJtagTx,
};
use esp_radio::wifi::{
    AuthenticationMethod, AuthenticationMethodConfig, Config as WifiConfig, WifiController,
    scan::ScanConfig, sta::StationConfig,
};
use improv_serial::{self as improv, Command, ImprovError, ParsedCommand, Parser, State};
use iobewi_config_space::ConfigBackend;
use iobewi_wifi_core::{Network, WifiProvisioning, WifiTransport};
use iobewi_wifi_manager::{LinkObserver, MaintainError, Sleep, WifiManager};

use crate::flash_config::FlashConfigBackend;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);

pub struct EspWifiTransport {
    controller: WifiController<'static>,
    stack: Stack<'static>,
    current: Option<(String, String)>,
}

impl EspWifiTransport {
    pub fn new(controller: WifiController<'static>, stack: Stack<'static>) -> Self {
        Self { controller, stack, current: None }
    }
}

impl WifiTransport for EspWifiTransport {
    type Address = embassy_net::Ipv4Address;
    type NetworkHandle = Stack<'static>;

    async fn connect(&mut self, ssid: &str, password: String) -> bool {
        // Already up on these exact credentials (e.g. maintain() restarted after an Improv
        // request): do not bounce the link under a running stream.
        if self.stack.is_config_up() {
            if let Some((s, p)) = &self.current {
                if s == ssid && *p == password {
                    return true;
                }
            }
        }
        self.current = None;

        let Ok(ssid_cfg) = ssid.try_into() else {
            log::info!("wifi: invalid ssid");
            return false;
        };
        let station = StationConfig::default().with_ssid(ssid_cfg);
        let station = if password.is_empty() {
            station
        } else {
            let Ok(pw) = password.as_str().try_into() else {
                log::info!("wifi: invalid password");
                return false;
            };
            station.with_authentication(AuthenticationMethodConfig::Wpa2Personal(pw))
        };

        // Err(NotConnected) when idle is expected.
        let _ = self.controller.disconnect_async().await;
        if self.controller.set_config(&WifiConfig::Station(station)).is_err() {
            log::info!("wifi: set_config failed");
            return false;
        }

        log::info!("wifi: connecting ssid={}", ssid);
        match with_timeout(CONNECT_TIMEOUT, self.controller.connect_async()).await {
            Ok(Ok(info)) => log::info!("wifi: associated {:?}", info),
            Ok(Err(err)) => {
                log::info!("wifi: connect error {:?}", err);
                return false;
            }
            Err(_) => {
                log::info!("wifi: connect timeout");
                let _ = self.controller.disconnect_async().await;
                return false;
            }
        }
        if with_timeout(DHCP_TIMEOUT, self.stack.wait_config_up()).await.is_err() {
            log::info!("wifi: DHCP timeout");
            let _ = self.controller.disconnect_async().await;
            return false;
        }
        if let Some(config) = self.stack.config_v4() {
            log::info!("wifi: got IP {}", config.address);
        }
        self.current = Some((ssid.to_string(), password));
        true
    }

    async fn scan(&mut self) -> Vec<Network> {
        let config = ScanConfig::default().with_max(20);
        match self.controller.scan_async(&config).await {
            Ok(list) => list
                .into_iter()
                .map(|ap| Network {
                    ssid: ap.ssid.as_str().to_string(),
                    signal_strength: ap.signal_strength,
                    secured: !matches!(ap.auth_method, None | Some(AuthenticationMethod::None)),
                })
                .collect(),
            Err(err) => {
                log::info!("wifi: scan error {:?}", err);
                Vec::new()
            }
        }
    }

    async fn wait_down(&mut self) {
        if !self.stack.is_config_up() {
            return;
        }
        // Radio link lost, or DHCP/IP configuration lost.
        let _ = select(
            self.controller.wait_for_disconnect_async(),
            self.stack.wait_config_down(),
        )
        .await;
        self.current = None;
    }

    fn ip(&self) -> Option<Self::Address> {
        self.stack.config_v4().map(|c| c.address.address())
    }

    fn network_handle(&self) -> Option<Self::NetworkHandle> {
        self.stack.is_config_up().then_some(self.stack)
    }

    fn is_online(&self) -> bool {
        self.stack.is_config_up()
    }
}

struct EmbassySleep;

impl Sleep for EmbassySleep {
    async fn sleep_ms(&self, ms: u32) {
        Timer::after(Duration::from_millis(ms as u64)).await;
    }
}

struct LogObserver;

impl LinkObserver<Stack<'static>> for LogObserver {
    fn link_down(&mut self) {
        log::info!("wifi: link down, reconnecting");
    }
    fn ready(&mut self, _network: Stack<'static>) {
        log::info!("wifi: ready");
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
pub async fn recovery_watch(mut button: Input<'static>, backend: FlashConfigBackend) -> ! {
    loop {
        button.wait_for_low().await;
        if with_timeout(RECOVERY_HOLD, button.wait_for_high()).await.is_err() {
            match backend.clear("wifi").await {
                Ok(_) => log::info!("provisioning: BOOT held, Wi-Fi config erased; restarting"),
                Err(_) => log::info!("provisioning: BOOT held, erase FAILED; restarting"),
            }
            esp_hal::system::software_reset();
        }
    }
}

pub type Manager = WifiManager<EspWifiTransport, FlashConfigBackend>;

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
pub async fn wifi_task(mut manager: Manager, mut ports: Ports) -> ! {
    let sleep = EmbassySleep;
    let mut observer = LogObserver;
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
