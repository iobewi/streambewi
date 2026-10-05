#![no_std]
#![no_main]

extern crate alloc;

mod flash_config;
mod stream;
mod wifi;

use embassy_executor::Spawner;
use core::cell::RefCell;

use embassy_futures::join::join;
use embassy_net::{Runner, StackResources};
use embassy_time::{Duration, Timer};
use embassy_sync::blocking_mutex::Mutex;
use embassy_usb::Builder;
#[cfg(feature = "usb-debug")]
use embassy_usb::Handler;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Pull},
    rng::Rng,
    timer::timg::TimerGroup,
    usb::{
        otg::{
            Usb,
            embassy_usb_device::{Config as OtgConfig, Driver},
        },
        usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
    },
};
use esp_radio::wifi::{ControllerConfig, Interface, WifiController};
use esp_storage::FlashStorage;
use flash_config::{FlashConfigBackend, SharedFlash};
use iobewi_config_space::ConfigManager;
use iobewi_wifi_manager::{CONFIG_BUDGET, WifiManager};
use iobewi_fat16::{ReadStatus, VirtualFat16};
use iobewi_usb_msc::{InquiryIdentity, MscClass, ReadAction, ReadPolicy, State as MscState};
use stream::{STREAM, SharedStreamSource};
use usb_radio_core::FAT16_CONFIG;

esp_bootloader_esp_idf::esp_app_desc!();

/// Longest wait for stream data inside one READ(10) sector (Windows gave up after ~20 s).
const PENDING_TIMEOUT: Duration = Duration::from_secs(5);

const MSC_IDENTITY: InquiryIdentity = InquiryIdentity {
    vendor: *b"IOBEWI  ",
    product: *b"USB RADIO POC   ",
    revision: *b"0001",
};

/// Product policy for unavailable sectors: wait for data that is about to arrive, but never
/// wedge the MSC on a stalled stream; expired data is answered with zeroes.
struct StreamReadPolicy;

impl ReadPolicy for StreamReadPolicy {
    fn unavailable(&mut self, status: ReadStatus, elapsed: Duration) -> ReadAction {
        match status {
            ReadStatus::Pending if elapsed < PENDING_TIMEOUT => {
                ReadAction::RetryAfter(Duration::from_millis(5))
            }
            _ => ReadAction::ZeroFill,
        }
    }
}

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        CELL.uninit().write($val)
    }};
}

/// Logs USB bus lifecycle events to show how far a host's enumeration gets (`usb-debug`).
#[cfg(feature = "usb-debug")]
struct BusLog;

#[cfg(feature = "usb-debug")]
impl Handler for BusLog {
    fn enabled(&mut self, enabled: bool) {
        log::info!("usb: enabled={}", enabled);
    }
    fn reset(&mut self) {
        log::info!("usb: bus reset");
    }
    fn addressed(&mut self, addr: u8) {
        log::info!("usb: addressed={}", addr);
    }
    fn configured(&mut self, configured: bool) {
        log::info!("usb: configured={}", configured);
    }
    fn suspended(&mut self, suspended: bool) {
        log::info!("usb: suspended={}", suspended);
    }
}

