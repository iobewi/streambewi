use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_net::{
    Stack,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use iobewi_fat16::{FileSource, ReadStatus, SECTOR_SIZE};
use reqwless::{
    client::HttpClient,
    request::{Method, RequestBuilder},
};
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
    /// Keep the consumer session across the next reconnection of the host (see `hold_session`).
    hold: AtomicBool,
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
            hold: AtomicBool::new(false),
            last_push: Mutex::new(Cell::new(0)),
        }
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
        self.with_file(|file| file.read_file_sector(index, out))
    }
}

pub async fn run(stack: Stack<'static>, stream: &'static SharedStream) -> ! {
    let tcp_state = mk_static!(
        TcpClientState<1, 4096, 4096>,
        TcpClientState::<1, 4096, 4096>::new()
    );
    let tcp_client = TcpClient::new(stack, tcp_state);
    let dns_client = DnsSocket::new(stack);

    stack.wait_config_up().await;
    if let Some(config) = stack.config_v4() {
        log::info!("net: got IP {}", config.address);
    }

    loop {
        let reconnect = stream.mark_reconnect();
        log::info!(
            "stream: connecting attempt={} url={}",
            reconnect,
            STREAM_URL
        );

        let mut client = HttpClient::new(&tcp_client, &dns_client);
        let mut header_buf = [0u8; 2048];

        let request =
            match with_timeout(CONNECT_TIMEOUT, client.request(Method::GET, STREAM_URL)).await {
                Ok(Ok(request)) => request,
                Ok(Err(err)) => {
                    log::info!("stream: request error {:?}", err);
                    Timer::after(Duration::from_secs(2)).await;
                    continue;
                }
                Err(_) => {
                    log::info!(
                        "stream: no connection after {} s, retrying",
                        CONNECT_TIMEOUT.as_secs()
                    );
                    Timer::after(Duration::from_secs(2)).await;
                    continue;
                }
            };

        // reqwless already provides Host from the URL.
        let mut request = request.headers(&[
            ("Connection", "close"),
            ("Icy-MetaData", "0"),
            ("User-Agent", "streambewi/0.1"),
        ]);

        let response = match with_timeout(CONNECT_TIMEOUT, request.send(&mut header_buf)).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                log::info!("stream: connect/send error {:?}", err);
                Timer::after(Duration::from_secs(2)).await;
                continue;
            }
            Err(_) => {
                log::info!(
                    "stream: no response after {} s, retrying",
                    CONNECT_TIMEOUT.as_secs()
                );
                Timer::after(Duration::from_secs(2)).await;
                continue;
            }
        };

        log::info!(
            "stream: HTTP status={} content_length={:?}",
            response.status.0,
            response.content_length
        );

        if response.status.0 != 200 {
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        let mut body = response.body().reader();
        let mut buf = [0u8; CHUNK_BYTES];

        loop {
            stream.wait_for_room().await;

            let want = core::cmp::min(stream.writable(), buf.len());
            if want == 0 {
                continue;
            }

            match read_bounded(&mut body, &mut buf[..want], READ_TIMEOUT).await {
                Err(ReadStop::Closed) => {
                    log::info!("stream: server closed connection");
                    break;
                }
                Err(ReadStop::Silent) => {
                    // Half-open connection: drop it (the socket is removed and the pool slot freed)
                    // and let the reconnection below open a new one.
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

                    let before_bucket = before / (64 * 1024);
                    let after_bucket = written / (64 * 1024);
                    if before < PREBUFFER_BYTES && written >= PREBUFFER_BYTES
                        || after_bucket != before_bucket
                    {
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

        Timer::after(Duration::from_secs(1)).await;
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
}
