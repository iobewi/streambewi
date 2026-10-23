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

// ---------------------------------------------------------------------------------------------------------
// The first READ(10) commands of each MSC session, and the exact bytes of the file's first four sectors.

use core::cell::RefCell;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use iobewi_fat16::{ReadOnlyBlockDevice, ReadStatus, SECTOR_SIZE};

/// READ(10) commands detailed per session.
const COMMANDS: u32 = 16;
/// First LBA of the file's data: boot sector, two FATs, root directory (`Geometry::data_start_lba`).
const DATA_START: u32 = 1 + 2 * 129 + 2;

struct Tracker {
    session: u32,
    session_start: u32,
    command: u32,
    open: bool,
    start: u32,
    prev_end: u32,
    first_lba: u32,
    last_lba: u32,
    sectors: u32,
    ready: u32,
    pending: u32,
    other: u32,
    crc: u32,
    pending_logged: bool,
    live: u64,
    consumed: u64,
    base: Option<u64>,
    heads: u8,
}

static TRACKER: Mutex<CriticalSectionRawMutex, RefCell<Tracker>> = Mutex::new(RefCell::new(Tracker {
    session: 0,
    session_start: 0,
    command: 0,
    open: false,
    start: 0,
    prev_end: 0,
    first_lba: 0,
    last_lba: 0,
    sectors: 0,
    ready: 0,
    pending: 0,
    other: 0,
    crc: 0,
    pending_logged: false,
    live: 0,
    consumed: 0,
    base: None,
    heads: 0,
}));

struct Hex<'a>(&'a [u8]);
impl core::fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

fn crc32(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    crc
}

fn file_index(lba: u32) -> Option<u32> {
    lba.checked_sub(DATA_START)
}

fn log_command(t: &Tracker, end: u32, note: &str) {
    let fidx = file_index(t.first_lba);
    log::info!(
        "usb-qual rd: s={} c={} {note} t={} gap={} dur={} lba={}..{} fidx={:?} n={} ready={} pend={} other={} crc={:08x} live={} consumed={} base={:?}",
        t.session,
        t.command,
        t.start.wrapping_sub(t.session_start),
        t.start.wrapping_sub(t.prev_end),
        end.wrapping_sub(t.start),
        t.first_lba,
        t.last_lba,
        fidx,
        t.sectors,
        t.ready,
        t.pending,
        t.other,
        t.crc,
        t.live,
        t.consumed,
        t.base
    );
}

fn flush(t: &mut Tracker) {
    if t.open && t.sectors > 0 {
        let end = now() - 1;
        log_command(t, end, "done");
        t.prev_end = end;
    }
    t.open = false;
}

pub fn session_begin() {
    TRACKER.lock(|c| {
        let mut t = c.borrow_mut();
        t.session += 1;
        t.session_start = now() - 1;
        t.command = 0;
        t.open = false;
        t.prev_end = t.session_start;
        t.heads = 0;
    });
}

pub fn session_end() {
    TRACKER.lock(|c| flush(&mut c.borrow_mut()));
}

/// A READ(10) starts: closes the previous command's line and opens this one (the first `COMMANDS` only).
pub fn command_started() {
    let (live, consumed) = crate::stream::STREAM.progress();
    let base = crate::stream::STREAM.session_base();
    TRACKER.lock(|c| {
        let mut t = c.borrow_mut();
        flush(&mut t);
        t.command += 1;
        t.open = t.command <= COMMANDS;
        if t.open {
            t.start = now() - 1;
            t.first_lba = u32::MAX;
            t.last_lba = 0;
            t.sectors = 0;
            t.ready = 0;
            t.pending = 0;
            t.other = 0;
            t.crc = 0xFFFF_FFFF;
            t.pending_logged = false;
            t.live = live;
            t.consumed = consumed;
            t.base = base;
        }
    });
}

/// A sector was asked of the disk by the open command.
pub fn sector(lba: u32, status: ReadStatus, data: &[u8; SECTOR_SIZE]) {
    TRACKER.lock(|c| {
        let mut t = c.borrow_mut();
        if !t.open {
            return;
        }
        t.first_lba = t.first_lba.min(lba);
        t.last_lba = t.last_lba.max(lba);
        match status {
            ReadStatus::Ready => {
                t.ready += 1;
                t.sectors += 1;
                t.crc = crc32(t.crc, data);
            }
            ReadStatus::Pending => {
                // The disk retries a pending sector every 5 ms: count the distinct ones, log the first.
                if !t.pending_logged {
                    t.pending_logged = true;
                    t.sectors += 1;
                    let now_ms = now() - 1;
                    log_command(&t, now_ms, "PENDING");
                }
                t.pending += 1;
            }
            _ => {
                t.other += 1;
                t.sectors += 1;
            }
        }
        if status == ReadStatus::Ready
            && let Some(idx) = file_index(lba)
            && idx < 4
            && t.heads & (1 << idx) == 0
        {
            t.heads |= 1 << idx;
            let find = |needle: &[u8]| data.windows(needle.len()).position(|w| w == needle);
            log::info!(
                "usb-qual sec: s={} idx={} lba={} id3={:?} xing={:?} info={:?} vbri={:?} tag={:?} hex={}",
                t.session,
                idx,
                lba,
                find(b"ID3"),
                find(b"Xing"),
                find(b"Info"),
                find(b"VBRI"),
                find(b"TAG"),
                Hex(data)
            );
        }
    });
}

/// The block device the MSC class reads, seen through the qual tracker. It forwards everything unchanged.
pub struct TrackedDisk<D>(pub D);

impl<D: ReadOnlyBlockDevice> ReadOnlyBlockDevice for TrackedDisk<D> {
    fn last_lba(&self) -> u32 {
        self.0.last_lba()
    }
    fn begin_session(&mut self) {
        session_begin();
        self.0.begin_session();
    }
    fn end_session(&mut self) {
        session_end();
        self.0.end_session();
    }
    fn read_sector(&mut self, lba: u32, out: &mut [u8; SECTOR_SIZE]) -> ReadStatus {
        let status = self.0.read_sector(lba, out);
        sector(lba, status, out);
        status
    }
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
