//! Bench qualification only (`qual`, #31): when the USB side of the product gets as far as it does, as counters
//! and first/last times. Nothing here changes what the product does; it is bounded (a few atomics, one line per
//! report) and logs nothing per sector.
//!
//! The question it answers is where a boot that never reads stops: the physical connection, the enumeration, the
//! SCSI commands, or the first data read. `msc: cmd` lines (usb-debug) cover the first non-READ commands; this
//! covers the bus events and every READ(10).

use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
use embassy_time::Instant;

#[derive(Clone, Copy)]
pub enum Ev {
    /// The product creates the USB device (prebuffer ready).
    Enabled,
    BusReset,
    Addressed,
    Configured,
    Unconfigured,
    Suspend,
    Resume,
    /// A READ(10) command starts (`ReadPolicy::command_started`).
    Read10,
    /// A sector was not available when a READ(10) asked for it.
    Unavailable,
    /// The device left the bus on purpose (`UsbLink::set_connected(false)`).
    Detach,
    Attach,
}

const NAMES: [&str; 11] = [
    "enabled", "reset", "addressed", "configured", "unconfigured", "suspend", "resume", "read10",
    "unavail", "detach", "attach",
];
const N: usize = NAMES.len();
static COUNT: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
/// Milliseconds since boot, plus one (0: never).
static FIRST: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static LAST: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];

fn now() -> u32 {
    Instant::now().as_millis() as u32 + 1
}

pub fn note(ev: Ev) {
    let i = ev as usize;
    let t = now();
    COUNT[i].fetch_add(1, Relaxed);
    let _ = FIRST[i].compare_exchange(0, t, Relaxed, Relaxed);
    LAST[i].store(t, Relaxed);
}

/// Milliseconds since boot of the first `ev`, if it happened.
pub fn first(ev: Ev) -> Option<u32> {
    FIRST[ev as usize].load(Relaxed).checked_sub(1)
}

static PREV_INDEX: AtomicU32 = AtomicU32::new(u32::MAX);
static DATA_LINES: AtomicU32 = AtomicU32::new(0);

/// A data sector of the file was asked for. Logs the first one and every jump (not the sequential ones, not the
/// retries of the same sector), at most 48 lines per boot: where the host starts reading and where it leaps.
pub fn data_read(index: u32, pending: bool, live: u64, base: Option<u64>) {
    let prev = PREV_INDEX.swap(index.wrapping_add(1), Relaxed);
    let same = index.wrapping_add(1) == prev;
    if prev != u32::MAX && (index == prev || same) {
        return;
    }
    if DATA_LINES.fetch_add(1, Relaxed) < 48 {
        log::info!(
            "usb-qual data: t={} index={} expected={} pending={} live={} base={:?}",
            now() - 1,
            index,
            prev,
            pending,
            live,
            base
        );
    }
}

static HEADS: AtomicU32 = AtomicU32::new(0);

/// The first bytes of sector 0 as the host gets them, and where MPEG frame syncs (0xFF, then 0xE0 or more)
/// start in it: the content the host decides its first jump from. At most 8 lines per boot.
pub fn head(sector: &[u8]) {
    if HEADS.fetch_add(1, Relaxed) >= 8 {
        return;
    }
    let mut syncs = [u16::MAX; 3];
    let mut n = 0;
    for i in 0..sector.len().saturating_sub(1) {
        if n < syncs.len() && sector[i] == 0xFF && sector[i + 1] & 0xE0 == 0xE0 {
            syncs[n] = i as u16;
            n += 1;
        }
    }
    log::info!(
        "usb-qual head: t={} b={:02x?} id3={} syncs={:?}",
        now() - 1,
        &sector[..16.min(sector.len())],
        sector.starts_with(b"ID3"),
        &syncs[..n]
    );
}

/// One line: for each event `count@first/last` in milliseconds since boot, then the stream's progress.
pub fn report(tag: &str) {
    let mut line = heapless_line::Line::new();
    for i in 0..N {
        let n = COUNT[i].load(Relaxed);
        if n > 0 {
            line.push(NAMES[i], n, FIRST[i].load(Relaxed) - 1, LAST[i].load(Relaxed) - 1);
        }
    }
    let (written, consumed) = crate::stream::STREAM.progress();
    log::info!(
        "usb-qual {tag}: t={}{} written={written} consumed={consumed}",
        now() - 1,
        line.as_str()
    );
}

mod heapless_line {
    use core::fmt::Write;
    /// A fixed buffer for the report line: 11 events, each at most ~40 bytes.
    pub struct Line {
        buf: [u8; 480],
        len: usize,
    }
    impl Line {
        pub const fn new() -> Self {
            Self { buf: [0; 480], len: 0 }
        }
        pub fn push(&mut self, name: &str, n: u32, first: u32, last: u32) {
            let _ = write!(self, " {name}={n}@{first}/{last}");
        }
        pub fn as_str(&self) -> &str {
            core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
        }
    }
    impl Write for Line {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let room = self.buf.len() - self.len;
            let n = s.len().min(room);
            self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }
}
