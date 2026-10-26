use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use embassy_futures::select::{Either3, select3};
use iobewi_config_space::ConfigBackend;
use iobewi_state::{Flags, State};

use crate::brightness::{Brightness, SaveError};
use crate::settings::SettingsStore;

use crate::fetch::{Chain, TcpConnector, is_redirect, origin};
use alloc::string::String;
use edge_http::{BodyType, io::Body};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use iobewi_fat16::{FileSource, ReadStatus, SECTOR_SIZE};
use iobewi_http_client::{HeadError, Target, get, read_head};
use iobewi_net_io::{Close, Connector, ErrorType};
use streambewi_core::{
    PREBUFFER_BYTES, Resume, Stream, StreamFile, is_ready, new_stream, progress,
};

/// Bind the offset a host resumes at to the live edge (see `streambewi_core::Resume`). `false` restores the
/// previous policy exactly.
const RESUME_REBASE: bool = true;

pub const STREAM_URL: &str = "http://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3";

pub static STREAM: SharedStream = SharedStream::new();

/// Largest chunk read from the network and pushed into the window at once.
const CHUNK_BYTES: usize = 2048;

/// Longest the producer waits for the next bytes of the stream while it wants them.
///
/// A network path can die without a FIN or a RST (the access point stays associated, the Internet
/// behind it is gone): the TCP connection is then half-open and a read would wait for ever. The next
/// bytes normally arrive within a fraction of a second (a 2 KiB chunk every ~125 ms at 128 kbit/s);
/// the longest observed gap on a healthy connection is under 2 s. 8 s is several times that and just
/// above the 5 s the reader's window covers. It only counts while the producer is reading: a producer
/// held back by the reader (`wait_for_room`) is not waiting for the network.
const READ_TIMEOUT: Duration = Duration::from_secs(8);
/// Period of the stream progress heartbeat in the operating log.
const PROGRESS_LOG_PERIOD: Duration = Duration::from_secs(60);
/// Shortest time between two `debug` snapshots of the stream while it receives.
const SNAPSHOT_PERIOD: Duration = Duration::from_secs(15);
/// Room for a response head; a longer one is refused (`TooLarge`), not truncated.
const HEAD_BYTES: usize = 2048;
/// Longest the producer waits to open the connection and receive the response headers.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Failures in a row since the last good response (or the last station change), to keep a station that
/// cannot be reached from filling the journal: the first `LOUD_FIRST` failures are logged, then one in
/// `LOUD_EVERY`, each with its number. The first failure and the return to a good response are always logged.
/// A counter and nothing else: no allocation, no timer.
pub(crate) struct Failures(AtomicU32);

const LOUD_FIRST: u32 = 3;
const LOUD_EVERY: u32 = 10;

impl Failures {
    pub(crate) const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// Whether the failure that comes after `before` others is worth a line.
    pub(crate) fn is_logged(before: u32) -> bool {
        before < LOUD_FIRST || before.is_multiple_of(LOUD_EVERY)
    }

    /// The number of the failure being reported if it is worth a line.
    pub(crate) fn line(&self) -> Option<u32> {
        let before = self.0.load(Ordering::Relaxed);
        Self::is_logged(before).then_some(before + 1)
    }

