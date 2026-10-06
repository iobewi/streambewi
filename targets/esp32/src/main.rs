//! ESP32-S3 target of StreamBeWI: the only place that names chip peripherals and the IOBEWI ESP
//! drivers. It builds the platform ports and hands them to the portable product (`streambewi`).

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_net::StackResources;
use esp_alloc as _;
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
use streambewi::{
    NoSerial, Platform, Serial,
    boot_policy::{BootMode, BootPolicy},
};

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
    iobewi_log::install(|_| {}, "");

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

    let (boot, wifi_config) = BootPolicy::prepare(config)
        .await
        .expect("configuration budgets");
    if let Some(error) = boot.read_error() {
        log::error!("USB boot flag unreadable: {error:?}; provisioning without erasure");
    }
    let boot_mode = boot.mode();
    let reset = SystemReset(peripherals.RTC_TIMER);

    // BOOT (GPIO0) is a strapping pin: it cannot be held at power-up (low at reset enters the ROM
    // download mode), so the product watches it once running.
    let button = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );

    // UART0 remains available in both modes. Native JTAG is constructed only
    // in the provisioning branch; OTG only in an MSC boot after prebuffer.
    let uart = Uart::new(peripherals.UART0, UartConfig::default())
        .unwrap()
        .with_rx(peripherals.GPIO44)
        .with_tx(peripherals.GPIO43)
        .into_async();
    let (uart_rx, uart_tx) = uart.split();

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
    let usb = move || {
        Driver::new(
            Usb::new_fs(usb_fs, dp, dm),
            ep_out_buffer,
            OtgConfig::default(),
        )
    };

    // Both native USB constructors follow the immutable product boot decision.
    // Provisioning retains JTAG for the entire boot, even after saving true.
    let usb = if boot_mode == BootMode::MassStorage {
        Some(usb)
    } else {
        None
    };
    macro_rules! run_product {
        ($serial_b:expr) => {
            streambewi::run(Platform {
                wifi,
                boot,
                wifi_config,
                serial_a: Serial {
                    rx: uart_rx,
                    tx: uart_tx,
                },
                serial_b: $serial_b,
                button,
                reset,
                chip: b"ESP32-S3",
                usb,
            })
            .await
        };
    }
    match boot_mode {
        BootMode::Provisioning => {
            let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
                .into_async()
                .split();
            run_product!(Serial { rx, tx });
        }
        BootMode::MassStorage => run_product!(Serial {
            rx: NoSerial,
            tx: NoSerial
        }),
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

// Recovery must reset the complete system, including native USB peripheral state.
struct SystemReset(esp_hal::peripherals::RTC_TIMER<'static>);
impl streambewi::Reset for SystemReset {
    fn reset(self) -> ! {
        let _rtc = iobewi_esp_reset::arm_system_reset(self.0, 100);
        loop {
            core::hint::spin_loop();
        }
    }
}
