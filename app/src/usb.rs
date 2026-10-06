//! The StreamBeWI USB radio: a read-only Mass Storage disk with one virtual `RADIO.MP3` backed by the live
//! stream window. The USB device/PHY driver is a port: it is only created once the product is
//! CONFIGURED and the stream is ready (on some boards the USB pins are shared with the serial
//! port Improv uses).

use embassy_futures::join::join;
use embassy_time::{Duration, Timer};
use embassy_usb::{Builder, driver::Driver};
#[cfg(feature = "usb-debug")]
use embassy_usb::Handler;
use iobewi_fat16::{ReadStatus, VirtualFat16};
use iobewi_usb_msc::{InquiryIdentity, MscClass, ReadAction, ReadPolicy, State as MscState};
use streambewi_core::FAT16_CONFIG;

use crate::provisioning;
use crate::stream::{STREAM, SharedStreamSource};

/// Longest wait for stream data inside one READ(10) sector (Windows gave up after ~20 s).
const PENDING_TIMEOUT: Duration = Duration::from_secs(5);

const MSC_IDENTITY: InquiryIdentity = InquiryIdentity {
    vendor: *b"IOBEWI  ",
    product: *b"StreamBeWI      ",
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

/// Presents the USB disk once the product is configured and the stream has data. `start_driver`
/// creates the platform USB driver at that moment.
pub async fn serve<D, F>(start_driver: F)
where
    D: Driver<'static>,
    F: FnOnce() -> D,
{
    // UNCONFIGURED (no Wi-Fi network saved): no stream is possible, MSC stays off.
    while !provisioning::is_configured() {
        log::info!("provisioning: UNCONFIGURED, MSC off (waiting for Improv)");
        Timer::after(Duration::from_secs(5)).await;
    }
    log::info!("provisioning: CONFIGURED");

    // The USB disk is only presented once the stream has delivered the prebuffer: a visible
    // RADIO.MP3 with no audio behind it (Wi-Fi not provisioned/connected, stream down)
    // would look like an empty file to the host. Without data there is no USB device.
    while !STREAM.is_ready() {
        let (written, consumed) = STREAM.progress();
        log::info!("stream: prebuffer written={} consumed={}", written, consumed);
        Timer::after(Duration::from_millis(500)).await;
    }

    let (written, _) = STREAM.progress();
    log::info!("stream: prebuffer bytes={}", written);
    log::info!("usb: enabling MSC for Metronic");

    let driver = start_driver();

    let mut usb_config = embassy_usb::Config::new(0x303A, 0x4001);
    usb_config.manufacturer = Some("IOBEWI");
    usb_config.product = Some("StreamBeWI");
    usb_config.serial_number = Some("STREAMBEWI-0001");
    // Bus-powered by the player: Wi-Fi bursts draw far more than the former 100 mA declared.
    // 500 mA is the USB 2.0 maximum for a bus-powered device.
    usb_config.max_power = 500;
    usb_config.composite_with_iads = false;
    usb_config.device_class = 0x00;
    usb_config.device_sub_class = 0x00;
    usb_config.device_protocol = 0x00;

    let config_descriptor = mk_static!([u8; 256], [0; 256]);
    let bos_descriptor = mk_static!([u8; 256], [0; 256]);
    let control_buf = mk_static!([u8; 64], [0; 64]);
    let msc_state = mk_static!(MscState<'static>, MscState::new());
    #[cfg(feature = "usb-debug")]
    let bus_log = mk_static!(BusLog, BusLog);

    let mut builder = Builder::new(
        driver,
        usb_config,
        config_descriptor,
        bos_descriptor,
        &mut [],
        control_buf,
    );

    #[cfg(feature = "usb-debug")]
    builder.handler(bus_log);

    let mut msc = MscClass::new(&mut builder, msc_state, MSC_IDENTITY);
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
}
