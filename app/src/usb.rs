//! The StreamBeWI USB radio: a read-only Mass Storage disk with one virtual `RADIO.MP3` backed by the live
//! stream window. The USB device/PHY driver is a port: it is only created once the product is
//! CONFIGURED and the stream is ready (on some boards the USB pins are shared with the serial
//! port Improv uses).

use embassy_futures::{join::join3, select::select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
#[cfg(feature = "usb-debug")]
use embassy_usb::Handler;
use embassy_usb::{Builder, driver::Driver};
use iobewi_fat16::{ReadStatus, VirtualFat16};
use iobewi_usb_msc::{InquiryIdentity, MscClass, ReadAction, ReadPolicy, State as MscState};
use streambewi_core::{FAT16_CONFIG, PREBUFFER_BYTES};

use crate::provisioning;
use crate::stream::{STREAM, SharedStream, SharedStreamSource};

/// Longest total wait for stream data inside one READ(10) command (not per sector): a host aborts
/// a command after its own timeout (Windows gave up after ~20 s) whatever the number of sectors
/// it asked for, so a per-sector wait would accumulate beyond it (8 sectors x 5 s = 40 s).
const PENDING_TIMEOUT: Duration = Duration::from_secs(5);

/// A host that is starving for data while the producer has been silent this long finds nothing
/// valid to read: the device leaves the bus instead of answering with data it does not have.
const STALL_BEFORE_DROP: Duration = Duration::from_secs(2);
/// The stream has delivered nothing for longer than the buffer can cover, whatever the host does:
/// the device is off the bus until the stream is back, so that no host is left with a disk that
/// cannot be read. A reader that starves earlier (the Metronic, at the end of its buffered 5 s)
/// triggers `STALL_BEFORE_DROP` first.
const STALL_ABSOLUTE: Duration = Duration::from_secs(6);
/// How long the device stays off the bus, so that the host registers the removal.
const DISCONNECT_HOLD: Duration = Duration::from_millis(1500);
const RECONNECT_POLL: Duration = Duration::from_millis(200);

/// Soft-connect and soft-disconnect of the USB device, provided by the target (the portable
/// product cannot reach the controller).
pub trait UsbLink: Sync {
    fn set_connected(&self, connected: bool);
}

/// What the product needs to present the disk: the controller driver and its soft connection.
pub struct UsbPort<D> {
    pub driver: D,
    pub link: &'static dyn UsbLink,
}

/// Raised when a reader starves while the producer is silent; consumed by the link supervisor.
static STALL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// A reader waits for data (`Pending`) and the producer has delivered nothing for too long.
fn is_stall(status: ReadStatus, producer_idle: Duration) -> bool {
    status == ReadStatus::Pending && producer_idle >= STALL_BEFORE_DROP
}

const MSC_IDENTITY: InquiryIdentity = InquiryIdentity {
    vendor: *b"IOBEWI  ",
    product: *b"StreamBeWI      ",
    revision: *b"0001",
};

/// Decision for an unavailable sector, given how long the current READ(10) command has waited.
fn decide(status: ReadStatus, command_elapsed: Duration) -> ReadAction {
    match status {
        ReadStatus::Pending if command_elapsed < PENDING_TIMEOUT => {
            ReadAction::RetryAfter(Duration::from_millis(5))
        }
        _ => ReadAction::ZeroFill,
    }
}

/// Product policy for unavailable sectors: wait for data that is about to arrive, but never
/// wedge the MSC on a stalled stream; expired data is answered with zeroes. The wait budget is
/// shared by all the sectors of one READ(10) command.
struct StreamReadPolicy {
    command_start: Instant,
    stream: &'static SharedStream,
    stall: &'static Signal<CriticalSectionRawMutex, ()>,
}

impl StreamReadPolicy {
    fn new(
        stream: &'static SharedStream,
        stall: &'static Signal<CriticalSectionRawMutex, ()>,
    ) -> Self {
        Self {
            command_start: Instant::now(),
            stream,
            stall,
        }
    }
}

impl ReadPolicy for StreamReadPolicy {
    fn command_started(&mut self) {
        self.command_start = Instant::now();
    }

    fn unavailable(&mut self, status: ReadStatus, _sector_elapsed: Duration) -> ReadAction {
        if is_stall(status, self.stream.producer_idle()) {
            self.stall.signal(());
        }
        decide(status, self.command_start.elapsed())
    }
}

/// Takes the device off the bus when the stream stalls under a starving reader and presents it
/// again once the stream has delivered a fresh prebuffer, as at boot: the host never sees a file it
/// cannot read, and re-enumerates onto data that is current.
struct LinkSupervisor {
    stream: &'static SharedStream,
    stall: &'static Signal<CriticalSectionRawMutex, ()>,
    link: &'static dyn UsbLink,
    hold: Duration,
    poll: Duration,
    absolute: Duration,
}

impl LinkSupervisor {
    async fn run(&self) -> ! {
        loop {
            self.cycle().await;
        }
    }

    /// Resolves when the producer has been silent for `absolute`, whether or not a reader starves.
    async fn watch_idle(&self) {
        loop {
            Timer::after(self.poll).await;
            if self.stream.producer_idle() >= self.absolute {
                return;
            }
        }
    }

    async fn cycle(&self) {
        select(self.stall.wait(), self.watch_idle()).await;
        let (written_at_stall, _) = self.stream.progress();
        log::info!("usb: stream stalled, leaving the bus");
        // The player will resume at the offset it had: keep the session through the reconnection.
        self.stream.hold_session();
        self.link.set_connected(false);
        Timer::after(self.hold).await;
        while self.stream.progress().0 < written_at_stall + PREBUFFER_BYTES {
            Timer::after(self.poll).await;
        }
        // Reads that kept waiting while the device was off the bus must not drop it again.
        self.stall.reset();
        log::info!("usb: stream back, rejoining the bus");
        self.link.set_connected(true);
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

/// Enumerates the boot-selected USB driver once configuration and prebuffer are ready.
pub async fn serve<D>(port: UsbPort<D>)
where
    D: Driver<'static>,
{
    let UsbPort { driver, link } = port;
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
        if let Err(err) = msc
            .run(&mut disk, &mut StreamReadPolicy::new(&STREAM, &STALL))
            .await
        {
            log::info!("msc: fatal endpoint error: {:?}", err);
        }
    };

    let supervisor = LinkSupervisor {
        stream: &STREAM,
        stall: &STALL,
        link,
        hold: DISCONNECT_HOLD,
        poll: RECONNECT_POLL,
        absolute: STALL_ABSOLUTE,
    };
    join3(device_fut, msc_fut, supervisor.run()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_waits_within_the_command_budget_then_zero_fills() {
        let retry = ReadAction::RetryAfter(Duration::from_millis(5));
        assert_eq!(decide(ReadStatus::Pending, Duration::from_millis(0)), retry);
        assert_eq!(
            decide(ReadStatus::Pending, Duration::from_millis(4_999)),
            retry
        );
        assert_eq!(
            decide(ReadStatus::Pending, PENDING_TIMEOUT),
            ReadAction::ZeroFill
        );
        // Once the command has used its budget, every further sector of it is zero-filled at once.
        assert_eq!(
            decide(ReadStatus::Pending, Duration::from_secs(30)),
            ReadAction::ZeroFill
        );
    }

    #[test]
    fn expired_data_is_never_waited_for() {
        assert_eq!(
            decide(ReadStatus::Expired, Duration::from_millis(0)),
            ReadAction::ZeroFill
        );
    }

    #[test]
    fn a_new_command_gets_a_fresh_budget() {
        let stream: &'static SharedStream = Box::leak(Box::new(SharedStream::new()));
        let stall: &'static Signal<CriticalSectionRawMutex, ()> =
            Box::leak(Box::new(Signal::new()));
        let mut policy = StreamReadPolicy::new(stream, stall);
        policy.command_started();
        assert!(matches!(
            policy.unavailable(ReadStatus::Pending, Duration::from_secs(60)),
            ReadAction::RetryAfter(_)
        ));
    }

    #[test]
    fn a_reader_starving_under_a_silent_producer_is_a_stall() {
        let long = STALL_BEFORE_DROP;
        assert!(is_stall(ReadStatus::Pending, long));
        assert!(is_stall(ReadStatus::Pending, Duration::MAX));
        // Pacing at the live edge: the producer keeps delivering, the reader merely waits.
        assert!(!is_stall(ReadStatus::Pending, Duration::from_millis(500)));
        // Expired data is not a stall of the stream.
        assert!(!is_stall(ReadStatus::Expired, Duration::MAX));
    }

    struct FakeLink(std::sync::Mutex<Vec<bool>>);
    impl UsbLink for FakeLink {
        fn set_connected(&self, connected: bool) {
            self.0.lock().unwrap().push(connected);
        }
    }

    #[test]
    fn a_stall_leaves_the_bus_and_rejoins_after_a_fresh_prebuffer_keeping_the_session() {
        use iobewi_fat16::{FileSource, SECTOR_SIZE};
        let stream: &'static SharedStream = Box::leak(Box::new(SharedStream::new()));
        let stall: &'static Signal<CriticalSectionRawMutex, ()> =
            Box::leak(Box::new(Signal::new()));
        let link: &'static FakeLink = Box::leak(Box::new(FakeLink(Default::default())));
        let data = [0x55u8; 1024];
        for _ in 0..80 {
            stream.push(&data);
        }
        // A reader is served and has drained the window: it starves, as in a stream interruption.
        let mut source = SharedStreamSource::new(stream);
        source.begin_session();
        let mut sector = [0u8; SECTOR_SIZE];
        let mut index = 0;
        while source.read_file_sector(index, &mut sector) == ReadStatus::Ready {
            index += 1;
        }
        let (written_at_stall, consumed) = stream.progress();
        let supervisor = LinkSupervisor {
            stream,
            stall,
            link,
            hold: Duration::from_millis(30),
            poll: Duration::from_millis(5),
            absolute: Duration::from_secs(3600),
        };
        stall.signal(());
        embassy_futures::block_on(embassy_futures::join::join(supervisor.cycle(), async {
            Timer::after(Duration::from_millis(10)).await;
            assert_eq!(*link.0.lock().unwrap(), vec![false]);
            Timer::after(Duration::from_millis(60)).await;
            // Held down: the stream has not delivered a fresh prebuffer yet.
            assert_eq!(*link.0.lock().unwrap(), vec![false]);
            for _ in 0..70 {
                assert_eq!(stream.push(&data), data.len());
            }
        }));
        assert_eq!(*link.0.lock().unwrap(), vec![false, true]);
        assert!(stream.progress().0 >= written_at_stall + PREBUFFER_BYTES);
        // Reads that kept waiting during the hold do not drop the link again at once.
        assert!(!stall.signaled());
        // The host re-enumerates: ending and beginning its MSC session leaves the window alone, so
        // the player resumes at its own offset, on the data that arrived meanwhile.
        source.end_session();
        source.begin_session();
        assert_eq!(stream.progress().1, consumed);
        assert_eq!(
            source.read_file_sector(index, &mut sector),
            ReadStatus::Ready
        );
        assert!(sector.iter().all(|b| *b == 0x55));
        // Only that one reconnection is kept: the next session starts on the live edge as usual.
        source.end_session();
        source.begin_session();
        assert_ne!(stream.progress().1, consumed);
    }

    #[test]
    fn a_silent_producer_leaves_the_bus_even_when_no_reader_starves() {
        let stream: &'static SharedStream = Box::leak(Box::new(SharedStream::new()));
        let stall: &'static Signal<CriticalSectionRawMutex, ()> =
            Box::leak(Box::new(Signal::new()));
        let link: &'static FakeLink = Box::leak(Box::new(FakeLink(Default::default())));
        let data = [0x55u8; 1024];
        for _ in 0..80 {
            stream.push(&data);
        }
        let supervisor = LinkSupervisor {
            stream,
            stall,
            link,
            hold: Duration::from_millis(10),
            poll: Duration::from_millis(5),
            absolute: Duration::from_millis(60),
        };
        let (written_at_stall, _) = stream.progress();
        embassy_futures::block_on(embassy_futures::join::join(supervisor.cycle(), async {
            // The producer delivers nothing: after `absolute` the device has left the bus.
            Timer::after(Duration::from_millis(120)).await;
            assert_eq!(*link.0.lock().unwrap(), vec![false]);
            for _ in 0..70 {
                assert_eq!(stream.push(&data), data.len());
            }
        }));
        assert_eq!(*link.0.lock().unwrap(), vec![false, true]);
        assert!(stream.progress().0 >= written_at_stall + PREBUFFER_BYTES);
    }

    #[test]
    fn a_throttled_producer_is_not_idle() {
        let stream = SharedStream::new();
        stream.push(&[0u8; 16]);
        assert!(stream.producer_idle() < Duration::from_secs(1));
        stream.note_throttled();
        assert!(stream.producer_idle() < Duration::from_secs(1));
    }
}
