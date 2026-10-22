#![no_std]
#![no_main]
// The server future (three picoserve workers over the nested router) is deeper than the default limit.
#![recursion_limit = "256"]

extern crate alloc;
#[cfg(feature = "qual")]
mod qual_mem;
mod serial;

fn console(r: &log::Record<'_>) {
    esp_println::println!("{} - {}: {}", r.level(), r.target(), r.args());
}

#[cfg(feature = "qual")]
#[embassy_executor::task]
async fn qual_probe() {
    // Sample the per-region free heap every 50 ms, probe (stack paint included) every 30 s.
    let mut ticks = 0u32;
    qual_mem::probe("streambewi::qual", "boot");
    loop {
        embassy_time::Timer::after_millis(50).await;
        qual_mem::sample();
        ticks += 1;
        if ticks % 600 == 0 {
            qual_mem::probe("streambewi::qual", "periodic");
        }
    }
}

use alloc::vec;
use embassy_executor::Spawner;
use embassy_net::StackResources;
use esp_hal::{
    Blocking,
    gpio::{Input, InputConfig, Pull},
    peripherals::{
        FLASH, GPIO0, GPIO19, GPIO20, GPIO43, GPIO44, RTC_TIMER, UART0, USB_DEVICE, USB_FS, WIFI,
    },
    rtc_cntl::{Rtc, RwdtStage, RwdtStageAction},
    timer::timg::TimerGroup,
    uart::{Config as UartConfig, Uart},
    usb::usb_serial_jtag::UsbSerialJtag,
};
use iobewi_config_space::storage::{SpaceSpec, Store};
use iobewi_esp_config_space::EspConfigFlash;
use static_cell::StaticCell;
use streambewi::boot_policy::{self, BootMode, BootPolicy};

esp_bootloader_esp_idf::esp_app_desc!();

/// Soft-connect of the native USB device (DCTL.SDIS), used by the product when the stream stalls.
struct S3UsbLink;

impl streambewi::usb::UsbLink for S3UsbLink {
    fn set_connected(&self, connected: bool) {
        iobewi_esp_usb::set_connected(connected);
    }
}

static USB_LINK: S3UsbLink = S3UsbLink;

#[unsafe(no_mangle)]
pub static STREAMBEWI_CHIP: [u8; 8] = *b"ESP32-S3";

// embassy-net's internal DNS socket and DHCP, plus one socket per HTTP worker of the setup page.
const AP_SOCKETS: usize = 2 + streambewi::setup::WEB_WORKERS;
// DHCP, internal DNS, live stream (HTTP or HTTPS), SNTP, plus one per HTTP worker of the local UI. SNTP only
// runs in the station network; the AP network (`AP_SOCKETS`) never starts it. A socket more than the set
// holds makes smoltcp panic.
const SOCKETS: usize = 4 + streambewi::setup::WEB_WORKERS;
// Measured on the S3 (qual image): heap peak ~60 KiB with the local UI; each HTTP request costs ~10 KiB of
// stack (peak ~31 KiB). 96 KiB of heap left a 26.6 KiB stack and every request overflowed it.
const HEAP_BYTES: usize = 80 * 1024;
const MIN_STACK_BYTES: usize = 36 * 1024;
/// How long the disk stays off the USB bus before a software reset. Measured on the Metronic over 10
/// restarts each: 0 resumed after the bare RTC reset, 10 resumed with this detach (first read 0.8 to 1.1 s
/// after the MSC came up).
const DETACH_BEFORE_RESET_MS: u32 = 1_500;
const SPACES: &[SpaceSpec] = boot_policy::SPACES;

// One concrete S3 composition; there is no portable Board / resource factory.
struct Resources {
    flash: FLASH<'static>,
    wifi: WIFI<'static>,
    uart: UART0<'static>,
    rx: GPIO44<'static>,
    tx: GPIO43<'static>,
    button: GPIO0<'static>,
    rtc: RTC_TIMER<'static>,
    jtag: USB_DEVICE<'static>,
    usb: USB_FS<'static>,
    dp: GPIO20<'static>,
    dm: GPIO19<'static>,
}

// Fatal startup diagnostics are allowed before UART0 becomes an Improv port.
// One nonblocking FIFO attempt per byte: no retry, flush or USB register access.
fn boot_result<T, E: core::fmt::Debug>(
    result: Result<T, E>,
    uart: &mut Uart<'static, Blocking>,
    stage: &'static str,
) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            log::error!("startup {stage}: {error:?}");
            for byte in stage.as_bytes().iter().chain(b"\r\n") {
                if !uart.write_ready() {
                    break;
                }
                let _ = uart.write(core::slice::from_ref(byte));
            }
            loop {
                core::hint::spin_loop();
            }
        }
    }
}

