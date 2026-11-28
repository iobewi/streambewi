//! The StreamBeWI USB radio: a read-only Mass Storage disk with one virtual `RADIO.MP3` backed by the live
//! stream window. The USB device/PHY driver is a port: it is only created once the product is
//! CONFIGURED and the stream is ready (on some boards the USB pins are shared with the serial
//! port Improv uses).

use embassy_futures::join::join3;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
#[cfg(feature = "usb-debug")]
use embassy_usb::Handler;
use embassy_usb::{Builder, driver::Driver};
use iobewi_fat16::{ReadOnlyBlockDevice, ReadStatus, SECTOR_SIZE, VirtualFat16};
use iobewi_usb_msc::{InquiryIdentity, MscClass, ReadAction, ReadPolicy, State as MscState};

use crate::provisioning;
#[cfg(test)]
use crate::stream::CONSUMER_IDLE_LIMIT;
use crate::stream::{STREAM, SharedStream, SharedStreamSource};
use crate::stream_policy::{FAT16_CONFIG, PREBUFFER_BYTES};

/// Longest total wait for stream data inside one READ(10) command (not per sector): a host aborts
/// a command after its own timeout (Windows gave up after ~20 s) whatever the number of sectors
/// it asked for, so a per-sector wait would accumulate beyond it (8 sectors x 5 s = 40 s).
const PENDING_TIMEOUT: Duration = Duration::from_secs(5);

/// A host that is starving for data while the producer has been silent this long finds nothing
/// valid to read: the device leaves the bus instead of answering with data it does not have.
const STALL_BEFORE_DROP: Duration = Duration::from_secs(2);
/// How long the device stays off the bus, so that the host registers the removal.
const DISCONNECT_HOLD: Duration = Duration::from_millis(1500);
/// Minimum inactivity before an explicit metadata-to-head retry can reopen a file.
const HEAD_REOPEN_IDLE_LIMIT: Duration = Duration::from_secs(2);
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

/// Forget a stall raised before the device left the bus on purpose (station change).
pub(crate) fn clear_stall() {
    STALL.reset();
}

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

/// The zero sectors served in a row, so that a long series does not fill the journal (and the blocking console)
/// with one line per sector: the first `FIRST` are logged, then one every `EVERY`, then a total when the
/// series is over. A series ends at the second READ(10) command after the last zero sector. It counts and
/// decides what to log; it changes nothing of what is served.
#[derive(Default)]
struct ZeroSeries {
    /// Sectors of the series so far.
    total: u32,
    /// Sectors of the current READ(10) command.
    in_command: u32,
}

impl ZeroSeries {
    const FIRST: u32 = 3;
    const EVERY: u32 = 128;

    /// A sector is served as zeroes. Returns its number in the series if it deserves a line.
    fn sector(&mut self) -> Option<u32> {
        self.total = self.total.saturating_add(1);
        self.in_command = self.in_command.saturating_add(1);
        (self.total <= Self::FIRST || self.total.is_multiple_of(Self::EVERY)).then_some(self.total)
    }

    /// A READ(10) command starts. Returns the total of a series that has just ended and whose sectors
    /// were not all logged one by one.
    fn command(&mut self) -> Option<u32> {
        let ended =
            (self.in_command == 0 && self.total > 0).then(|| core::mem::take(&mut self.total));
        self.in_command = 0;
        ended.filter(|total| *total > Self::FIRST)
    }

    /// The session ends: the series in progress, if its sectors were not all logged.
    fn finish(&mut self) -> Option<u32> {
        let total = core::mem::take(&mut self.total);
        self.in_command = 0;
        (total > Self::FIRST).then_some(total)
    }
}

/// Product policy for unavailable sectors: wait for data that is about to arrive, but never
/// wedge the MSC on a stalled stream; expired data is answered with zeroes. The wait budget is
/// shared by all the sectors of one READ(10) command.
struct StreamReadPolicy {
    command_start: Instant,
    stream: &'static SharedStream,
    stall: &'static Signal<CriticalSectionRawMutex, ()>,
    zeros: ZeroSeries,
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
            zeros: ZeroSeries::default(),
        }
    }
}

impl Drop for StreamReadPolicy {
    fn drop(&mut self) {
        if let Some(total) = self.zeros.finish() {
            log::warn!(
                "usb: ZEROFILL series ended with the session: {} sectors",
                total
            );
        }
    }
}

