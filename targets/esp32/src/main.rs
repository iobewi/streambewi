#![no_std]
#![no_main]
#![recursion_limit = "256"]

extern crate alloc;
#[cfg(feature = "qual")]
mod qual_mem;

// The status LED sits on a different GPIO from one board to the next: the board is chosen at build time,
// and without a board the product has no LED and does not touch the RMT.
#[cfg(any(
    all(feature = "board-devkitc-1", feature = "board-devkitc-1-v1-1"),
    all(feature = "board-devkitc-1", feature = "board-s3-zero"),
    all(feature = "board-devkitc-1-v1-1", feature = "board-s3-zero"),
))]
compile_error!("Choose at most one board feature");
#[cfg(feature = "board-devkitc-1")]
type LedPin = esp_hal::peripherals::GPIO48<'static>;
#[cfg(feature = "board-devkitc-1-v1-1")]
type LedPin = esp_hal::peripherals::GPIO38<'static>;
#[cfg(feature = "board-s3-zero")]
type LedPin = esp_hal::peripherals::GPIO21<'static>;

/// The status LED: shows the look of the product's state (`streambewi::status`). A write error ends this
/// task and nothing else.
#[cfg(status_led)]
#[embassy_executor::task]
async fn status_led(
    observer: streambewi::status::Obs<'static>,
    ids: streambewi::status::Ids,
    mut led: iobewi_esp_ws2812::Ws2812<'static>,
) {
    streambewi::status::run_led(
        observer,
        ids,
        &streambewi::brightness::BRIGHTNESS,
        |color| led.set(color),
    )
    .await;
}
mod ota;
mod serial;

fn console(r: &log::Record<'_>) {
    esp_println::println!("{} - {}: {}", r.level(), r.target(), r.args());
}

