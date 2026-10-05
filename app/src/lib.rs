#![cfg_attr(not(test), no_std)]

//! StreamBeWI: a live internet radio presented to a USB host as a virtual `RADIO.MP3`.
//!
//! This crate is the product logic and is portable: it knows no chip, HAL or board. A target
//! (`targets/<chip>`) builds the platform ports and hands them to [`run`]:
//!
//! - Wi-Fi transport ([`WifiTransport`](iobewi_wifi_core::WifiTransport)) and persistent
//!   configuration ([`ConfigBackend`](iobewi_config_space::ConfigBackend)): IOBEWI adapters;
//! - up to two serial ports for Improv provisioning (`embedded-io-async`);
//! - a recovery button (`embedded-hal-async` [`Wait`](embedded_hal_async::digital::Wait));
//! - a restart function;
//! - a factory creating the USB device driver (`embassy-usb` `Driver`), called late.
//!
//! Logging uses the `log` facade; the target installs the logger.

extern crate alloc;

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}

pub mod provisioning;
pub mod stream;
pub mod usb;

use core::fmt::{Debug, Display};

use embassy_futures::join::{join, join3};
use embassy_net::Stack;
use embassy_usb::driver::Driver;
use embedded_hal_async::digital::Wait;
use embedded_io_async::{Read, Write};
use iobewi_config_space::{ConfigBackend, ConfigManager};
use iobewi_wifi_core::WifiTransport;
use iobewi_wifi_manager::{CONFIG_BUDGET, WifiManager};

pub use provisioning::NoSerial;

/// One serial port: both directions.
pub struct Serial<R, W> {
    pub rx: R,
    pub tx: W,
}

/// Everything the target provides.
pub struct Platform<T, B, R1, W1, R2, W2, Btn, F> {
    /// Wi-Fi station transport; its network handle is the Embassy stack.
    pub wifi: T,
    /// Persistent configuration storage (Wi-Fi credentials).
    pub config: B,
    /// First Improv serial port (use [`NoSerial`] when the board has none).
    pub serial_a: Serial<R1, W1>,
    /// Second Improv serial port.
    pub serial_b: Serial<R2, W2>,
    /// Active-low recovery button.
    pub button: Btn,
    /// Restarts the device.
    pub reset: fn() -> !,
    /// Chip name reported over Improv (for example `b"ESP32-S3"`).
    pub chip: &'static [u8],
    /// Creates the USB device driver, once the product is configured and streaming.
    pub usb: F,
}

/// Runs the product. Returns only if the USB disk stops.
pub async fn run<T, B, R1, W1, R2, W2, Btn, D, F>(platform: Platform<T, B, R1, W1, R2, W2, Btn, F>)
where
    T: WifiTransport<NetworkHandle = Stack<'static>>,
    T::Address: Display,
    B: ConfigBackend + Clone,
    B::Error: Debug,
    R1: Read,
    W1: Write,
    R2: Read,
    W2: Write,
    Btn: Wait,
    D: Driver<'static>,
    F: FnOnce() -> D,
{
    log::info!("streambewi: continuous HTTP MP3 -> USB MSC");
    log::info!("stream: {}", stream::STREAM_URL);

    let mut config_manager = ConfigManager::new(platform.config.clone());
    let wifi_space = config_manager
        .claim("wifi", CONFIG_BUDGET)
        .expect("wifi config space");
    let manager = WifiManager::new(platform.wifi, wifi_space);

    let network = provisioning::NetworkSignal::new();
    let ports = provisioning::Ports { a: platform.serial_a.tx, b: platform.serial_b.tx };

    let provisioning_fut = join3(
        provisioning::wifi_task(manager, ports, &network, platform.chip),
        provisioning::improv_reader(provisioning::Port::A, platform.serial_a.rx),
        provisioning::improv_reader(provisioning::Port::B, platform.serial_b.rx),
    );
    let recovery_fut =
        provisioning::recovery_watch(platform.button, platform.config, platform.reset);

    let stream_fut = async {
        // The transport creates the network stack lazily: wait for the first link-up.
        let stack = network.wait().await;
        stream::run(stack, &stream::STREAM).await
    };

    join(
        join3(provisioning_fut, recovery_fut, stream_fut),
        usb::serve(platform.usb),
    )
    .await;
}
