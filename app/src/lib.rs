#![cfg_attr(not(test), no_std)]

//! StreamBeWI: a live internet radio presented to a USB host as a virtual `RADIO.MP3`.
//!
//! The product consumes an IOBEWI [`Board`](iobewi_board::Board). The target delegates
//! startup to `entry!`; configuration policy, Improv, streaming and MSC remain here.
//! The persisted USB mode is read before selecting the boot I/O. MSC enumeration waits
//! for the audio prebuffer. No physical console shares the Improv transport.

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
use iobewi_board::{Board, BootIoFactory, ResourceRequest, UsbBootMode};
use iobewi_config_space::ConfigBackend;
use iobewi_device::DeviceMetadata;
use iobewi_wifi_core::WifiTransport;
use iobewi_wifi_manager::WifiManager;

/// Product requirements, validated by the board before startup.
/// DHCP, DNS and HTTP each need one socket.
pub const BOARD_RESOURCES: ResourceRequest = ResourceRequest {
    sockets: 3,
    heap_bytes: 96 * 1024,
    minimum_stack_bytes: 16 * 1024,
};

/// Runs the product on an owned board. Network bounds belong to this product.
pub async fn run<B: Board>(board: B)
where
    B::Wifi: WifiTransport<NetworkHandle = Stack<'static>>,
    <B::Wifi as WifiTransport>::Address: Display,
    <B::Config as ConfigBackend>::Error: Debug,
{
    let parts = board.into_parts();
    let (boot, wifi_config) = match boot_policy::BootPolicy::prepare(parts.config).await {
        Ok(prepared) => prepared,
        Err(error) => {
            log::error!("configuration admission failed: {:?}", error);
            core::future::pending::<()>().await;
            return;
        }
    };
    if let Some(error) = boot.read_error() {
        log::error!(
            "USB boot flag unreadable: {:?}; provisioning this boot",
            error
        );
    }
    let mode = match boot.mode() {
        boot_policy::BootMode::Provisioning => UsbBootMode::Provisioning,
        boot_policy::BootMode::MassStorage => UsbBootMode::MassStorage,
    };
    let io = match parts.io.select(mode) {
        Ok(io) => io,
        Err(error) => {
            log::error!("boot I/O selection failed: {:?}", error);
            core::future::pending::<()>().await;
            return;
        }
    };
    if io.usb.is_some() != (mode == UsbBootMode::MassStorage) {
        log::error!("board violated the USB boot-mode contract");
        core::future::pending::<()>().await;
        return;
    }
    let manager = WifiManager::new(parts.wifi, wifi_config);
    let network = provisioning::NetworkSignal::new();
    let (readers, ports) = provisioning::take_ports(io.serial);
    let provisioning_fut = join(
        provisioning::wifi_task(
            manager,
            ports,
            &network,
            parts.identity.chip_name().as_bytes(),
            &boot,
        ),
        provisioning::readers(readers),
    );
    let recovery_fut = provisioning::recovery_watch(parts.button, &boot, parts.reset);
    let stream_fut = async {
        let stack = network.wait().await;
        stream::run(stack, &stream::STREAM).await
    };
    join(join3(provisioning_fut, recovery_fut, stream_fut), async {
        match io.usb {
            Some(driver) => usb::serve(driver).await,
            None => core::future::pending::<()>().await,
        }
    })
    .await;
}