#[embassy_executor::task]
async fn product(p: Resources, spawner: Spawner) {
    let mut uart = Uart::new(p.uart, UartConfig::default())
        .expect("UART0 unavailable")
        .with_rx(p.rx)
        .with_tx(p.tx);
    let shared = iobewi_esp_flash::init(p.flash);
    let flash = boot_result(
        EspConfigFlash::from_label(shared, "config").await,
        &mut uart,
        "STREAMBEWI:CONFIG-PARTITION",
    );
    let blank = boot_result(flash.is_blank().await, &mut uart, "STREAMBEWI:CONFIG-READ");
    static STORE: StaticCell<Store<EspConfigFlash>> = StaticCell::new();
    let store = STORE.init(boot_result(
        Store::new(flash, esp_hal::rng::Rng::new().random(), SPACES),
        &mut uart,
        "STREAMBEWI:CONFIG-MANIFEST",
    ));
    // Explicit factory policy: initialize a wholly blank region only.
    // Never format after mount errors or reinterpret a previous NVS region.
    if blank {
        boot_result(store.format().await, &mut uart, "STREAMBEWI:CONFIG-INIT");
    }
    boot_result(store.mount().await, &mut uart, "STREAMBEWI:CONFIG-MOUNT");
    let (boot, wifi_space) = boot_result(
        BootPolicy::prepare(store.backend()).await,
        &mut uart,
        "STREAMBEWI:CONFIG-CLAIMS",
    );
    if let Some(error) = boot.read_error() {
        log::error!("USB boot flag unreadable: {error:?}; provisioning this boot");
    }

    let station_space = boot_result(
        BootPolicy::claim_stations(store.backend()),
        &mut uart,
        "STREAMBEWI:STATIONS-CLAIM",
    );
    let radio = boot_result(
        streambewi::stations::Radio::load(station_space).await,
        &mut uart,
        "STREAMBEWI:STATIONS-LOAD",
    );
    let log_space = boot_result(
        BootPolicy::claim_log(store.backend()),
        &mut uart,
        "STREAMBEWI:LOG-CLAIM",
    );
    let diag = alloc::boxed::Box::new(
        streambewi::diag::Diag::load(log_space, streambewi::logging::LOG_TARGET).await,
    );
    let (rx, tx) = uart.into_async().split();
    let mut serial = vec![(serial::Rx::Uart(rx), serial::Tx::Uart(tx))];
    // Read durable mode first; construct exactly one native USB controller.
    let usb = match boot.mode() {
        BootMode::Provisioning => {
            let (rx, tx) = UsbSerialJtag::new(p.jtag).into_async().split();
            serial.push((serial::Rx::Jtag(rx), serial::Tx::Jtag(tx)));
            None
        }
        BootMode::MassStorage => {
            static OUT: StaticCell<[u8; 1024]> = StaticCell::new();
            Some(streambewi::usb::UsbPort {
                driver: iobewi_esp_usb::device(p.usb, p.dp, p.dm, OUT.init([0; 1024])),
                link: &USB_LINK,
            })
        }
    };
    static NET: StaticCell<StackResources<SOCKETS>> = StaticCell::new();
    static AP_NET: StaticCell<StackResources<AP_SOCKETS>> = StaticCell::new();
    let transport = iobewi_esp_wifi::WifiManager::<SOCKETS, AP_SOCKETS>::new(
        p.wifi,
        spawner,
        NET.init(StackResources::new()),
    )
    .with_access_point(AP_NET.init(StackResources::new()));
    let mut manager = iobewi_wifi_manager::WifiManager::new(transport, wifi_space);
    let setup_network = if boot.mode() == BootMode::Provisioning {
        let ap = streambewi::setup::access_point_config();
        if manager.start_access_point(&ap).await {
            log::info!("setup: AP {} ready; open http://172.23.241.1/", ap.ssid());
            manager.access_point_handle()
        } else {
            log::error!("setup: AP startup failed; Improv remains available");
            None
        }
    } else {
        None
    };
    let button = Input::new(p.button, InputConfig::default().with_pull(Pull::Up));
    let mut rtc = Rtc::new(p.rtc);
    let otg_active = usb.is_some();
    let reset = move || {
        if otg_active {
            // Only when the disk is presented over USB-OTG (never in the provisioning boot, where the
            // port belongs to USB-Serial/JTAG): let the host see the disk leave before the reset.
            iobewi_esp_usb::set_connected(false);
            esp_hal::delay::Delay::new().delay_millis(DETACH_BEFORE_RESET_MS);
        }
        // Preserve broad RTC system reset for the subsequent JTAG provisioning boot.
        rtc.rwdt
            .set_timeout(RwdtStage::Stage0, esp_hal::time::Duration::from_millis(100));
        rtc.rwdt
            .set_stage_action(RwdtStage::Stage0, RwdtStageAction::ResetSystem);
        rtc.rwdt.enable();
        loop {
            core::hint::spin_loop();
        }
    };
    let info = streambewi::provisioning::DeviceInfo {
        chip: &STREAMBEWI_CHIP,
        // The package version is what `esp_app_desc!` records in the image descriptor.
        version: env!("CARGO_PKG_VERSION").as_bytes(),
    };
    // Time source for the certificate checks, started once the station network exists (SNTP); the trust
    // anchors are the public roots in flash. `init` runs once, before any TLS session. An `http://` station
    // never waits for the clock: only the TLS connector refuses to dial before it is set.
    let tls = iobewi_esp_tls::init(iobewi_time::now);
    let secure = move |sta: embassy_net::Stack<'static>| {
        spawner.spawn(
            iobewi_ntp::sync_task(
                sta,
                iobewi_ntp::SyncOptions {
                    server: "pool.ntp.org",
                    resync_period: embassy_time::Duration::from_secs(3600),
                    retry_period: embassy_time::Duration::from_secs(10),
                    exchange_timeout: embassy_time::Duration::from_secs(5),
                    // 2026-01-01T00:00:00Z: an answer below it is not a clock.
                    plausible_epoch_floor: 1_767_225_600,
                },
            )
            .unwrap(),
        );
        iobewi_tls_service::client::StaticTrustConnector {
            dialer: iobewi_esp_tls::embassy::EspTlsDialer { tls, stack: sta },
            ca_pem: iobewi_tls_roots::BUNDLE,
            clock_is_set: iobewi_time::is_set,
        }
    };
    streambewi::run(
        boot,
        manager,
        serial,
        button,
        reset,
        usb,
        info,
        setup_network,
        radio,
        diag,
        secure,
    )
    .await;
}