#[cfg(feature = "qual")]
#[embassy_executor::task]
async fn qual_probe() {
    loop {
        embassy_time::Timer::after_secs(30).await;
        qual_mem::probe("streambewi::qual", "periodic");
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
use iobewi_config_space::storage::{EkvBackend, SpaceSpec, Store};
use iobewi_esp_config_space::EspConfigFlash;
use iobewi_esp_firmware_a::{platform::SystemBoot, service::BootRuntime};
use iobewi_firmware_update::{
    config_space::ConfigSpaceMetadataStore,
    metadata::PendingCheckAction,
    service::boot::{self, BootSelfCheck, BootStatus, Confirmation},
};
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

const AP_SOCKETS: usize = 3; // embassy-net's internal DNS socket, DHCP and one setup HTTP connection.
// DHCP, internal DNS, live stream (HTTP or HTTPS), local UI, SNTP. SNTP only runs in the station network; the
// AP network (`AP_SOCKETS`) never starts it. A fifth socket in a set sized for four makes smoltcp panic.
const SOCKETS: usize = 5;
// Measured on the S3 (qual image): heap peak ~60 KiB with the local UI; each HTTP request costs ~10 KiB of
// stack (peak ~31 KiB). 96 KiB of heap left a 26.6 KiB stack and every request overflowed it.
// The OTA (#3) puts a TLS *server* handshake (mbedtls) under the product task's frames in the maintenance start.
// With 80 KiB the stack kept ~1 to 3 KiB over the static bound of that path: 64 KiB gave it 16 KiB more, and the
// board measured (`qual`, maintenance, full upload then validation and OTM1 publication) a stack margin of
// 16 952 B but only 15 012 B of free heap, under the 16 KiB asked. 68 KiB moves 4 KiB back to the heap: about
// 19.1 KiB of heap and 12.9 KiB of stack margin expected, both over their thresholds (8 192 B of stack). The
// maintenance start has the heap to itself; the normal start measured 67 056 B free at its minimum.
const HEAP_BYTES: usize = 68 * 1024;
const MIN_STACK_BYTES: usize = 36 * 1024;
/// How long the disk stays off the USB bus before a software reset. Measured on the Metronic over 10
/// restarts each: 0 resumed after the bare RTC reset, 10 resumed with this detach (first read 0.8 to 1.1 s
/// after the MSC came up).
const DETACH_BEFORE_RESET_MS: u32 = 1_500;
const SPACES: &[SpaceSpec] = boot_policy::SPACES;

/// The probation watchdog (TIMG0), armed after the HAL and the RTOS timer, disarmed by the boot decision of
/// `iobewi-esp-firmware-a` (a stable boot or a confirmation). The first start of a blank `config` formats
/// 128 pages (a few seconds): the margin is for that, and for a candidate that never gets to its decision.
const WATCHDOG_MS: u64 = 30_000;
/// The self-check of a candidate image is raced against this deadline (the reference's).
const SELF_CHECK_MS: u64 = 15_000;

/// The minimal checks StreamBeWI asks of a candidate before confirming it: its configuration store reads
/// back (every live record and its CRC, read only). Nothing external is asked: not the Wi-Fi network, not
/// the radio station, not the player on the USB port. A broken candidate that cannot even read its
/// configuration is rejected; one that merely has no Internet is kept.
struct Check<'a>(&'a EkvBackend<'static, EspConfigFlash>);
impl BootSelfCheck for Check<'_> {
    async fn self_check(&self) -> bool {
        if cfg!(feature = "ota-hang-self-check") {
            core::future::pending::<()>().await;
        }
        self.0.self_check().await && !cfg!(feature = "ota-reject-self-check")
    }
}

/// A rollback (or a refused probation) restarts the product; the watchdog stays armed meanwhile.
pub(crate) async fn reset_for_rollback() -> ! {
    embassy_time::Timer::after(embassy_time::Duration::from_millis(200)).await;
    esp_hal::system::software_reset()
}

/// The one-shot request for a maintenance start: kept by the RTC RAM across a reset, zeroed by a power-on.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut MAINTENANCE_REQUEST: [u32; 2] = [0; 2];

fn keep_maintenance_request() {
    // SAFETY: one word pair, written before the restart and read once at the next start, from one task.
    unsafe {
        core::ptr::write_volatile(
            &raw mut MAINTENANCE_REQUEST,
            streambewi::ota::encode_request(),
        );
    }
}

/// Reads the request and clears it: the next restart is a normal one unless it asks again.
fn take_maintenance_request() -> bool {
    // SAFETY: as above.
    let words = unsafe { core::ptr::read_volatile(&raw const MAINTENANCE_REQUEST) };
    unsafe { core::ptr::write_volatile(&raw mut MAINTENANCE_REQUEST, streambewi::ota::CLEARED) };
    streambewi::ota::is_request(words)
}

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
    #[cfg(status_led)]
    rmt: esp_hal::peripherals::RMT<'static>,
    #[cfg(status_led)]
    led: LedPin,
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

/// The OTA boot gate (#3), as in `examples/firmware/ota-system`: which slot runs and in what state, the probation of
/// a candidate, its confirmation or its rollback. Before anything else of the product starts.
async fn ota_gate(
    system: &mut SystemBoot,
    shared: &'static iobewi_esp_flash::SharedFlash,
    backend: EkvBackend<'static, EspConfigFlash>,
    ota_store: &impl iobewi_firmware_update::metadata::MetadataStore,
) {
    {
        let check = Check(&backend);
        // A candidate that cannot even observe its own boot records stops here, watchdog armed.
        let Ok(runtime) = BootRuntime::new(system, shared, &check).await else {
            log::error!("ota: boot observation failed");
            loop {
                core::hint::spin_loop();
            }
        };
        let result = boot::on_boot(ota_store, &runtime).await;
        log::info!("ota: running {} status {:?}", result.slot, result.status);
        match result.status {
            BootStatus::Rollback => reset_for_rollback().await,
            BootStatus::PendingVerify => {
                match boot::pending_check(
                    &runtime,
                    embassy_time::Duration::from_millis(SELF_CHECK_MS),
                )
                .await
                {
                    PendingCheckAction::Confirm => {
                        match boot::confirm_pending(ota_store, &runtime).await {
                            Confirmation::Valid => log::info!("ota: confirmed"),
                            // Confirmed already; the OTM1 completion is retried at the next start.
                            Confirmation::Degraded => {
                                log::warn!("ota: confirmed; metadata completion failed")
                            }
                            Confirmation::ResetRequired => reset_for_rollback().await,
                        }
                    }
                    PendingCheckAction::Reject => {
                        boot::reject_pending(&runtime).await;
                        reset_for_rollback().await;
                    }
                    PendingCheckAction::Reset => reset_for_rollback().await,
                }
            }
            BootStatus::Stable => {
                if result.storage_healthy != Some(true) {
                    log::warn!("ota: stable; storage self-check failed");
                }
            }
            BootStatus::Degraded => log::warn!("ota: confirmed; metadata completion failed"),
        }
    }
}

#[embassy_executor::task]
async fn product(p: Resources, spawner: Spawner) {
    // The maintenance request is consumed first, so that a start that fails is not followed by another one.
    let maintenance_requested = take_maintenance_request();
    streambewi::ota::set_available(ota::configured());
    // `Booting` shows from here. A fatal startup error below keeps it on (see `boot_result`).
    streambewi::status::init();
    #[cfg(status_led)]
    {
        // No LED, no restitution: the product goes on without it.
        if let Ok(rmt) = esp_hal::rmt::Rmt::new(p.rmt, iobewi_esp_ws2812::RMT_FREQ)
            && let Ok(led) = iobewi_esp_ws2812::Ws2812::new(rmt.channel0, p.led)
            && let Some((observer, ids)) = streambewi::status::observer()
        {
            spawner.spawn(status_led(observer, ids, led).unwrap());
        }
    }
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
    // The OTA boot gate (#3), as in `examples/firmware/ota-system`: which slot runs and in what state, the
    // probation of a candidate, its confirmation or its rollback. Before anything else of the product starts.
    let ota_space = boot_result(
        BootPolicy::claim_ota(store.backend()),
        &mut uart,
        "STREAMBEWI:OTA-CLAIM",
    );
    let ota_store = ConfigSpaceMetadataStore(&ota_space);
    let mut system = boot_result(
        SystemBoot::discover(shared).await,
        &mut uart,
        "STREAMBEWI:OTA-DISCOVER",
    );
    // Out of line and boxed: the gate's frames are not those of the product task (`tools/stack_depth.py`).
    let gate: core::pin::Pin<alloc::boxed::Box<dyn core::future::Future<Output = ()> + '_>> =
        alloc::boxed::Box::pin(ota_gate(&mut system, shared, store.backend(), &ota_store));
    gate.await;
    // The settings first: the status LED stays dark until the saved brightness is known, so that a LED saved
    // at 0 % does not light up while the rest of the configuration is read. Reading never fails and never
    // writes; whatever is wrong with the record, the default brightness applies.
    let settings_space = boot_result(
        BootPolicy::claim_settings(store.backend()),
        &mut uart,
        "STREAMBEWI:SETTINGS-CLAIM",
    );
    let (settings, loaded) = streambewi::settings::SettingsStore::load(settings_space).await;
    if loaded != streambewi::settings::Loaded::Record
        && loaded != streambewi::settings::Loaded::Absent
    {
        log::warn!("settings: record not usable ({loaded:?}), default values apply");
    }
    streambewi::brightness::BRIGHTNESS.loaded(settings.led_percent().await);
    let settings = alloc::boxed::Box::new(settings);
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
    // Tells a reader of the journal that the product started again: drawn at each start, stored nowhere.
    diag.set_boot_id(esp_hal::rng::Rng::new().random());
    let (rx, tx) = uart.into_async().split();
    let mut serial = vec![(serial::Rx::Uart(rx), serial::Tx::Uart(tx))];
    // A maintenance start needs the saved Wi-Fi (the OTA is on the station network) and an image that can
    // serve it. Otherwise the request is dropped and the start is the normal one.
    let maintenance =
        maintenance_requested && boot.mode() == BootMode::MassStorage && ota::configured();
    if maintenance_requested && !maintenance {
        log::warn!(
            "maintenance: asked for, but no saved Wi-Fi or no OTA credentials in this image; normal start"
        );
    }
    // Read durable mode first; construct exactly one native USB controller (none in maintenance: the disk
    // is not presented, the host has seen it leave with the restart).
    let usb = if maintenance {
        None
    } else {
        match boot.mode() {
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
        // The restart that `POST /api/maintenance` asked for is a maintenance start: the request is kept in
        // the RTC RAM (a power cut forgets it).
        if streambewi::ota::maintenance_pending() {
            keep_maintenance_request();
        }
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
    if maintenance {
        log::info!("maintenance: the OTA server alone (no stream, no disk, no local page)");
        let ota_store = &ota_store;
        streambewi::run_maintenance(boot, manager, serial, button, reset, info, move |sta| {
            ota::boxed_serve(sta, shared, system, ota_store)
        })
        .await;
        return;
    }
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
        settings,
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
        // The detail of the product's own traces (`debug`) is for the bench only.
        let mut policy = iobewi_log::LogPolicy::new(log::LevelFilter::Warn);
        policy
            .add_target("iobewi_log", log::LevelFilter::Info)
            .unwrap();
        policy
            .add_target(streambewi::logging::LOG_TARGET, log::LevelFilter::Debug)
            .unwrap();
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
    // After the HAL and the RTOS timer reset TIMG0, before the partition discovery and the configuration.
    iobewi_esp_watchdog::arm_ms(WATCHDOG_MS);
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
        #[cfg(status_led)]
        rmt: p.RMT,
        #[cfg(all(status_led, feature = "board-devkitc-1"))]
        led: p.GPIO48,
        #[cfg(all(status_led, feature = "board-devkitc-1-v1-1"))]
        led: p.GPIO38,
        #[cfg(all(status_led, feature = "board-s3-zero"))]
        led: p.GPIO21,
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