    pub(crate) fn count(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn failed(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// A good response: the failures in a row end. Returns how many there were.
    pub(crate) fn recovered(&self) -> u32 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

/// The product's own count (one station is streamed at a time).
static FAILURES: Failures = Failures::new();

/// A line about a failed attempt, subject to `Failures`.
macro_rules! failure {
    ($failures:expr, $level:ident, $($arg:tt)+) => {
        if let Some(n) = $failures.line() {
            log::$level!("{} (failure {})", format_args!($($arg)+), n);
        }
    };
}

/// Why a bounded read returned no data.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadStop<E> {
    /// The server closed the connection.
    Closed,
    /// Nothing arrived within the limit: the connection is considered dead.
    Silent,
    Failed(E),
}

/// One read of at most `limit`; returns the number of bytes (never 0).
pub(crate) async fn read_bounded<R: embedded_io_async::Read>(
    body: &mut R,
    buf: &mut [u8],
    limit: Duration,
) -> Result<usize, ReadStop<R::Error>> {
    match with_timeout(limit, body.read(buf)).await {
        Err(_) => Err(ReadStop::Silent),
        Ok(Ok(0)) => Err(ReadStop::Closed),
        Ok(Ok(n)) => Ok(n),
        Ok(Err(err)) => Err(ReadStop::Failed(err)),
    }
}

/// The product stream window shared between the network task and the USB MSC task.
pub struct SharedStream {
    inner: Mutex<CriticalSectionRawMutex, RefCell<Stream>>,
    /// The resume policy of the MSC session (#31).
    resume: Mutex<CriticalSectionRawMutex, RefCell<Resume>>,
    reconnects: AtomicU32,
    last_read: Mutex<CriticalSectionRawMutex, Cell<u64>>,
    /// Keep the consumer session across the next reconnection of the host (see `hold_session`).
    hold: AtomicBool,
    /// Incremented by every station change; a USB stall cycle started before it must not act after it.
    generation: AtomicU32,
    /// Ticks of the last accepted push (0: nothing pushed yet); a critical section, since the target
    /// has no 64-bit atomics.
    last_push: Mutex<CriticalSectionRawMutex, Cell<u64>>,
}

impl Default for SharedStream {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedStream {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(RefCell::new(new_stream())),
            resume: Mutex::new(RefCell::new(Resume::new(RESUME_REBASE))),
            reconnects: AtomicU32::new(0),
            last_read: Mutex::new(Cell::new(0)),
            hold: AtomicBool::new(false),
            generation: AtomicU32::new(0),
            last_push: Mutex::new(Cell::new(0)),
        }
    }

    /// Recent file data requests; this cannot prove that the player's speaker is audible.
    pub fn is_reading(&self) -> bool {
        self.last_read.lock(|last| {
            last.get() != 0
                && Instant::now() - Instant::from_ticks(last.get()) < Duration::from_secs(6)
        })
    }
    pub fn is_ready(&self) -> bool {
        self.inner.lock(|cell| is_ready(&cell.borrow()))
    }

    pub fn progress(&self) -> (u64, u64) {
        self.inner.lock(|cell| progress(&cell.borrow()))
    }

    pub(crate) fn writable(&self) -> usize {
        self.inner
            .lock(|cell| cell.borrow().writable().min(CHUNK_BYTES))
    }

    pub(crate) fn push(&self, input: &[u8]) -> usize {
        let pushed = self.inner.lock(|cell| cell.borrow_mut().push(input));
        if pushed > 0 {
            let now = Instant::now().as_ticks().max(1);
            self.last_push.lock(|last| last.set(now));
        }
        pushed
    }

    /// Wait until the window has room for the producer (the reader is behind).
    ///
    /// A producer held back by a slow or absent reader is throttled, not starved by the network: it
    /// counts as active while it waits, so that `producer_idle` measures silence of the stream only.
    /// Otherwise a host that reads ahead of the live edge, or does not read at all, would make the
    /// producer look dead and the device would leave the bus and never come back (a fresh prebuffer
    /// cannot be written into a full window).
    pub(crate) async fn wait_for_room(&self) {
        while self.writable() == 0 {
            let now = Instant::now().as_ticks().max(1);
            self.last_push.lock(|last| last.set(now));
            Timer::after(Duration::from_millis(5)).await;
        }
    }

    /// How long the producer has delivered nothing; `Duration::MAX` before the first byte.
    pub fn producer_idle(&self) -> Duration {
        match self.last_push.lock(|last| last.get()) {
            0 => Duration::MAX,
            ticks => Instant::now() - Instant::from_ticks(ticks),
        }
    }

    /// Keep the current consumer session across the host's next disconnection and reconnection:
    /// the end of the MSC session and the beginning of the next one leave the window untouched.
    ///
    /// A player that was disconnected for a while resumes at its previous file offset; rebasing on
    /// the live edge would put that offset ahead of the stream (it would wait for data that arrives
    /// much later) or serve it expired data. Keeping the session keeps the offsets continuous: the
    /// bytes after the interruption are simply the data that arrived since.
    pub fn hold_session(&self) {
        self.hold.store(true, Ordering::Release);
    }

    /// Time since the last sector served to the host; `None` if none yet.
    pub(crate) fn since_last_read(&self) -> Option<Duration> {
        self.last_read.lock(|last| match last.get() {
            0 => None,
            ticks => Some(Instant::now() - Instant::from_ticks(ticks)),
        })
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    /// Start a new station: the old consumer session ends, nothing is held across it, and the producer
    /// counts as not having delivered yet. The window itself needs no clearing: positions are absolute
    /// and the caller waits for `PREBUFFER_BYTES` written past the returned position, so the next
    /// session (`begin_session`: 64 KiB behind the live edge) starts inside the new station's data.
    /// Returns the live position at the change.
    pub(crate) fn begin_station_change(&self) -> u64 {
        self.hold.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.inner.lock(|cell| {
            cell.borrow_mut().end_session();
        });
        self.last_push.lock(|last| last.set(0));
        self.progress().0
    }

    fn mark_reconnect(&self) -> u32 {
        self.reconnects.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// `FileSource` handed to the virtual FAT16 volume: locks the shared window per read.
#[derive(Clone, Copy)]
pub struct SharedStreamSource {
    stream: &'static SharedStream,
}

impl SharedStreamSource {
    pub const fn new(stream: &'static SharedStream) -> Self {
        Self { stream }
    }

    fn with_file<R>(&self, f: impl FnOnce(&mut StreamFile<'_>) -> R) -> R {
        self.stream.inner.lock(|cell| {
            self.stream.resume.lock(|resume| {
                f(&mut StreamFile::with_resume(
                    &mut cell.borrow_mut(),
                    &mut resume.borrow_mut(),
                ))
            })
        })
    }
}

impl FileSource for SharedStreamSource {
    fn begin_session(&mut self) {
        if self.stream.hold.swap(false, Ordering::AcqRel) {
            log::info!("stream: session kept across the reconnection");
            return;
        }
        self.with_file(|file| file.begin_session());
        let (live, _) = self.stream.progress();
        log::info!("stream: session start live={}", live);
    }

    fn end_session(&mut self) {
        if self.stream.hold.load(Ordering::Acquire) {
            return;
        }
        self.with_file(|file| file.end_session());
        let (live, consumed) = self.stream.progress();
        log::info!("stream: session end consumed={} live={}", consumed, live);
    }

    fn read_file_sector(&mut self, index: u32, out: &mut [u8; SECTOR_SIZE]) -> ReadStatus {
        let before = self
            .stream
            .inner
            .lock(|c| c.borrow().session().map(|s| s.base));
        let status = self.with_file(|file| file.read_file_sector(index, out));
        if status == ReadStatus::Ready {
            self.stream
                .last_read
                .lock(|last| last.set(Instant::now().as_ticks().max(1)));
        }
        {
            let after = self
                .stream
                .inner
                .lock(|c| c.borrow().session().map(|s| s.base));
            if before != after {
                let (live, _) = self.stream.progress();
                log::info!(
                    "stream: REBASE on read of sector {} base {:?} -> {:?} live={}",
                    index,
                    before,
                    after,
                    live
                );
            }
        }
        status
    }
}

/// Minimum time the disk stays off the bus on a station change: the host must register the removal.
const SWITCH_DETACH: Duration = Duration::from_millis(1_500);
/// The MSC is quiet once no sector has been served for this long (or after `QUIET_WAIT`).
const QUIET_AFTER: Duration = Duration::from_millis(300);
const QUIET_WAIT: Duration = Duration::from_secs(2);

/// Runs the producer and applies station changes without a restart.
///
/// On a change (`setup::SWITCH`, raised once the new selection is held in RAM) the disk leaves the bus,
/// the old connection is dropped, the selection is written to flash (`persist`) while nobody is on the
/// bus to be starved by the flash critical sections, and the disk comes back once the write is done,
/// `SWITCH_DETACH` has elapsed and a fresh prebuffer of the new station is in the window. A failed write
/// is logged and leaves the selection active but flagged unsaved (`Radio::is_unsaved`).
pub async fn run<T: Connector>(
    stack: Stack<'static>,
    stream: &'static SharedStream,
    tls: &T,
    link: Option<&'static dyn crate::usb::UsbLink>,
    active_url: impl AsyncFn() -> alloc::string::String,
    persist: impl AsyncFn() -> Result<bool, ()>,
) -> !
where
    for<'a> <T::Connection<'a> as ErrorType>::Error: core::fmt::Display,
{
    let tcp = TcpConnector { stack };

    stack.wait_config_up().await;
    if let Some(config) = stack.config_v4() {
        log::info!("net: got IP {}", config.address);
    }

    // `None` until the first station change; then the instant the disk left the bus and the live
    // position it must pass by one prebuffer before the disk comes back.
    let mut pending_attach: Option<(Instant, u64)> = None;
    loop {
        let url = active_url().await;
        let attach = async {
            if let (Some(link), Some((since, live))) = (link, pending_attach) {
                while stream.progress().0 < live + PREBUFFER_BYTES {
                    Timer::after(Duration::from_millis(100)).await;
                }
                Timer::at(since + SWITCH_DETACH).await;
                crate::usb::clear_stall();
                link.set_connected(true);
                crate::status::usb_ready();
                crate::setup::RESTARTING.store(0, Ordering::Relaxed);
                log::info!(
                    "station: disk back after {} ms (live={})",
                    since.elapsed().as_millis(),
                    stream.progress().0
                );
            }
            core::future::pending::<()>().await
        };
        let switched = select3(
            produce(&tcp, tls, stream, &url),
            attach,
            crate::setup::SWITCH.wait(),
        )
        .await;
        match switched {
            Either3::Third(()) => crate::status::stream_busy(),
            Either3::First(never) => match never {},
            Either3::Second(()) => unreachable!(),
        }
        // The old producer is dropped (its socket is released). Leave the bus first, then restart.
        let since = Instant::now();
        if let Some(link) = link {
            link.set_connected(false);
            crate::status::usb_off_bus();
        }
        let live = stream.begin_station_change();
        log::info!("station: switching, disk off the bus (live={})", live);
        // No sector served recently: the host is no longer reading. A soft disconnect (DCTL.SDIS) raises no
        // device event, so the MSC session itself only ends at the host's next bus reset.
        let waited = Instant::now();
        while stream
            .since_last_read()
            .is_some_and(|idle| idle < QUIET_AFTER)
            && waited.elapsed() < QUIET_WAIT
        {
            Timer::after(Duration::from_millis(50)).await;
        }
        let started = Instant::now();
        match persist().await {
            Ok(wrote) => {
                crate::status::stream_selection_unsaved(false);
                log::info!(
                    "station: selection persisted={} in {} ms",
                    wrote,
                    started.elapsed().as_millis()
                )
            }
            Err(()) => {
                crate::status::stream_selection_unsaved(true);
                log::error!(
                    "station: persisting the selection FAILED after {} ms: active in RAM, NOT saved",
                    started.elapsed().as_millis()
                )
            }
        }
        pending_attach = Some((since, live));
    }
}

/// Waits while another actor (a station change, a stall recovery) has the disk off the bus, then says
/// what the disk is doing: `Ready` (on the bus), or something else (not presented yet, or dead).
///
/// The caller must act on the answer without an `.await` in between. Without a registry to ask (a build
/// that never called `status::init`), the disk is taken for presented: the protected path is the safe one.
async fn settled_disk(watch: &mut Option<crate::status::Disk<'_>>) -> Flags {
    let Some(watch) = watch else {
        return Flags::of(&[State::Ready]);
    };
    let usb = watch.sources.ids().usb;
    let mut snapshot = watch.observer.get();
    loop {
        let disk = snapshot.of(usb).unwrap_or(Flags::EMPTY);
        if !disk.contains(State::Unavailable) {
            return disk;
        }
        snapshot = watch.observer.changed().await;
    }
}

/// Writes the LED brightness. With the disk on the bus this is the detach window of a station change,
/// without its change of station: the disk leaves the bus, nobody has read for `QUIET_AFTER` (or
/// `QUIET_WAIT` passed), the settings are written, the disk stays off for `SWITCH_DETACH` since it left,
/// and it comes back. The stream connection is not touched, and the session is held across the
/// reconnection as `LinkSupervisor` does, so the player resumes where it was. A station change that took
/// over meanwhile brings the disk back itself (as for a stall recovery). Without a disk, or with the disk
/// not presented, it is written at once.
async fn save_once<B: ConfigBackend>(
    settings: &SettingsStore<B>,
    percent: u8,
    stream: &'static SharedStream,
    link: Option<&'static dyn crate::usb::UsbLink>,
    watch: &mut Option<crate::status::Disk<'_>>,
) -> Result<(), SaveError> {
    let Some(link) = link else {
        log::debug!("settings: no disk, written at once");
        return settings.save_led_percent(percent).await;
    };
    if !settled_disk(watch).await.contains(State::Ready) {
        log::debug!("settings: the disk is not on the bus, written at once");
        return settings.save_led_percent(percent).await;
    }
    // From here to the detach: no `.await`.
    let generation = stream.generation();
    stream.hold_session();
    link.set_connected(false);
    if let Some(watch) = watch {
        watch.sources.usb_off_bus();
    }
    // A stall raised just before cannot start a recovery of its own that would bring the disk back
    // while the settings are being written.
    crate::usb::clear_stall();
    let since = Instant::now();
    log::debug!("settings: the disk leaves the bus to save");
    let waited = Instant::now();
    while stream
        .since_last_read()
        .is_some_and(|idle| idle < QUIET_AFTER)
        && waited.elapsed() < QUIET_WAIT
    {
        Timer::after(Duration::from_millis(50)).await;
    }
    log::debug!(
        "settings: host quiet after {} ms, writing",
        since.elapsed().as_millis()
    );
    let outcome = settings.save_led_percent(percent).await;
    log::debug!(
        "settings: write {} after {} ms",
        if outcome.is_ok() { "done" } else { "FAILED" },
        since.elapsed().as_millis()
    );
    Timer::at(since + SWITCH_DETACH).await;
    if stream.generation() == generation {
        crate::usb::clear_stall();
        link.set_connected(true);
        if let Some(watch) = watch {
            watch.sources.usb_ready();
        }
        log::debug!(
            "settings: the disk is back after {} ms",
            since.elapsed().as_millis()
        );
    } else {
        log::info!("settings: a station change took the disk over during the save");
    }
    outcome
}

/// Does the saves that the settings page asks for, one at a time, in the order asked, and reports each
/// outcome to `brightness`. A save asked for while another is being written waits for it.
pub async fn save_settings<B: ConfigBackend>(
    settings: &SettingsStore<B>,
    brightness: &Brightness,
    stream: &'static SharedStream,
    link: Option<&'static dyn crate::usb::UsbLink>,
) -> ! {
    let mut watch = crate::status::disk();
    loop {
        let percent = brightness.next_save().await;
        let outcome = save_once(settings, percent, stream, link, &mut watch).await;
        match outcome {
            Ok(()) => log::info!("settings: LED brightness {}% saved", percent),
            Err(_) => {
                log::error!("settings: saving the LED brightness FAILED: the saved value is kept")
            }
        }
        brightness.finish_save(percent, outcome);
    }
}

/// What one request to one target came to.
enum Exchange {
    /// A redirect status with its `Location`, if the server sent a usable one.
    Redirect {
        status: u16,
        location: Option<String>,
    },
    /// A 200 whose body was pumped until it ended: reconnect.
    Streamed,
    /// Nothing to stream: try again after this delay.
    Retry(Duration),
}

/// Connect to `target` with `connector`, `GET` it and, on a 200, move the body into the window.
///
/// The same code serves `http` and `https`: only the connector differs, and an `https` connector
/// verifies the certificate of `target.host` (or refuses). Failures are logged by kind: connect (DNS,
/// TCP or TLS, as the connector says), request, response head, HTTP status.
async fn exchange<K>(
    connector: &K,
    target: &Target,
    stream: &'static SharedStream,
    failures: &Failures,
) -> Exchange
where
    K: Connector,
    for<'a> <K::Connection<'a> as ErrorType>::Error: core::fmt::Display,
{
    let scheme = if target.tls { "tls" } else { "tcp" };
    let mut rx = alloc::vec![0u8; 4096];
    let mut tx = alloc::vec![0u8; 2048];
    let mut head_buf = alloc::vec![0u8; HEAD_BYTES];

    let started = Instant::now();
    let host = target.host.as_str();
    let connected = with_timeout(
        CONNECT_TIMEOUT,
        connector.connect(host, target.port, &mut rx, &mut tx),
    )
    .await;
    let mut conn = match connected {
        Ok(Ok(conn)) => conn,
        Ok(Err(err)) => {
            failure!(
                failures,
                info,
                "stream: {} connect to {} failed: {}",
                scheme,
                origin(target),
                err
            );
            return Exchange::Retry(Duration::from_secs(2));
        }
        Err(_) => {
            failure!(
                failures,
                info,
                "stream: no {} connection to {} after {} s, retrying",
                scheme,
                origin(target),
                CONNECT_TIMEOUT.as_secs()
            );
            return Exchange::Retry(Duration::from_secs(2));
        }
    };
    log::debug!(
        "stream: {} up in {} ms",
        scheme,
        started.elapsed().as_millis()
    );

    if let Err(err) = get(
        &mut conn,
        &target.authority(),
        &target.path,
        &[("Icy-MetaData", "0"), ("User-Agent", "streambewi/0.1")],
    )
    .await
    {
        failure!(failures, info, "stream: request error {}", err);
        let _ = conn.close().await;
        return Exchange::Retry(Duration::from_secs(2));
    }
    let head = match with_timeout(CONNECT_TIMEOUT, read_head(&mut conn, &mut head_buf)).await {
        Ok(Ok(head)) => head,
        Ok(Err(err)) => {
            match err {
                HeadError::Io(_) => {
                    failure!(failures, info, "stream: response head read failed")
                }
                HeadError::Closed => {
                    failure!(failures, info, "stream: closed before the response head")
                }
                HeadError::TooLarge => failure!(
                    failures,
                    warn,
                    "stream: response head over {} bytes, refused",
                    HEAD_BYTES
                ),
                HeadError::TooManyHeaders => failure!(
                    failures,
                    warn,
                    "stream: response head with more than {} headers, refused",
                    iobewi_http_client::MAX_HEADERS
                ),
                HeadError::Malformed => {
                    failure!(failures, warn, "stream: response head refused (malformed)")
                }
            }
            let _ = conn.close().await;
            return Exchange::Retry(Duration::from_secs(2));
        }
        Err(_) => {
            failure!(
                failures,
                info,
                "stream: no response head after {} s",
                CONNECT_TIMEOUT.as_secs()
            );
            let _ = conn.close().await;
            return Exchange::Retry(Duration::from_secs(2));
        }
    };
    if head.status == 200 {
        log::info!(
            "stream: HTTP status={} content_length={:?} chunked={}",
            head.status,
            head.content_length,
            head.chunked
        );
    } else {
        // A station that keeps answering 404 would repeat this line for ever.
        failure!(
            failures,
            info,
            "stream: HTTP status={} content_length={:?} chunked={}",
            head.status,
            head.content_length,
            head.chunked
        );
    }
    if is_redirect(head.status) {
        let _ = conn.close().await;
        return Exchange::Redirect {
            status: head.status,
            location: head.location,
        };
    }
    if head.status != 200 {
        let _ = conn.close().await;
        return Exchange::Retry(Duration::from_secs(5));
    }
    crate::status::stream_connected();
    let failed = failures.recovered();
    if failed >= LOUD_FIRST {
        log::info!("stream: back after {} failed attempts", failed);
    }

    // The body as the head describes it: chunked (what Radio France sends over TLS), with a length,
    // or up to the close. `edge-http` decodes it, so the window only ever gets audio bytes, never
    // chunk delimiters; the bytes that came with the head are the start of the body.
    let body_type = if head.chunked {
        BodyType::Chunked
    } else if let Some(len) = head.content_length {
        BodyType::ContentLen(len as u64)
    } else {
        BodyType::Raw
    };
    let read_len = head.filled - head.body_start;
    head_buf.copy_within(head.body_start..head.filled, 0);
    {
        let mut body = Body::new(body_type, &mut head_buf[..], read_len, &mut conn);
        pump(stream, &mut body).await;
    }
    #[cfg(feature = "qual")]
    if QUAL_ABRUPT.swap(false, Ordering::Relaxed) {
        log::info!("qual-drop: session dropped without a close");
        return Exchange::Streamed;
    }
    let closing = Instant::now();
    let closed = conn.close().await;
    log::info!(
        "stream: {} session closed ({}) in {} ms",
        scheme,
        if closed.is_ok() { "clean" } else { "error" },
        closing.elapsed().as_millis()
    );
    Exchange::Streamed
}

/// One attempt at a station: its URL, then the redirects it leads through, each reached with a new
/// connection by the connector its scheme asks for. Returns how long to wait before the next attempt.
async fn attempt<P: Connector, T: Connector>(
    tcp: &P,
    tls: &T,
    stream: &'static SharedStream,
    url: &str,
) -> Duration
where
    for<'a> <P::Connection<'a> as ErrorType>::Error: core::fmt::Display,
    for<'a> <T::Connection<'a> as ErrorType>::Error: core::fmt::Display,
{
    attempt_counting(tcp, tls, stream, url, &FAILURES).await
}

/// `attempt`, with the count of failures in a row that limits what is logged.
async fn attempt_counting<P: Connector, T: Connector>(
    tcp: &P,
    tls: &T,
    stream: &'static SharedStream,
    url: &str,
    failures: &Failures,
) -> Duration
where
    for<'a> <P::Connection<'a> as ErrorType>::Error: core::fmt::Display,
    for<'a> <T::Connection<'a> as ErrorType>::Error: core::fmt::Display,
{
    let Ok(mut target) = Target::parse(url) else {
        // The address is not logged: it may carry a token.
        failure!(failures, warn, "stream: unusable station address");
        failures.failed();
        return Duration::from_secs(5);
    };
    let mut chain = Chain::new(&target);
    loop {
        if failures.line().is_some() {
            log::debug!("stream: request to {}", origin(&target));
        }
        // An `https` target is never fetched in clear: the TLS connector or nothing.
        let outcome = if target.tls {
            exchange(tls, &target, stream, failures).await
        } else {
            exchange(tcp, &target, stream, failures).await
        };
        match outcome {
            Exchange::Streamed => return Duration::from_secs(1),
            Exchange::Retry(delay) => {
                failures.failed();
                return delay;
            }
            Exchange::Redirect { status, location } => {
                match chain.follow(&target, location.as_deref()) {
                    Ok(next) => {
                        log::info!("stream: redirect {} to {}", status, origin(&next));
                        target = next;
                    }
                    Err(refusal) => {
                        failure!(
                            failures,
                            warn,
                            "stream: redirect {} not followed: {}",
                            status,
                            refusal
                        );
                        failures.failed();
                        return Duration::from_secs(5);
                    }
                }
            }
        }
    }
}

async fn produce<P: Connector, T: Connector>(
    tcp: &P,
    tls: &T,
    stream: &'static SharedStream,
    url: &str,
) -> core::convert::Infallible
where
    for<'a> <P::Connection<'a> as ErrorType>::Error: core::fmt::Display,
    for<'a> <T::Connection<'a> as ErrorType>::Error: core::fmt::Display,
{
    let mut first = true;
    // A new station starts with a clean count.
    FAILURES.recovered();
    loop {
        let reconnect = stream.mark_reconnect();
        // Never the address itself (it may carry a token), only its scheme and host; and not every time
        // when the station cannot be reached.
        if FAILURES.line().is_some() {
            match Target::parse(url) {
                Ok(target) => log::info!(
                    "stream: connecting attempt={} to {}",
                    reconnect,
                    origin(&target)
                ),
                Err(_) => log::info!("stream: connecting attempt={}", reconnect),
            }
        }
        if first {
            crate::status::stream_connecting();
        } else {
            crate::status::stream_reconnecting();
        }
        first = false;
        let delay = attempt(tcp, tls, stream, url).await;
        // The connection is over: do not keep saying it is up (or ready) while waiting to retry.
        crate::status::stream_reconnecting();
        Timer::after(delay).await;
    }
}

/// Bench qualification only: whether the connection that just ended must be dropped without a close.
#[cfg(feature = "qual")]
static QUAL_ABRUPT: AtomicBool = AtomicBool::new(false);

/// Bench qualification only: end the connection on a schedule of lifetimes (`QUAL_DROP_LIVES`, seconds, one
/// per successive connection, cycling), alternately closed cleanly and dropped without a close. The schedule is
/// the build's: no fixed period, and it is a way to exercise the reconnection, not a network outage.
#[cfg(feature = "qual")]
fn qual_drop_due(stream: &SharedStream, started: Instant) -> bool {
    let Some(list) = option_env!("QUAL_DROP_LIVES") else {
        return false;
    };
    let n = stream.reconnects.load(Ordering::Relaxed).max(1) as usize - 1;
    let Some(secs) = list
        .split(',')
        .filter_map(|s| s.parse::<u64>().ok())
        .cycle()
        .nth(n)
    else {
        return false;
    };
    if started.elapsed() < Duration::from_secs(secs) {
        return false;
    }
    let abrupt = n % 2 == 1;
    QUAL_ABRUPT.store(abrupt, Ordering::Relaxed);
    let (live, consumed) = stream.progress();
    log::info!(
        "qual-drop: ending connection #{} after {} s, {} live={} consumed={} lead={}",
        n + 1,
        secs,
        if abrupt {
            "ABRUPT (no close)"
        } else {
            "graceful close"
        },
        live,
        consumed,
        live.saturating_sub(consumed)
    );
    true
}

/// Paces the `debug` snapshot: due once `SNAPSHOT_PERIOD` has passed since the last one. A timestamp and
/// nothing else.
struct Snapshots(Instant);

impl Snapshots {
    fn due(&mut self, now: Instant) -> bool {
        let due = now.saturating_duration_since(self.0) >= SNAPSHOT_PERIOD;
        if due {
            self.0 = now;
        }
        due
    }
}

/// Move the body of a response into the window until the server closes, the path goes silent or
/// the read fails.
async fn pump<R: embedded_io_async::Read>(stream: &'static SharedStream, body: &mut R)
where
    R::Error: core::fmt::Debug,
{
    let mut buf = [0u8; CHUNK_BYTES];
    let mut last_progress_log = Instant::now();
    let mut snapshots = Snapshots(Instant::now());
    // Bytes this connection delivered: the stream is ready for the status LED once it passed the
    // prebuffer. A local counter, one comparison per chunk; the registry is touched once.
    let mut delivered = 0u64;
    let mut ready = false;
    #[cfg(feature = "qual")]
    let started = Instant::now();

    loop {
        #[cfg(feature = "qual")]
        if qual_drop_due(stream, started) {
            break;
        }
        stream.wait_for_room().await;

        let want = core::cmp::min(stream.writable(), buf.len());
        if want == 0 {
            continue;
        }

        match read_bounded(body, &mut buf[..want], READ_TIMEOUT).await {
            Err(ReadStop::Closed) => {
                log::info!("stream: server closed connection");
                break;
            }
            Err(ReadStop::Silent) => {
                // Half-open connection: drop it (the socket is removed and the pool slot freed)
                // and let the reconnection open a new one.
                log::warn!(
                    "stream: no data for {} s, abandoning the connection",
                    READ_TIMEOUT.as_secs()
                );
                break;
            }
            Ok(n) => {
                let before = stream.progress().0;
                let pushed = stream.push(&buf[..n]);
                let (written, consumed) = stream.progress();
                if !ready {
                    delivered += pushed as u64;
                    if delivered >= PREBUFFER_BYTES {
                        ready = true;
                        crate::status::stream_ready();
                    }
                }

                // Prebuffer reached, then a heartbeat every 60 s while data flows.
                if before < PREBUFFER_BYTES && written >= PREBUFFER_BYTES
                    || last_progress_log.elapsed() >= PROGRESS_LOG_PERIOD
                {
                    last_progress_log = Instant::now();
                    log::info!(
                        "stream: live={} consumed={} lead={}",
                        written,
                        consumed,
                        written.saturating_sub(consumed)
                    );
                }

                // Detailed diagnostic only: the level is checked before anything is read or formatted.
                if snapshots.due(Instant::now()) && log::log_enabled!(log::Level::Debug) {
                    log::debug!(
                        "stream: snapshot live={} consumed={} lead={} failures_in_a_row={}",
                        written,
                        consumed,
                        written.saturating_sub(consumed),
                        FAILURES.count()
                    );
                }

                if pushed == 0 {
                    Timer::after(Duration::from_millis(5)).await;
                }
            }
            Err(ReadStop::Failed(err)) => {
                log::info!("stream: body read error {:?}", err);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;
    use embedded_io_async::{ErrorType, Read};

    /// A body that behaves as scripted: never answers, answers after a delay, or reports the end.
    enum Body {
        Silent,
        After(Duration, usize),
        Closed,
    }
    impl ErrorType for Body {
        type Error = Infallible;
    }
    impl Read for Body {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Infallible> {
            match self {
                Body::Silent => core::future::pending().await,
                Body::After(delay, n) => {
                    Timer::after(*delay).await;
                    buf[..*n].fill(0xA5);
                    Ok(*n)
                }
                Body::Closed => Ok(0),
            }
        }
    }

    fn run<T>(f: impl core::future::Future<Output = T>) -> (T, std::time::Duration) {
        let start = std::time::Instant::now();
        let out = embassy_futures::block_on(f);
        (out, start.elapsed())
    }

    #[test]
    fn a_connection_that_goes_silent_is_abandoned_after_the_limit() {
        let mut buf = [0u8; 64];
        let (out, took) = run(read_bounded(
            &mut Body::Silent,
            &mut buf,
            Duration::from_millis(80),
        ));
        assert_eq!(out, Err(ReadStop::Silent));
        assert!(took >= std::time::Duration::from_millis(70), "{took:?}");
        assert!(took < std::time::Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn a_slow_but_live_connection_is_not_abandoned() {
        // Jitter shorter than the limit: the data is delivered.
        let mut buf = [0u8; 64];
        let (out, _) = run(read_bounded(
            &mut Body::After(Duration::from_millis(30), 17),
            &mut buf,
            Duration::from_millis(400),
        ));
        assert_eq!(out, Ok(17));
        assert!(buf[..17].iter().all(|b| *b == 0xA5));
    }

    #[test]
    fn the_end_of_the_stream_is_not_a_silence() {
        let mut buf = [0u8; 8];
        let (out, _) = run(read_bounded(
            &mut Body::Closed,
            &mut buf,
            Duration::from_millis(50),
        ));
        assert_eq!(out, Err(ReadStop::Closed));
    }

    #[test]
    fn a_station_change_ends_the_session_and_drops_the_hold() {
        let stream: &'static SharedStream = Box::leak(Box::new(SharedStream::new()));
        let data = [0x55u8; 1024];
        for _ in 0..80 {
            stream.push(&data);
        }
        let mut source = SharedStreamSource::new(stream);
        source.begin_session();
        stream.hold_session();
        let before = stream.generation();
        let live = stream.begin_station_change();
        assert_eq!(live, 80 * 1024);
        assert_eq!(stream.generation(), before + 1);
        assert!(stream.inner.lock(|c| c.borrow().session().is_none()));
        // The next enumeration starts a fresh session instead of keeping the old one.
        source.begin_session();
        assert!(stream.inner.lock(|c| c.borrow().session().is_some()));
        assert_eq!(stream.producer_idle(), Duration::MAX);
    }

    #[test]
    fn waiting_for_room_is_not_counted_against_the_network() {
        // As in the producer loop: wait for room first, then read with the limit. The reader is behind for
        // much longer than the limit; the read that follows is still given its full limit and succeeds.
        let stream: &'static SharedStream = Box::leak(Box::new(SharedStream::new()));
        let data = [0x55u8; 1024];
        let mut source = SharedStreamSource::new(stream);
        for _ in 0..80 {
            stream.push(&data);
        }
        source.begin_session();
        while stream.push(&data) > 0 {}
        assert_eq!(stream.writable(), 0);
        let limit = Duration::from_millis(60);
        let (out, took) = run(async {
            let freeing = async {
                // The reader catches up only after twice the limit.
                Timer::after(Duration::from_millis(120)).await;
                let mut sector = [0u8; SECTOR_SIZE];
                let mut index = 0;
                while source.read_file_sector(index, &mut sector) == ReadStatus::Ready {
                    index += 1;
                }
            };
            let producing = async {
                stream.wait_for_room().await;
                let mut buf = [0u8; 64];
                read_bounded(
                    &mut Body::After(Duration::from_millis(20), 5),
                    &mut buf,
                    limit,
                )
                .await
            };
            embassy_futures::join::join(freeing, producing).await.1
        });
        assert_eq!(out, Ok(5));
        assert!(took >= std::time::Duration::from_millis(110), "{took:?}");
    }

    /// What a client of the window sees: sectors from the start of the session, until one is not ready.
    fn window_bytes(stream: &'static SharedStream) -> alloc::vec::Vec<u8> {
        let mut source = SharedStreamSource::new(stream);
        source.begin_session();
        let mut out = alloc::vec::Vec::new();
        let mut sector = [0u8; SECTOR_SIZE];
        let mut index = 0;
        while source.read_file_sector(index, &mut sector) == ReadStatus::Ready {
            out.extend_from_slice(&sector);
            index += 1;
        }
        out
    }

    fn payload(len: usize) -> alloc::vec::Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    /// `data` as an HTTP chunked body, in chunks of the given sizes, then the last chunk.
    fn chunked(data: &[u8], sizes: &[usize]) -> alloc::vec::Vec<u8> {
        let mut wire = alloc::vec::Vec::new();
        let mut rest = data;
        for size in sizes {
            let (head, tail) = rest.split_at(*size);
            wire.extend_from_slice(alloc::format!("{:x}\r\n", head.len()).as_bytes());
            wire.extend_from_slice(head);
            wire.extend_from_slice(b"\r\n");
            rest = tail;
        }
        assert!(rest.is_empty());
        wire.extend_from_slice(b"0\r\n\r\n");
        wire
    }

    /// A connection that serves a scripted response in small reads and records the request.
    struct MockConn<'a> {
        script: &'a Script,
        data: alloc::vec::Vec<u8>,
        pos: usize,
        request: alloc::vec::Vec<u8>,
    }
    impl ErrorType for MockConn<'_> {
        type Error = Infallible;
    }
    impl Read for MockConn<'_> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Infallible> {
            let n = (self.data.len() - self.pos).min(buf.len()).min(13);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
    impl embedded_io_async::Write for MockConn<'_> {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Infallible> {
            self.request.extend_from_slice(buf);
            Ok(buf.len())
        }
        async fn flush(&mut self) -> Result<(), Infallible> {
            let request = String::from_utf8(core::mem::take(&mut self.request)).unwrap();
            self.script.requests.borrow_mut().push(request);
            Ok(())
        }
    }
    impl Close for MockConn<'_> {
        async fn close(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Refused;
    impl core::fmt::Display for Refused {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("refused")
        }
    }

    /// A connector that answers its successive connections with the scripted responses, or refuses.
    #[derive(Default)]
    struct Script {
        responses: RefCell<alloc::vec::Vec<alloc::vec::Vec<u8>>>,
        connects: RefCell<alloc::vec::Vec<(String, u16)>>,
        requests: RefCell<alloc::vec::Vec<String>>,
        refuse: bool,
    }
    impl Script {
        fn serving(responses: &[&[u8]]) -> Self {
            Self {
                responses: RefCell::new(responses.iter().map(|r| r.to_vec()).collect()),
                ..Self::default()
            }
        }
        fn refusing() -> Self {
            Self {
                refuse: true,
                ..Self::default()
            }
        }
        fn connections(&self) -> usize {
            self.connects.borrow().len()
        }
    }
    impl Connector for Script {
        type Error = Refused;
        type Connection<'a> = MockConn<'a>;
        async fn connect<'a>(
            &'a self,
            host: &'a str,
            port: u16,
            _rx: &'a mut [u8],
            _tx: &'a mut [u8],
        ) -> Result<MockConn<'a>, Refused> {
            self.connects.borrow_mut().push((host.into(), port));
            if self.refuse || self.responses.borrow().is_empty() {
                return Err(Refused);
            }
            Ok(MockConn {
                script: self,
                data: self.responses.borrow_mut().remove(0),
                pos: 0,
                request: alloc::vec::Vec::new(),
            })
        }
    }

    fn fresh() -> &'static SharedStream {
        Box::leak(Box::new(SharedStream::new()))
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn ok_chunked(audio: &[u8]) -> alloc::vec::Vec<u8> {
        let mut r = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        r.extend(chunked(audio, &[1000, 7, 1, audio.len() - 1008]));
        r
    }

    #[test]
    fn a_chunked_body_gives_the_window_audio_bytes_and_no_framing() {
        let audio = payload(4000);
        let wire = chunked(&audio, &[1000, 7, 1, 2992]);
        // The head came with the first bytes of the body; the rest is still on the connection.
        let early = 11;
        let mut buf = [0u8; 64];
        buf[..early].copy_from_slice(&wire[..early]);
        let mut rest = MockConn {
            script: &Script::default(),
            data: wire[early..].to_vec(),
            pos: 0,
            request: alloc::vec::Vec::new(),
        };
        let stream = fresh();
        let mut body = edge_http::io::Body::new(BodyType::Chunked, &mut buf[..], early, &mut rest);
        run(pump(stream, &mut body));
        let got = window_bytes(stream);
        assert_eq!(got.len(), 4000 / SECTOR_SIZE * SECTOR_SIZE);
        assert_eq!(got[..], audio[..got.len()]);
        assert_eq!(stream.progress().0, 4000);
    }

    #[test]
    fn an_http_station_streams_with_a_content_length() {
        let audio = payload(2048);
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 2048\r\n\r\n".to_vec();
        response.extend(&audio);
        let (tcp, tls) = (Script::serving(&[&response]), Script::refusing());
        let stream = fresh();
        let (delay, _) = run(attempt(
            &tcp,
            &tls,
            stream,
            "http://radio.example:8000/live.mp3",
        ));
        assert_eq!(delay, secs(1));
        assert_eq!(window_bytes(stream)[..], audio[..]);
        assert_eq!(tcp.connects.borrow()[0], ("radio.example".into(), 8000));
        let request = tcp.requests.borrow()[0].clone();
        assert!(
            request.starts_with("GET /live.mp3 HTTP/1.1\r\nHost: radio.example:8000\r\n"),
            "{request}"
        );
        assert_eq!(tls.connections(), 0);
    }

    /// A response head with `n` filler headers, then chunked audio.
    fn many_headers(n: usize, audio: &[u8]) -> alloc::vec::Vec<u8> {
        let mut r = b"HTTP/1.1 200 OK\r\n".to_vec();
        for i in 0..n {
            r.extend(alloc::format!("icy-x{i}: value\r\n").into_bytes());
        }
        r.extend(b"Transfer-Encoding: chunked\r\n\r\n");
        r.extend(chunked(audio, &[1000, 7, 1, audio.len() - 1008]));
        r
    }

    #[test]
    fn a_response_with_26_headers_streams_and_more_than_48_is_refused() {
        // The shape of a real Icecast over HTTPS on :8443 (26 headers), which a
        // 24-header parser refused as malformed.
        let audio = payload(3000);
        let tls = Script::serving(&[&many_headers(25, &audio)]);
        let stream = fresh();
        let (delay, _) = run(attempt(
            &Script::refusing(),
            &tls,
            stream,
            "https://a.example:8443/j",
        ));
        assert_eq!(delay, secs(1));
        assert_eq!(stream.progress().0, 3000);
        let got = window_bytes(stream);
        assert_eq!(got[..], audio[..got.len()]);
        // 48 headers in all (47 + Transfer-Encoding) still stream.
        let tls = Script::serving(&[&many_headers(47, &audio)]);
        let stream = fresh();
        run(attempt(
            &Script::refusing(),
            &tls,
            stream,
            "https://a.example:8443/j",
        ));
        assert_eq!(stream.progress().0, 3000);
        let got = window_bytes(stream);
        assert_eq!(got[..], audio[..got.len()]);
        // 49: refused explicitly, no audio, retried.
        let tls = Script::serving(&[&many_headers(48, &audio)]);
        let stream = fresh();
        let (delay, _) = run(attempt(
            &Script::refusing(),
            &tls,
            stream,
            "https://a.example:8443/j",
        ));
        assert_eq!(delay, secs(2));
        assert_eq!(stream.progress().0, 0);
    }

    #[test]
    fn an_https_station_goes_through_the_tls_connector_only() {
        let audio = payload(3000);
        let (tcp, tls) = (Script::refusing(), Script::serving(&[&ok_chunked(&audio)]));
        let stream = fresh();
        let (delay, _) = run(attempt(
            &tcp,
            &tls,
            stream,
            "https://radio.example/live#frag",
        ));
        assert_eq!(delay, secs(1));
        assert_eq!(tls.connects.borrow()[0], ("radio.example".into(), 443));
        assert!(
            tls.requests.borrow()[0].starts_with("GET /live HTTP/1.1\r\nHost: radio.example\r\n")
        );
        let got = window_bytes(stream);
        assert_eq!(got[..], audio[..got.len()]);
        assert_eq!(tcp.connections(), 0);
    }

    #[test]
    fn a_tls_failure_never_falls_back_to_clear_text() {
        let (tcp, tls) = (
            Script::serving(&[b"HTTP/1.1 200 OK\r\n\r\nxx"]),
            Script::refusing(),
        );
        let stream = fresh();
        let (delay, _) = run(attempt(&tcp, &tls, stream, "https://radio.example/live"));
        assert_eq!(delay, secs(2));
        assert_eq!(tls.connections(), 1);
        assert_eq!(tcp.connections(), 0);
        assert_eq!(stream.progress().0, 0);
    }

    #[test]
    fn a_redirect_to_another_https_host_is_a_new_tls_connection_to_that_host() {
        let audio = payload(1024);
        let tls = Script::serving(&[
            b"HTTP/1.1 302 Found\r\nLocation: https://cdn.example:8443/s?k=v\r\nContent-Length: 0\r\n\r\n",
            &ok_chunked(&audio),
        ]);
        let tcp = Script::refusing();
        let stream = fresh();
        let (delay, _) = run(attempt(&tcp, &tls, stream, "https://a.example/live"));
        assert_eq!(delay, secs(1));
        let connects = tls.connects.borrow();
        assert_eq!(connects[0], ("a.example".into(), 443));
        assert_eq!(connects[1], ("cdn.example".into(), 8443));
        assert!(
            tls.requests.borrow()[1]
                .starts_with("GET /s?k=v HTTP/1.1\r\nHost: cdn.example:8443\r\n")
        );
        assert_eq!(window_bytes(stream)[..], audio[..]);
    }

    #[test]
    fn every_redirect_status_is_followed_and_http_may_upgrade_to_https() {
        for status in [301, 302, 303, 307, 308] {
            let audio = payload(512);
            let redirect = alloc::format!(
                "HTTP/1.1 {status} X\r\nLocation: https://secure.example/live\r\n\r\n"
            );
            let tcp = Script::serving(&[redirect.as_bytes()]);
            let tls = Script::default();
            let stream = fresh();
            let mut body = b"HTTP/1.1 200 OK\r\nContent-Length: 512\r\n\r\n".to_vec();
            body.extend(&audio);
            *tls.responses.borrow_mut() = alloc::vec![body];
            let (delay, _) = run(attempt(&tcp, &tls, stream, "http://a.example/live"));
            assert_eq!(delay, secs(1), "{status}");
            assert_eq!(
                tls.connects.borrow()[0],
                ("secure.example".into(), 443),
                "{status}"
            );
            assert_eq!(window_bytes(stream)[..], audio[..], "{status}");
        }
    }

    #[test]
    fn a_redirect_from_https_to_http_is_refused_without_connecting() {
        let tls =
            Script::serving(&[b"HTTP/1.1 301 Moved\r\nLocation: http://a.example/live\r\n\r\n"]);
        let tcp = Script::serving(&[b"HTTP/1.1 200 OK\r\n\r\nxx"]);
        let stream = fresh();
        let (delay, _) = run(attempt(&tcp, &tls, stream, "https://a.example/live"));
        assert_eq!(delay, secs(5));
        assert_eq!(tcp.connections(), 0);
        assert_eq!(stream.progress().0, 0);
    }

    #[test]
    fn redirect_loops_and_long_chains_stop_the_attempt() {
        // A loop: a -> b -> a.
        let tls = Script::serving(&[
            b"HTTP/1.1 302 F\r\nLocation: https://b.example/s\r\n\r\n",
            b"HTTP/1.1 302 F\r\nLocation: https://a.example/s\r\n\r\n",
        ]);
        let (delay, _) = run(attempt(
            &Script::refusing(),
            &tls,
            fresh(),
            "https://a.example/s",
        ));
        assert_eq!((delay, tls.connections()), (secs(5), 2));
        // A chain that never repeats: stops after the fifth redirect, at the sixth response.
        let hops: alloc::vec::Vec<alloc::vec::Vec<u8>> = (1..=8)
            .map(|i| alloc::format!("HTTP/1.1 307 T\r\nLocation: /h{i}\r\n\r\n").into_bytes())
            .collect();
        let refs: alloc::vec::Vec<&[u8]> = hops.iter().map(|h| h.as_slice()).collect();
        let tls = Script::serving(&refs);
        let (delay, _) = run(attempt(
            &Script::refusing(),
            &tls,
            fresh(),
            "https://a.example/h0",
        ));
        assert_eq!(delay, secs(5));
        assert_eq!(tls.connections(), 1 + crate::fetch::MAX_REDIRECTS);
    }

    #[test]
    fn a_missing_location_or_another_status_is_not_streamed() {
        for (response, delay) in [
            (&b"HTTP/1.1 302 F\r\n\r\n"[..], 5),
            (&b"HTTP/1.1 404 NF\r\n\r\n"[..], 5),
            (&b"HTTP/1.1 500 E\r\n\r\n"[..], 5),
            (&b"HTTP/1.1 304 NM\r\n\r\n"[..], 5),
            (&b"garbage\r\n\r\n"[..], 2),
        ] {
            let tcp = Script::serving(&[response]);
            let stream = fresh();
            let (got, _) = run(attempt(
                &tcp,
                &Script::refusing(),
                stream,
                "http://a.example/s",
            ));
            assert_eq!(got, secs(delay), "{:?}", String::from_utf8_lossy(response));
            assert_eq!(stream.progress().0, 0);
        }
    }

    #[test]
    fn a_head_that_does_not_fit_is_refused_not_truncated() {
        let mut response = b"HTTP/1.1 200 OK\r\nX-Pad: ".to_vec();
        response.extend(core::iter::repeat_n(b'a', HEAD_BYTES));
        response.extend(b"\r\n\r\n");
        let tcp = Script::serving(&[&response]);
        let stream = fresh();
        let (delay, _) = run(attempt(
            &tcp,
            &Script::refusing(),
            stream,
            "http://a.example/s",
        ));
        assert_eq!(delay, secs(2));
        assert_eq!(stream.progress().0, 0);
    }

    #[test]
    fn a_station_url_the_producer_cannot_use_is_logged_and_retried_later() {
        let (tcp, tls) = (Script::refusing(), Script::refusing());
        for url in [
            "http://a.example:0/",
            "http://[::1]/",
            "https://u@a.example/",
        ] {
            let (delay, _) = run(attempt(&tcp, &tls, fresh(), url));
            assert_eq!(delay, secs(5), "{url}");
        }
        assert_eq!(tcp.connections() + tls.connections(), 0);
    }

    // --- Repeated failures are logged sparingly --------------------------------------------------

    #[test]
    fn the_first_failures_are_logged_then_one_in_ten() {
        let logged: alloc::vec::Vec<u32> = (0..35u32)
            .filter(|before| Failures::is_logged(*before))
            .map(|before| before + 1)
            .collect();
        // Failures number 1, 2 and 3, then the 11th, 21st and 31st.
        assert_eq!(logged, [1, 2, 3, 11, 21, 31]);
        let failures = Failures::new();
        assert_eq!(
            failures.line(),
            Some(1),
            "the first failure is always logged"
        );
    }

    #[test]
    fn a_station_that_cannot_be_reached_is_counted_and_a_good_answer_ends_the_count() {
        let (tls, stream) = (Script::refusing(), fresh());
        let failures = Failures::new();
        // Five refused connections in a row.
        let tcp = Script::refusing();
        for expected in 1..=5 {
            let (delay, _) = run(attempt_counting(
                &tcp,
                &tls,
                stream,
                "http://radio.example/live",
                &failures,
            ));
            assert_eq!(delay, secs(2));
            assert_eq!(failures.count(), expected);
        }
        // The fourth failure is not logged, the first three were.
        assert_eq!(Failures::new().line(), Some(1));
        assert!(failures.line().is_none(), "the 6th is silent");

        // The station answers: the count ends, and what it was is returned for the summary.
        let audio = payload(512);
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 512\r\n\r\n".to_vec();
        response.extend(&audio);
        let tcp = Script::serving(&[&response]);
        let (delay, _) = run(attempt_counting(
            &tcp,
            &tls,
            stream,
            "http://radio.example/live",
            &failures,
        ));
        assert_eq!(delay, secs(1));
        assert_eq!(failures.count(), 0, "back to normal");
        assert_eq!(window_bytes(stream)[..], audio[..]);
        assert_eq!(
            failures.line(),
            Some(1),
            "the next failure is a first one again"
        );
    }

    #[test]
    fn a_bad_status_or_an_unusable_address_counts_as_a_failure_but_a_redirect_does_not() {
        let (tls, stream) = (Script::refusing(), fresh());
        let failures = Failures::new();
        let not_found = Script::serving(&[b"HTTP/1.1 404 Not Found\r\n\r\n"]);
        run(attempt_counting(
            &not_found,
            &tls,
            stream,
            "http://r.example/x",
            &failures,
        ));
        assert_eq!(failures.count(), 1);
        run(attempt_counting(
            &Script::refusing(),
            &tls,
            stream,
            "not a url",
            &failures,
        ));
        assert_eq!(failures.count(), 2);
        // A redirect that is followed is not a failure of the station.
        let audio = payload(512);
        let mut ok = b"HTTP/1.1 200 OK\r\nContent-Length: 512\r\n\r\n".to_vec();
        ok.extend(&audio);
        let redirect = Script::serving(&[
            b"HTTP/1.1 302 Found\r\nLocation: http://b.example/live\r\n\r\n",
            &ok,
        ]);
        run(attempt_counting(
            &redirect,
            &tls,
            stream,
            "http://a.example/live",
            &failures,
        ));
        assert_eq!(failures.count(), 0, "the 200 ended the count");
    }
    #[test]
    fn the_snapshot_is_due_every_period_at_most() {
        let t0 = Instant::from_secs(100);
        let mut snapshots = Snapshots(t0);
        assert!(!snapshots.due(t0));
        assert!(!snapshots.due(t0 + Duration::from_secs(14)));
        assert!(snapshots.due(t0 + Duration::from_secs(15)));
        // Counted again from the snapshot, whatever the time since.
        assert!(!snapshots.due(t0 + Duration::from_secs(29)));
        assert!(snapshots.due(t0 + Duration::from_secs(30)));
        assert!(snapshots.due(t0 + Duration::from_secs(300)));
        assert!(!snapshots.due(t0 + Duration::from_secs(301)));
        // A clock that did not move forward never makes it due.
        assert!(!snapshots.due(t0));
    }

    #[test]
    fn the_snapshot_level_is_off_unless_the_detailed_diagnostic_is_on() {
        let normal = iobewi_log::default_policy(crate::logging::LOG_TARGET).unwrap();
        assert!(!normal.enabled(log::Level::Debug, module_path!()));
        assert!(normal.enabled(log::Level::Info, module_path!()));
        let mut debug = normal.clone();
        debug
            .add_target("streambewi::stream", log::LevelFilter::Debug)
            .unwrap();
        assert!(debug.enabled(log::Level::Debug, "streambewi::stream"));
        assert!(!debug.enabled(log::Level::Debug, "streambewi::setup"));
    }
}

#[cfg(test)]
#[path = "stream_save_tests.rs"]
mod save_tests;
