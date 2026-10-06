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
//! - a consuming restart port;
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

pub mod boot_policy;
pub mod provisioning;
pub mod stream;
pub mod usb;

use core::fmt::{Debug, Display};

use embassy_futures::join::{join, join3};
use embassy_net::Stack;
use embassy_usb::driver::Driver;
use embedded_hal_async::digital::Wait;
use embedded_io_async::{Read, Write};
use iobewi_config_space::{ConfigBackend, ConfigSpace};
use iobewi_wifi_core::WifiTransport;
use iobewi_wifi_manager::WifiManager;

pub use provisioning::NoSerial;

/// One serial port: both directions.
pub struct Serial<R, W> {
    pub rx: R,
    pub tx: W,
}

/// Consuming platform reset capability.
pub trait Reset {
    fn reset(self) -> !;
}

/// Everything the legacy target provides.
pub struct Platform<T, B: ConfigBackend, R1, W1, R2, W2, Btn, F, R> {
    /// Wi-Fi station transport; its network handle is the Embassy stack.
    pub wifi: T,
    /// Immutable boot selection and serialized persistence/recovery policy.
    pub boot: boot_policy::BootPolicy<B>,
    /// Wi-Fi space reserved together with the boot-policy budget.
    pub wifi_config: ConfigSpace<B>,
    /// First Improv serial port (use [`NoSerial`] when the board has none).
    pub serial_a: Serial<R1, W1>,
    /// Second Improv serial port.
    pub serial_b: Serial<R2, W2>,
    /// Active-low recovery button.
    pub button: Btn,
    /// Restarts the device.
    pub reset: R,
    /// Chip name reported over Improv (for example `b"ESP32-S3"`).
    pub chip: &'static [u8],
    /// None for a provisioning boot; Some creates the driver after prebuffer.
    pub usb: Option<F>,
}

/// Runs the product. Returns only if the USB disk stops.
pub async fn run<T, B, R1, W1, R2, W2, Btn, D, F, R>(
    platform: Platform<T, B, R1, W1, R2, W2, Btn, F, R>,
) where
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
    R: Reset,
{
    log::info!("streambewi: continuous HTTP MP3 -> USB MSC");
    log::info!("stream: {}", stream::STREAM_URL);

    let manager = WifiManager::new(platform.wifi, platform.wifi_config);
    let boot = platform.boot;

    let network = provisioning::NetworkSignal::new();
    let ports = provisioning::Ports {
        a: platform.serial_a.tx,
        b: platform.serial_b.tx,
    };

    let provisioning_fut = join3(
        provisioning::wifi_task(manager, ports, &network, platform.chip, &boot),
        provisioning::improv_reader(provisioning::Port::A, platform.serial_a.rx),
        provisioning::improv_reader(provisioning::Port::B, platform.serial_b.rx),
    );
    let recovery_fut = provisioning::recovery_watch(platform.button, &boot, platform.reset);

    let stream_fut = async {
        // The transport creates the network stack lazily: wait for the first link-up.
        let stack = network.wait().await;
        stream::run(stack, &stream::STREAM).await
    };

    join(join3(provisioning_fut, recovery_fut, stream_fut), async {
        match (boot.mode(), platform.usb) {
            (boot_policy::BootMode::MassStorage, Some(driver)) => usb::serve(driver).await,
            _ => core::future::pending::<()>().await,
        }
    })
    .await;
}
