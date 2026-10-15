use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_net::{
    Stack,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Read as _;
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

        let request = match client.request(Method::GET, STREAM_URL).await {
            Ok(request) => request,
            Err(err) => {
                log::info!("stream: request error {:?}", err);
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

        let response = match request.send(&mut header_buf).await {
            Ok(response) => response,
            Err(err) => {
                log::info!("stream: connect/send error {:?}", err);
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

            match body.read(&mut buf[..want]).await {
                Ok(0) => {
                    log::info!("stream: server closed connection");
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
                Err(err) => {
                    log::info!("stream: body read error {:?}", err);
                    break;
                }
            }
        }

        Timer::after(Duration::from_secs(1)).await;
    }
}
