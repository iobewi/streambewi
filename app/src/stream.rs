use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use embassy_futures::select::{Either3, select3};

use crate::fetch::{Chain, TcpConnector, is_redirect, origin};
use alloc::string::String;
use edge_http::{BodyType, io::Body};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use iobewi_fat16::{FileSource, ReadStatus, SECTOR_SIZE};
use iobewi_http_client::{HeadError, Target, get, read_head};
use iobewi_net_io::{Close, Connector, ErrorType};
use streambewi_core::{PREBUFFER_BYTES, Stream, StreamFile, is_ready, new_stream, progress};

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
/// Room for a response head; a longer one is refused (`TooLarge`), not truncated.
const HEAD_BYTES: usize = 2048;
/// Longest the producer waits to open the connection and receive the response headers.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

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
        self.stream
            .inner
            .lock(|cell| f(&mut StreamFile::new(&mut cell.borrow_mut())))
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
                    "stream: REBASE on re-read of sector {} base {:?} -> {:?} live={}",
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
            Either3::Third(()) => {}
            Either3::First(never) => match never {},
            Either3::Second(()) => unreachable!(),
        }
        // The old producer is dropped (its socket is released). Leave the bus first, then restart.
        let since = Instant::now();
        if let Some(link) = link {
            link.set_connected(false);
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
            Ok(wrote) => log::info!(
                "station: selection persisted={} in {} ms",
                wrote,
                started.elapsed().as_millis()
            ),
            Err(()) => log::error!(
                "station: persisting the selection FAILED after {} ms: active in RAM, NOT saved",
                started.elapsed().as_millis()
            ),
        }
        pending_attach = Some((since, live));
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
async fn exchange<K>(connector: &K, target: &Target, stream: &'static SharedStream) -> Exchange
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
            log::info!(
                "stream: {} connect to {} failed: {}",
                scheme,
                origin(target),
                err
            );
            return Exchange::Retry(Duration::from_secs(2));
        }
        Err(_) => {
            log::info!(
                "stream: no {} connection to {} after {} s, retrying",
                scheme,
                origin(target),
                CONNECT_TIMEOUT.as_secs()
            );
            return Exchange::Retry(Duration::from_secs(2));
        }
    };
    log::info!(
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
        log::info!("stream: request error {}", err);
        let _ = conn.close().await;
        return Exchange::Retry(Duration::from_secs(2));
    }
    let head = match with_timeout(CONNECT_TIMEOUT, read_head(&mut conn, &mut head_buf)).await {
        Ok(Ok(head)) => head,
        Ok(Err(err)) => {
            match err {
                HeadError::Io(_) => log::info!("stream: response head read failed"),
                HeadError::Closed => log::info!("stream: closed before the response head"),
                HeadError::TooLarge => {
                    log::warn!("stream: response head over {} bytes, refused", HEAD_BYTES)
                }
                HeadError::Malformed => log::warn!("stream: response head refused (malformed)"),
            }
            let _ = conn.close().await;
            return Exchange::Retry(Duration::from_secs(2));
        }
        Err(_) => {
            log::info!(
                "stream: no response head after {} s",
                CONNECT_TIMEOUT.as_secs()
            );
            let _ = conn.close().await;
            return Exchange::Retry(Duration::from_secs(2));
        }
    };
    log::info!(
        "stream: HTTP status={} content_length={:?} chunked={}",
        head.status,
        head.content_length,
        head.chunked
    );
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
    let Ok(mut target) = Target::parse(url) else {
        log::warn!("stream: unusable url {}", url);
        return Duration::from_secs(5);
    };
    let mut chain = Chain::new(&target);
    loop {
        // An `https` target is never fetched in clear: the TLS connector or nothing.
        let outcome = if target.tls {
            exchange(tls, &target, stream).await
        } else {
            exchange(tcp, &target, stream).await
        };
        match outcome {
            Exchange::Streamed => return Duration::from_secs(1),
            Exchange::Retry(delay) => return delay,
            Exchange::Redirect { status, location } => {
                match chain.follow(&target, location.as_deref()) {
                    Ok(next) => {
                        log::info!("stream: redirect {} to {}", status, origin(&next));
                        target = next;
                    }
                    Err(refusal) => {
                        log::warn!("stream: redirect {} not followed: {}", status, refusal);
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
    loop {
        let reconnect = stream.mark_reconnect();
        log::info!("stream: connecting attempt={} url={}", reconnect, url);
        Timer::after(attempt(tcp, tls, stream, url).await).await;
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

    loop {
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
}
