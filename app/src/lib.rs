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
pub mod diag;
pub mod fetch;
pub mod logging;
pub mod provisioning;
pub mod setup;
pub mod stations;
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
#[allow(clippy::too_many_arguments)] // Concrete product resources composed by the target.
pub async fn run<T, B, Btn, R, W, D, F, C, S>(
    boot: boot_policy::BootPolicy<B>,
    manager: WifiManager<T, B>,
    serial: impl IntoIterator<Item = (R, W)>,
    button: Btn,
    reset: F,
    usb: Option<usb::UsbPort<D>>,
    info: provisioning::DeviceInfo,
    setup_network: Option<Stack<'static>>,
    radio: stations::Radio<B>,
    diag: alloc::boxed::Box<diag::Diag<B>>,
    secure: S,
) where
    S: FnOnce(Stack<'static>) -> C,
    C: iobewi_net_io::Connector,
    for<'a> <C::Connection<'a> as iobewi_net_io::ErrorType>::Error: Display,
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
    setup::set_setup(boot.mode() == boot_policy::BootMode::Provisioning);
    let network = provisioning::NetworkSignal::new();
    let (readers, ports) = provisioning::take_ports(serial);
    let provisioning_fut = join(
        provisioning::wifi_task(manager, ports, &network, info, &boot),
        provisioning::readers(readers),
    );
    let recovery_fut = provisioning::recovery_watch(button, &boot, reset);
    let link = usb.as_ref().map(|port| port.link);
    // Exactly one HTTP server per boot: the AP during setup, STA during normal operation.
    // Share the first STA-ready signal with the producer rather than giving Signal two waiters.
    let services = async {
        let (web_stack, station_stack) = match setup_network {
            Some(ap) => (ap, None),
            None => {
                let sta = network.wait().await;
                (sta, Some(sta))
            }
        };
        let producer = async {
            let sta = match station_stack {
                Some(sta) => sta,
                None => network.wait().await,
            };
            // The target builds the connector for `https` stations (and starts the clock it needs) once
            // the station network exists; `http` stations never wait for it.
            let secure = secure(sta);
            stream::run(
                sta,
                &stream::STREAM,
                &secure,
                link,
                async || radio.active_url().await,
                async || radio.persist().await.map_err(|_| ()),
            )
            .await
        };
        join3(
            producer,
            setup::serve(web_stack, &radio, &diag),
            diag::drain_loop(&diag),
        )
        .await;
    };
    join(join3(provisioning_fut, recovery_fut, services), async {
        match usb {
            Some(port) => usb::serve(port).await,
            None => core::future::pending::<()>().await,
        }
    })
    .await;
}