impl ReadPolicy for StreamReadPolicy {
    fn command_started(&mut self) {
        self.command_start = Instant::now();
        if let Some(total) = self.zeros.command() {
            log::warn!("usb: ZEROFILL series ended: {} sectors", total);
        }
    }

    fn unavailable(&mut self, status: ReadStatus, _sector_elapsed: Duration) -> ReadAction {
        if is_stall(status, self.stream.producer_idle()) && self.stream.recovery_start().is_some() {
            self.stall.signal(());
        }
        let action = decide(status, self.command_start.elapsed());
        if action == ReadAction::ZeroFill
            && let Some(n) = self.zeros.sector()
        {
            // Operating log: a zero sector served is a lost-data event. The first ones of a series are
            // logged, then one in `EVERY`, then the total (`ZeroSeries`).
            log::warn!(
                "usb: ZEROFILL #{} status={:?} command_elapsed={} ms",
                n,
                status,
                self.command_start.elapsed().as_millis()
            );
        }
        action
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
}

impl LinkSupervisor {
    async fn run(&self) -> ! {
        loop {
            self.cycle().await;
        }
    }

    async fn cycle(&self) {
        self.stall.wait().await;
        // A far-ahead request can be Pending even with a full window. Do not
        // disconnect a held session that cannot accept the recovery prebuffer.
        let Some(written_at_stall) = self.stream.recovery_start() else {
            return;
        };
        let generation = self.stream.generation();
        log::info!("usb: stream stalled, leaving the bus");
        log::debug!(
            "usb: stall recovery waits for {} more bytes",
            PREBUFFER_BYTES
        );
        crate::status::usb_off_bus();
        // The player will resume at the offset it had: keep the session through the reconnection.
        self.stream.hold_session();
        self.link.set_connected(false);
        Timer::after(self.hold).await;
        while self.stream.progress().0 < written_at_stall + PREBUFFER_BYTES
            && self.stream.generation() == generation
        {
            Timer::after(self.poll).await;
        }
        if self.stream.generation() != generation {
            // A station change took over the bus while this cycle waited: it rejoins by itself.
            log::info!("usb: stall cycle superseded by a station change");
            return;
        }
        // Reads that kept waiting while the device was off the bus must not drop it again.
        self.stall.reset();
        log::info!("usb: stream back, rejoining the bus");
        self.link.set_connected(true);
        crate::status::usb_ready();
    }
}

/// Exact metadata-to-file-head sequence observed on the Metronic after STOP (#47).
/// It is an open hint, not a SCSI PLAY command. A bare head re-read is not a new open.
#[derive(Default)]
struct OpenProbe(u8);

impl OpenProbe {
    fn observe(&mut self, lba: u32, root: u32, head: u32) -> bool {
        let (step, opened) = match (self.0, lba) {
            (1, iobewi_fat16::RESERVED_SECTORS) => (2, false),
            (2, lba) if lba == root => (3, false),
            (3, lba) if lba == head => (0, true),
            (_, lba) if lba == root => (1, false),
            _ => (0, false),
        };
        self.0 = step;
        opened
    }
}

/// Keep product-specific open recognition outside the generic FAT and MSC libraries.
struct StreamDisk {
    volume: VirtualFat16<SharedStreamSource>,
    source: SharedStreamSource,
    probe: OpenProbe,
    retry_after: Duration,
}

impl StreamDisk {
    fn new(stream: &'static SharedStream, retry_after: Duration) -> Self {
        let source = SharedStreamSource::new(stream);
        Self {
            volume: VirtualFat16::new(source, FAT16_CONFIG).expect("valid FAT16 config"),
            source,
            probe: OpenProbe::default(),
            retry_after,
        }
    }
}

impl ReadOnlyBlockDevice for StreamDisk {
    fn last_lba(&self) -> u32 {
        self.volume.last_lba()
    }

    fn begin_session(&mut self) {
        self.probe = OpenProbe::default();
        self.volume.begin_session();
    }

    fn end_session(&mut self) {
        self.probe = OpenProbe::default();
        self.volume.end_session();
    }