#[esp_hal::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();
    log::info!("usb-radio POC: P3 continuous HTTP MP3 -> USB MSC");
    log::info!("usb-radio POC: DP=GPIO20 DM=GPIO19");
    log::info!("stream: {}", stream::STREAM_URL);

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Wi-Fi's vendor driver needs a C-compatible heap. Keep the MP3 ring itself static
    // so its memory use is deterministic and independent from this allocator.
    esp_alloc::heap_allocator!(size: 96 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Wi-Fi credentials come from Improv Serial (ESP Web Tools) and live in flash.
    let flash = mk_static!(
        SharedFlash,
        Mutex::new(RefCell::new(FlashStorage::new(peripherals.FLASH)))
    );
    let mut config_manager = ConfigManager::new(FlashConfigBackend::new(flash));
    // Recovery: BOOT (GPIO0) is a strapping pin, so it cannot be held at power-up (low at reset
    // enters the ROM download mode). It is watched once the firmware runs instead.
    let boot_button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));
    spawner.spawn(recovery_task(boot_button, FlashConfigBackend::new(flash)).unwrap());
    let wifi_space = config_manager
        .claim("wifi", CONFIG_BUDGET)
        .expect("wifi config space");

    let wifi_interface = Interface::station();
    let controller =
        WifiController::new(peripherals.WIFI, ControllerConfig::default()).unwrap();

    let net_config = embassy_net::Config::dhcpv4(Default::default());
    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        net_config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner.spawn(net_task(runner).unwrap());

    // Improv Serial runs over the USB-Serial-JTAG (the native USB port, the only one some
    // ESP32-S3 boards expose). It shares the port with the log output and with the OTG
    // controller, so the OTG (MSC) is only started once the device is CONFIGURED.
    let (jtag_rx, jtag_tx) = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split();
    let manager = WifiManager::new(wifi::EspWifiTransport::new(controller, stack), wifi_space);
    spawner.spawn(improv_task(jtag_rx).unwrap());
    spawner.spawn(wifi_task(manager, jtag_tx).unwrap());

    let stream_fut = stream::run(stack, &STREAM);

    let usb_fut = async {
        // UNCONFIGURED (no Wi-Fi network saved): no stream is possible, MSC stays off.
        while !wifi::is_configured() {
            log::info!("provisioning: UNCONFIGURED, MSC off (waiting for Improv)");
            Timer::after(Duration::from_secs(5)).await;
        }
        log::info!("provisioning: CONFIGURED");

        // The USB disk is only presented once the stream has delivered the prebuffer: a visible
        // RADIO.MP3 with no audio behind it (Wi-Fi not provisioned/connected, stream down)
        // would look like an empty file to the host. Without data there is no USB device.
        while !STREAM.is_ready() {
            let (written, consumed) = STREAM.progress();
            log::info!(
                "stream: prebuffer written={} consumed={}",
                written,
                consumed
            );
            Timer::after(Duration::from_millis(500)).await;
        }

        let (written, _) = STREAM.progress();
        log::info!("stream: prebuffer bytes={}", written);
        log::info!("usb: enabling MSC for Metronic");

        let usb = Usb::new_fs(
            peripherals.USB_FS,
            peripherals.GPIO20,
            peripherals.GPIO19,
        );

        let mut ep_out_buffer = [0u8; 1024];
        let driver = Driver::new(usb, &mut ep_out_buffer, OtgConfig::default());

        let mut usb_config = embassy_usb::Config::new(0x303A, 0x4001);
        usb_config.manufacturer = Some("IOBEWI");
        usb_config.product = Some("USB Radio POC");
        usb_config.serial_number = Some("RADIO-POC-0002");
        // Bus-powered by the player: Wi-Fi bursts draw far more than the former 100 mA declared.
        // 500 mA is the USB 2.0 maximum for a bus-powered device.
        usb_config.max_power = 500;
        usb_config.composite_with_iads = false;
        usb_config.device_class = 0x00;
        usb_config.device_sub_class = 0x00;
        usb_config.device_protocol = 0x00;

        let mut config_descriptor = [0u8; 256];
        let mut bos_descriptor = [0u8; 256];
        let mut control_buf = [0u8; 64];

        let mut msc_state = MscState::new();
        #[cfg(feature = "usb-debug")]
        let mut bus_log = BusLog;

        let mut builder = Builder::new(
            driver,
            usb_config,
            &mut config_descriptor,
            &mut bos_descriptor,
            &mut [],
            &mut control_buf,
        );

        #[cfg(feature = "usb-debug")]
        builder.handler(&mut bus_log);

        let mut msc = MscClass::new(&mut builder, &mut msc_state, MSC_IDENTITY);
        let mut usb_device = builder.build();
        let mut disk = VirtualFat16::new(SharedStreamSource::new(&STREAM), FAT16_CONFIG)
            .expect("valid FAT16 config");

        let device_fut = usb_device.run();
        let msc_fut = async {
            if let Err(err) = msc.run(&mut disk, &mut StreamReadPolicy).await {
                log::info!("msc: fatal endpoint error: {:?}", err);
            }
        };

        join(device_fut, msc_fut).await;
    };

    join(stream_fut, usb_fut).await;
}

#[embassy_executor::task]
async fn improv_task(rx: UsbSerialJtagRx<'static, esp_hal::Async>) {
    wifi::improv_reader(rx).await
}

#[embassy_executor::task]
async fn wifi_task(manager: wifi::Manager, tx: UsbSerialJtagTx<'static, esp_hal::Async>) {
    wifi::wifi_task(manager, tx).await
}

#[embassy_executor::task]
async fn recovery_task(button: Input<'static>, backend: FlashConfigBackend) {
    wifi::recovery_watch(button, backend).await
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}
