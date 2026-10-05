use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_net::{
    Stack,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Timer};
use embedded_io_async::Read as _;
use reqwless::{
    client::HttpClient,
    request::{Method, RequestBuilder},
};
use iobewi_fat16::{FileSource, ReadStatus, SECTOR_SIZE};
use usb_radio_core::{PREBUFFER_BYTES, Stream, StreamFile, is_ready, new_stream, progress};

pub const STREAM_URL: &str =
    "http://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3";

pub static STREAM: SharedStream = SharedStream::new();

/// Largest chunk read from the network and pushed into the window at once.
const CHUNK_BYTES: usize = 2048;

/// The product stream window shared between the network task and the USB MSC task.
pub struct SharedStream {
    inner: Mutex<CriticalSectionRawMutex, RefCell<Stream>>,
    reconnects: AtomicU32,
}

impl SharedStream {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(RefCell::new(new_stream())),
            reconnects: AtomicU32::new(0),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.inner.lock(|cell| is_ready(&cell.borrow()))
    }

    pub fn progress(&self) -> (u64, u64) {
        self.inner.lock(|cell| progress(&cell.borrow()))
    }

    fn writable(&self) -> usize {
        self.inner
            .lock(|cell| cell.borrow().writable().min(CHUNK_BYTES))
    }

    fn push(&self, input: &[u8]) -> usize {
        self.inner.lock(|cell| cell.borrow_mut().push(input))
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
        self.with_file(|file| file.begin_session());
        let (live, _) = self.stream.progress();
        log::info!("stream: session start live={}", live);
    }

    fn end_session(&mut self) {
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
            ("User-Agent", "usb-radio-poc/0.3"),
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
            while stream.writable() == 0 {
                Timer::after(Duration::from_millis(5)).await;
            }

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
