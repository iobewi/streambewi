#![cfg_attr(not(test), no_std)]

//! StreamBeWI: a live internet radio presented to a USB host as a virtual `RADIO.MP3`.
//!
//! The target owns hardware startup and supplies prepared product resources.
//! Configuration policy, Improv, streaming and MSC remain in this portable crate.
//! The persisted USB mode is read before selecting native I/O in the target.

extern crate alloc;

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}

pub mod boot_policy;
pub mod logging;
pub mod provisioning;
pub mod stream;
pub mod usb;

use core::fmt::{Debug, Display};
use embassy_futures::join::{join, join3};
use embassy_net::Stack;
use embassy_usb::driver::Driver;
use embedded_hal_async::digital::Wait;
use embedded_io_async::{Read, Write};
use iobewi_config_space::ConfigBackend;
use iobewi_wifi_core::WifiTransport;
use iobewi_wifi_manager::WifiManager;

/// Run the product with boot-selected transports. The target prepares configuration
/// and selects JTAG or OTG before calling this function; there is no live handover.
pub async fn run<T, B, Btn, R, W, D, F>(
    boot: boot_policy::BootPolicy<B>,
    manager: WifiManager<T, B>,
    serial: impl IntoIterator<Item = (R, W)>,
    button: Btn,
    reset: F,
    usb: Option<D>,
    info: provisioning::DeviceInfo,
) where
    T: WifiTransport<NetworkHandle = Stack<'static>>,
    T::Address: Display,
    B: ConfigBackend,
    B::Error: Debug,
    Btn: Wait,
    R: Read,
    W: Write,
    D: Driver<'static>,
    F: FnMut(),
{
    if usb.is_some() != (boot.mode() == boot_policy::BootMode::MassStorage) {
        log::error!("target violated the USB boot-mode contract");
        core::future::pending::<()>().await;
        return;
    }
    let network = provisioning::NetworkSignal::new();
    let (readers, ports) = provisioning::take_ports(serial);
    let provisioning_fut = join(
        provisioning::wifi_task(manager, ports, &network, info, &boot),
        provisioning::readers(readers),
    );
    let recovery_fut = provisioning::recovery_watch(button, &boot, reset);
    let stream_fut = async {
        let stack = network.wait().await;
        stream::run(stack, &stream::STREAM).await
    };
    join(join3(provisioning_fut, recovery_fut, stream_fut), async {
        match usb {
            Some(driver) => usb::serve(driver).await,
            None => core::future::pending::<()>().await,
        }
    })
    .await;
}
