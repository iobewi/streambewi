//! ESP32-S3 target of StreamBeWI: the only place that names chip peripherals and the IOBEWI ESP
//! drivers. It builds the platform ports and hands them to the portable product (`streambewi`).

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_net::StackResources;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Pull},
    timer::timg::TimerGroup,
    uart::{Config as UartConfig, Uart},
    usb::{
        otg::{
            Usb,
            embassy_usb_device::{Config as OtgConfig, Driver},
        },
        usb_serial_jtag::UsbSerialJtag,
    },
};
use iobewi_esp_config_space::{NvsConfigBackend, NvsPartition};
use streambewi::{Platform, Serial};

esp_bootloader_esp_idf::esp_app_desc!();

/// NVS partition of the default espflash partition table (`nvs`, 0x9000, 24 KiB).
const NVS_PARTITION: NvsPartition = NvsPartition::new(0x9000, 0x6000);

/// Sockets reserved in the network stack: the HTTP stream, DNS and DHCP.
const NET_SOCKETS: usize = 3;

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}

#[esp_hal::main]
async fn main(spawner: Spawner) {
    iobewi_log::install(iobewi_esp_console::console_print, "");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Wi-Fi's vendor driver needs a C-compatible heap. Keep the MP3 ring itself static
    // so its memory use is deterministic and independent from this allocator.
    esp_alloc::heap_allocator!(size: 96 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let flash = iobewi_esp_flash::init(peripherals.FLASH);
    let config = NvsConfigBackend::new(flash, NVS_PARTITION)
        .await
        .expect("NVS config backend");

    // BOOT (GPIO0) is a strapping pin: it cannot be held at power-up (low at reset enters the ROM
    // download mode), so the product watches it once running.
    let button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));

    // Improv Serial is served on both the USB-UART bridge (UART0) and the native USB-Serial-JTAG,
    // since boards expose one or both. The JTAG port shares GPIO19/20 with the OTG controller,
    // which the product only starts once configured.
    let uart = Uart::new(peripherals.UART0, UartConfig::default())
        .unwrap()
        .with_rx(peripherals.GPIO44)
        .with_tx(peripherals.GPIO43)
        .into_async();
    let (uart_rx, uart_tx) = uart.split();
    let (jtag_rx, jtag_tx) = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split();

    let wifi = iobewi_esp_wifi::WifiManager::new(
        peripherals.WIFI,
        spawner,
        mk_static!(
            StackResources<NET_SOCKETS>,
            StackResources::<NET_SOCKETS>::new()
        ),
    );

    let ep_out_buffer = mk_static!([u8; 1024], [0; 1024]);
    let (usb_fs, dp, dm) = (peripherals.USB_FS, peripherals.GPIO20, peripherals.GPIO19);
    let usb = move || Driver::new(Usb::new_fs(usb_fs, dp, dm), ep_out_buffer, OtgConfig::default());

    streambewi::run(Platform {
        wifi,
        config,
        serial_a: Serial { rx: uart_rx, tx: uart_tx },
        serial_b: Serial { rx: jtag_rx, tx: jtag_tx },
        button,
        reset: iobewi_esp_reset::software_reset,
        chip: b"ESP32-S3",
        usb,
    })
    .await;
}