#[esp_hal::main]
fn main() -> ! {
    #[cfg(feature = "qual")]
    qual_mem::paint();
    let p =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));
    streambewi::logging::install(console); // Operating log on UART0 (Improv ignores non-frame text).
    #[cfg(feature = "qual")]
    {
        let mut policy = iobewi_log::default_policy(streambewi::logging::LOG_TARGET).unwrap();
        policy
            .add_target("iobewi_usb_msc", log::LevelFilter::Info)
            .unwrap();
        policy
            .add_target("embassy_usb", log::LevelFilter::Trace)
            .unwrap();
        iobewi_log::apply_policy(policy);
    }
    esp_alloc::heap_allocator!(size: HEAP_BYTES);
    // The DRAM the 2nd stage bootloader frees (`dram2_seg`, 73 744 bytes), as a second region of its own: the
    // TLS session (16 KiB record buffer, MbedTLS state, certificate chain) needs more than the main heap
    // leaves, and the audio ring, the stack and the main heap are unchanged. esp-alloc tries the regions in
    // the order they were added. The address and size come from the linker script (`dram2_seg`), not from
    // arithmetic on the main heap.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73_744);
    unsafe extern "C" {
        static _stack_start: u8;
        static _stack_end: u8;
    }
    let stack = (&raw const _stack_start as usize) - (&raw const _stack_end as usize);
    assert!(
        stack >= MIN_STACK_BYTES,
        "insufficient linker stack reservation"
    );
    let timer = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timer.timer0, p.FROM_CPU_INTR0);
    let resources = Resources {
        flash: p.FLASH,
        wifi: p.WIFI,
        uart: p.UART0,
        rx: p.GPIO44,
        tx: p.GPIO43,
        button: p.GPIO0,
        rtc: p.RTC_TIMER,
        jtag: p.USB_DEVICE,
        usb: p.USB_FS,
        dp: p.GPIO20,
        dm: p.GPIO19,
    };
    static EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();
    EXECUTOR
        .init(esp_rtos::embassy::Executor::new())
        .run(|spawner| {
            spawner.spawn(product(resources, spawner).unwrap());
            #[cfg(feature = "qual")]
            spawner.spawn(qual_probe().unwrap());
        })
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // Bench qualification build: the console is a plain UART0 write, no acquisition.
    #[cfg(feature = "qual")]
    esp_println::println!("PANIC: {_info}");
    // No USB/JTAG/UART acquisition from panic: these are owned by Improv/MSC.
    loop {
        core::hint::spin_loop();
    }
}