    fn read_sector(&mut self, lba: u32, out: &mut [u8; SECTOR_SIZE]) -> ReadStatus {
        let geometry = self.volume.geometry();
        let root = geometry.data_start_lba - geometry.root_sectors;
        if self.probe.observe(lba, root, geometry.data_start_lba) {
            self.source.retry_head_after_idle(self.retry_after);
        }
        self.volume.read_sector(lba, out)
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
    crate::status::usb_initializing();
    let mut waits = 0u32;
    while !STREAM.is_ready() {
        // One line when the wait starts, then one every 10 s: a silent stream is visible, not flooded.
        if waits.is_multiple_of(20) {
            let (written, consumed) = STREAM.progress();
            log::info!(
                "stream: waiting for the prebuffer written={} consumed={}",
                written,
                consumed
            );
        }
        waits = waits.wrapping_add(1);
        Timer::after(Duration::from_millis(500)).await;
    }

    let (written, _) = STREAM.progress();
    log::info!("stream: prebuffer bytes={}", written);
    log::info!("usb: enabling MSC for Metronic");
    crate::status::usb_ready();

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
    let mut disk = StreamDisk::new(&STREAM, HEAD_REOPEN_IDLE_LIMIT);

    let device_fut = usb_device.run();
    let msc_fut = async {
        if let Err(err) = msc
            .run(&mut disk, &mut StreamReadPolicy::new(&STREAM, &STALL))
            .await
        {
            log::info!("msc: fatal endpoint error: {:?}", err);
            crate::status::usb_error();
        }
    };

    let supervisor = LinkSupervisor {
        stream: &STREAM,
        stall: &STALL,
        link,
        hold: DISCONNECT_HOLD,
        poll: RECONNECT_POLL,
    };
    join3(device_fut, msc_fut, supervisor.run()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_open_pattern_excludes_bare_head_rereads() {
        let mut probe = OpenProbe::default();
        for lba in [261, 261, 259, 261, 259, 1, 259] {
            assert!(!probe.observe(lba, 259, 261));
        }
        assert!(probe.observe(261, 259, 261));
        assert!(!probe.observe(261, 259, 261));
    }

    fn retry_fixture() -> (&'static SharedStream, StreamDisk) {
        let stream: &'static SharedStream = Box::leak(Box::new(SharedStream::new()));
        for tag in 0..160u8 {
            assert_eq!(stream.push(&[tag; SECTOR_SIZE]), SECTOR_SIZE);
        }
        let mut disk = StreamDisk::new(stream, Duration::from_millis(20));
        disk.begin_session();
        (stream, disk)
    }

    fn open_head(disk: &mut StreamDisk) -> [u8; SECTOR_SIZE] {
        let mut sector = [0; SECTOR_SIZE];
        for lba in [259, 1, 259, 261] {
            assert_eq!(disk.read_sector(lba, &mut sector), ReadStatus::Ready);
        }
        sector
    }

    fn fill_window(stream: &SharedStream, tag: u8) {
        while stream.push(&[tag; SECTOR_SIZE]) > 0 {}
    }

    #[test]
    fn head_retry_respects_recognition_boundary_and_idle() {
        use crate::stream_policy::HEAD_SECTORS;

        for count in [1, 3, HEAD_SECTORS, HEAD_SECTORS + 1] {
            let (stream, mut disk) = retry_fixture();
            let first = open_head(&mut disk);
            let mut sector = [0; SECTOR_SIZE];
            for index in 1..count {
                assert_eq!(
                    disk.read_sector(261 + index, &mut sector),
                    ReadStatus::Ready
                );
            }
            fill_window(stream, 0xF1);
            // Even the full metadata pattern must not rebase a recent read.
            assert_eq!(open_head(&mut disk), first);
            embassy_futures::block_on(Timer::after(Duration::from_millis(30)));
            // A bare head read must not rebase, even after inactivity.
            assert_eq!(disk.read_sector(261, &mut sector), ReadStatus::Ready);
            assert_eq!(sector, first);
            embassy_futures::block_on(Timer::after(Duration::from_millis(30)));
            let reopened = open_head(&mut disk);
            if count <= HEAD_SECTORS {
                assert_ne!(reopened, first, "count={count}");
            } else {
                assert_eq!(reopened, first, "count={count}");
            }
            assert_eq!(disk.read_sector(261, &mut sector), ReadStatus::Ready);
            assert_eq!(sector, reopened);
        }
    }

    #[test]
    fn successive_three_sector_open_failures_can_renew_the_origin() {
        let (stream, mut disk) = retry_fixture();
        let mut first = open_head(&mut disk);
        let mut sector = [0; SECTOR_SIZE];
        let mut consumed = stream.progress().1;
        for tag in [0xE1, 0xE2, 0xE3] {
            for lba in [261, 262, 263, 261, 262] {
                assert_eq!(disk.read_sector(lba, &mut sector), ReadStatus::Ready);
            }
            fill_window(stream, tag);
            embassy_futures::block_on(Timer::after(Duration::from_millis(30)));
            let reopened = open_head(&mut disk);
            assert_ne!(reopened, first);
            assert!(stream.progress().1 > consumed);
            consumed = stream.progress().1;
            assert_eq!(disk.read_sector(261, &mut sector), ReadStatus::Ready);
            assert_eq!(sector, reopened);
            first = reopened;
        }
    }

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
    fn a_throttled_producer_is_not_a_stalled_one() {
        use iobewi_fat16::FileSource;
        let stream: &'static SharedStream = Box::leak(Box::new(SharedStream::new()));
        let data = [0x55u8; 1024];
        for _ in 0..80 {
            stream.push(&data);
        }
        // A reader that has read nothing: the window fills up to the lead and the producer is held.
        let mut source = SharedStreamSource::new(stream);
        source.begin_session();
        while stream.push(&data) > 0 {}
        assert_eq!(stream.writable(), 0);
        // The producer waits for room for a long time (the host reads ahead of the stream, or not at
        // all): that is not silence of the stream.
        embassy_futures::block_on(embassy_futures::join::join(
            stream.wait_for_room(CONSUMER_IDLE_LIMIT),
            async {
                Timer::after(Duration::from_millis(120)).await;
                assert!(stream.producer_idle() < Duration::from_millis(40));
                assert!(!is_stall(ReadStatus::Pending, stream.producer_idle()));
                // The reader catches up: there is room again.
                let mut sector = [0u8; iobewi_fat16::SECTOR_SIZE];
                let mut index = 0;
                while source.read_file_sector(index, &mut sector) == ReadStatus::Ready {
                    index += 1;
                }
            },
        ));
        assert!(stream.writable() > 0);
    }

    // --- ZEROFILL lines are limited, what is served is not ---------------------------------------

    #[test]
    fn a_series_logs_its_first_sectors_then_one_in_128_and_a_total_when_it_ends() {
        let mut series = ZeroSeries::default();
        let logged: Vec<u32> = (0..300).filter_map(|_| series.sector()).collect();
        assert_eq!(logged, [1, 2, 3, 128, 256], "5 lines for 300 zero sectors");
        // The command ends; the next one has zero sectors again: the series goes on.
        assert_eq!(series.command(), None);
        series.sector();
        assert_eq!(series.command(), None);
        // Two commands without any zero sector: the series is over, with its total.
        assert_eq!(series.command(), Some(301));
        assert_eq!(series.command(), None, "told once");
        // A new series starts over at one.
        assert_eq!(series.sector(), Some(1));
    }

    #[test]
    fn a_short_series_has_no_total_it_was_logged_sector_by_sector() {
        let mut series = ZeroSeries::default();
        assert_eq!((series.sector(), series.sector()), (Some(1), Some(2)));
        assert_eq!(series.command(), None);
        assert_eq!(series.command(), None, "2 sectors, both logged: no summary");
        assert_eq!(series.finish(), None);
    }

    #[test]
    fn the_end_of_the_session_reports_a_series_in_progress_once() {
        let mut series = ZeroSeries::default();
        for _ in 0..96 {
            series.sector();
        }
        assert_eq!(series.finish(), Some(96));
        assert_eq!(series.finish(), None);
    }

    #[test]
    fn limiting_the_lines_changes_nothing_of_what_is_served() {
        let stream: &'static SharedStream = Box::leak(Box::new(SharedStream::new()));
        let stall: &'static Signal<CriticalSectionRawMutex, ()> =
            Box::leak(Box::new(Signal::new()));
        let mut policy = StreamReadPolicy::new(stream, stall);
        // Expired data is zero-filled at once, every time, whatever was logged or not.
        for command in 0..4 {
            policy.command_started();
            for sector in 0..96 {
                assert_eq!(
                    policy.unavailable(ReadStatus::Expired, Duration::from_millis(0)),
                    ReadAction::ZeroFill,
                    "command {command} sector {sector}"
                );
            }
        }
        // A sector that is only late is still waited for within the command's budget.
        policy.command_started();
        assert!(matches!(
            policy.unavailable(ReadStatus::Pending, Duration::from_secs(60)),
            ReadAction::RetryAfter(_)
        ));
        assert_eq!(policy.zeros.total, 4 * 96, "every zero sector is counted");
    }
}
