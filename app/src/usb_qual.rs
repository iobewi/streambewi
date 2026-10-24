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
    "enabled",
    "reset",
    "addressed",
    "configured",
    "unconfigured",
    "suspend",
    "resume",
    "read10",
    "unavail",
    "detach",
    "attach",
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

// ---------------------------------------------------------------------------------------------------------
// The first READ(10) commands of each MSC session and the first four sectors of the file, kept in RAM and printed
// later by the qual task (`drain`), outside the USB path: nothing is printed while a command is being answered.

use core::cell::RefCell;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use iobewi_fat16::{ReadOnlyBlockDevice, ReadStatus, SECTOR_SIZE};

/// READ(10) commands detailed per session.
const COMMANDS: u32 = 240;
/// First LBA of the file's data: boot sector, two FATs, root directory (`Geometry::data_start_lba`).
const DATA_START: u32 = 1 + 2 * 129 + 2;
const RECORDS: usize = 32;
/// Sessions whose first four file sectors are kept.
const SECTOR_SESSIONS: usize = 1;
/// Audio sectors (file index of 8 or more) kept for the current session.
const AUDIO_KEEP: usize = 16;
/// Bytes of a sector per printed line, and the age a record must have before it is printed.
const CHUNK: usize = 128;
const PRINT_AFTER_MS: u32 = 800;

#[derive(Clone, Copy)]
struct Rec {
    session: u32,
    command: u32,
    /// 0 free, 1 waiting to be printed, 2 printed.
    state: u8,
    start: u32,
    end: u32,
    first_lba: u32,
    last_lba: u32,
    sectors: u32,
    ready: u32,
    pending: u32,
    other: u32,
    crc: u32,
    /// MPEG frame syncs (0xFF then 0xE0 or more) in the sectors served: audio has many, zeroes none.
    syncs: u32,
    /// The first 16 bytes of the first sector served by the command.
    head: [u8; 16],
    /// First valid MPEG-1/2 Layer III header in the first sector served: offset, frame length, and how many
    /// further headers follow at the expected distance inside that sector (None if no header).
    frame: Option<(u16, u16, u8)>,
    /// File-offset shift of the rebase in force when the command started (0 = none).
    shift: u64,
    live: u64,
    consumed: u64,
    base: u64,
}

const EMPTY: Rec = Rec {
    session: 0,
    command: 0,
    state: 0,
    start: 0,
    end: 0,
    first_lba: 0,
    last_lba: 0,
    sectors: 0,
    ready: 0,
    pending: 0,
    other: 0,
    crc: 0,
    syncs: 0,
    head: [0; 16],
    frame: None,
    shift: 0,
    live: 0,
    consumed: 0,
    base: u64::MAX,
};

struct Qual {
    session: u32,
    session_start: u32,
    command: u32,
    open: bool,
    cur: Rec,
    recs: [Rec; RECORDS],
    dropped: u32,
    /// Per kept session: the session number, which of sectors 0 to 3 were seen, the next chunk to print.
    sec_session: [u32; SECTOR_SESSIONS],
    sec_seen: [u8; SECTOR_SESSIONS],
    sec_next: [u8; SECTOR_SESSIONS],
    sectors: [[[u8; SECTOR_SIZE]; 4]; SECTOR_SESSIONS],
    /// The first `AUDIO_KEEP` distinct audio sectors served in the current session, and the next chunk to print.
    aud: [[u8; SECTOR_SIZE]; AUDIO_KEEP],
    aud_idx: [u32; AUDIO_KEEP],
    aud_n: u8,
    aud_next: u8,
}

static QUAL: Mutex<CriticalSectionRawMutex, RefCell<Qual>> = Mutex::new(RefCell::new(Qual {
    session: 0,
    session_start: 0,
    command: 0,
    open: false,
    cur: EMPTY,
    recs: [EMPTY; RECORDS],
    dropped: 0,
    sec_session: [0; SECTOR_SESSIONS],
    sec_seen: [0; SECTOR_SESSIONS],
    sec_next: [0; SECTOR_SESSIONS],
    sectors: [[[0; SECTOR_SIZE]; 4]; SECTOR_SESSIONS],
    aud: [[0; SECTOR_SIZE]; AUDIO_KEEP],
    aud_idx: [0; AUDIO_KEEP],
    aud_n: 0,
    aud_next: 0,
}));

/// Highest file sector index served, start of the last READ(10) and of the last sector read, since boot.
static MAX_INDEX: AtomicU32 = AtomicU32::new(0);
static LAST_SECTOR_MS: AtomicU32 = AtomicU32::new(0);
static SESSIONS: AtomicU32 = AtomicU32::new(0);

struct Hex<'a>(&'a [u8]);
impl core::fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Length in bytes of the MPEG-1/2 Layer III frame whose header is `h`, if the header is valid.
fn frame_len(h: &[u8]) -> Option<u16> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let ver = (h[1] >> 3) & 3; // 3 = MPEG-1, 2 = MPEG-2, 0 = MPEG-2.5, 1 = reserved
    let layer = (h[1] >> 1) & 3; // 1 = Layer III
    let br = (h[2] >> 4) as usize;
    let sr = ((h[2] >> 2) & 3) as usize;
    if ver == 1 || layer != 1 || br == 0 || br == 15 || sr == 3 {
        return None;
    }
    const BR1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const BR2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    const SR1: [u32; 3] = [44100, 48000, 32000];
    let pad = ((h[2] >> 1) & 1) as u32;
    let (kbps, rate, k) = if ver == 3 {
        (BR1[br], SR1[sr], 144)
    } else {
        (BR2[br], SR1[sr] / if ver == 2 { 2 } else { 4 }, 72)
    };
    Some((k * kbps * 1000 / rate + pad) as u16)
}

/// Looks for the first valid Layer III header in a sector and follows the chain of frames inside it.
fn frame_probe(data: &[u8; SECTOR_SIZE]) -> Option<(u16, u16, u8)> {
    let (off, len) = (0..SECTOR_SIZE - 3).find_map(|i| frame_len(&data[i..]).map(|l| (i, l as usize)))?;
    let (mut pos, mut chain) = (off + len, 0u8);
    while pos + 4 <= SECTOR_SIZE && frame_len(&data[pos..]).is_some() {
        chain += 1;
        pos += frame_len(&data[pos..]).unwrap_or(1) as usize;
    }
    Some((off as u16, len as u16, chain))
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

fn flush(q: &mut Qual) {
    if q.open && q.cur.sectors > 0 {
        let free = q.recs.iter().position(|r| r.state == 0);
        match free {
            Some(i) => {
                q.recs[i] = q.cur;
                q.recs[i].state = 1;
            }
            None => q.dropped += 1,
        }
    }
    q.open = false;
}

pub fn session_begin() {
    SESSIONS.fetch_add(1, Relaxed);
    QUAL.lock(|c| {
        let mut q = c.borrow_mut();
        q.session += 1;
        q.session_start = now() - 1;
        q.command = 0;
        q.open = false;
        q.aud_n = 0;
        q.aud_next = 0;
        let slot = q.session as usize - 1;
        if slot < SECTOR_SESSIONS {
            q.sec_session[slot] = q.session;
        }
    });
}

pub fn session_end() {
    QUAL.lock(|c| flush(&mut c.borrow_mut()));
}

/// A READ(10) starts: closes the previous command's record and opens this one (the first `COMMANDS` only).
pub fn command_started() {
    let (live, consumed) = crate::stream::STREAM.progress();
    let base = crate::stream::STREAM.session_base().unwrap_or(u64::MAX);
    let shift = crate::stream::STREAM.resume_stats().4;
    let t = now() - 1;
    QUAL.lock(|c| {
        let mut q = c.borrow_mut();
        flush(&mut q);
        q.command += 1;
        q.open = q.command <= COMMANDS;
        if q.open {
            q.cur = Rec {
                session: q.session,
                command: q.command,
                start: t,
                first_lba: u32::MAX,
                crc: 0xFFFF_FFFF,
                shift,
                live,
                consumed,
                base,
                ..EMPTY
            };
        }
    });
}

/// A sector was asked of the disk and has been answered (`end` is when the answer was complete).
pub fn sector(lba: u32, status: ReadStatus, data: &[u8; SECTOR_SIZE]) {
    let t = now() - 1;
    if status == ReadStatus::Ready
        && let Some(idx) = file_index(lba)
    {
        MAX_INDEX.fetch_max(idx, Relaxed);
        LAST_SECTOR_MS.store(t, Relaxed);
    }
    // The checksum is computed outside the critical section.
    let open = QUAL.lock(|c| c.borrow().open);
    let (crc, syncs) = if open && status == ReadStatus::Ready {
        (
            Some(crc32(0, data)),
            data.windows(2)
                .filter(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0)
                .count() as u32,
        )
    } else {
        (None, 0)
    };
    QUAL.lock(|c| {
        let mut q = c.borrow_mut();
        if q.open {
            let r = &mut q.cur;
            r.first_lba = r.first_lba.min(lba);
            r.last_lba = r.last_lba.max(lba);
            if r.sectors == 0 {
                r.head.copy_from_slice(&data[..16]);
                r.frame = frame_probe(data);
            }
            r.sectors += 1;
            r.end = t;
            match status {
                ReadStatus::Ready => {
                    r.ready += 1;
                    // Combined by xor: the same sectors give the same value whatever their order of arrival.
                    r.crc ^= crc.unwrap_or(0);
                    r.syncs += syncs;
                }
                ReadStatus::Pending => r.pending += 1,
                _ => r.other += 1,
            }
        }
        if status == ReadStatus::Ready
            && let Some(idx) = file_index(lba)
            && idx >= 8
            && (q.aud_n as usize) < AUDIO_KEEP
            && !q.aud_idx[..q.aud_n as usize].contains(&idx)
        {
            let n = q.aud_n as usize;
            q.aud_idx[n] = idx;
            q.aud[n] = *data;
            q.aud_n += 1;
        }
        if status == ReadStatus::Ready
            && let Some(idx) = file_index(lba)
            && idx < 4
        {
            let session = q.session;
            if let Some(slot) = q.sec_session.iter().position(|s| *s == session)
                && q.sec_seen[slot] & (1 << idx) == 0
            {
                q.sec_seen[slot] |= 1 << idx;
                q.sectors[slot][idx as usize] = *data;
            }
        }
    });
}

/// Prints what is old enough, a little at a time. Called by the qual task only.
pub fn drain() {
    let t = now() - 1;
    // One record per call.
    let rec = QUAL.lock(|c| {
        let mut q = c.borrow_mut();
        let found = q
            .recs
            .iter_mut()
            .filter(|r| r.state == 1 && t.wrapping_sub(r.end.max(r.start)) >= PRINT_AFTER_MS)
            .min_by_key(|r| (r.session, r.command));
        found.map(|r| {
            r.state = 0;
            *r
        })
    });
    if let Some(r) = rec {
        log::info!(
            "usb-qual rd: s={} c={} start={} end={} dur={} lba={}..{} fidx={:?} n={} ready={} pend={} other={} xor={:08x} syncs={} head={} frame={:?} shift={} live={} consumed={} base={:?}",
            r.session,
            r.command,
            r.start,
            r.end,
            r.end.wrapping_sub(r.start),
            r.first_lba,
            r.last_lba,
            file_index(r.first_lba),
            r.sectors,
            r.ready,
            r.pending,
            r.other,
            r.crc,
            r.syncs,
            Hex(&r.head),
            r.frame,
            r.shift,
            r.live,
            r.consumed,
            if r.base == u64::MAX {
                None
            } else {
                Some(r.base)
            }
        );
        return;
    }
    // One 128-byte chunk of the kept audio sectors per call, once the session has been quiet for a while.
    let mut abuf = [0u8; CHUNK];
    let achunk = QUAL.lock(|c| {
        let mut q = c.borrow_mut();
        let per = SECTOR_SIZE / CHUNK;
        let n = q.aud_next as usize;
        if n >= q.aud_n as usize * per
            || (q.aud_n as usize) < AUDIO_KEEP && t.wrapping_sub(LAST_SECTOR_MS.load(Relaxed)) < 1500
        {
            return None;
        }
        let (i, off) = (n / per, n % per * CHUNK);
        abuf.copy_from_slice(&q.aud[i][off..off + CHUNK]);
        q.aud_next += 1;
        Some((q.session, q.aud_idx[i], off))
    });
    if let Some((s, idx, off)) = achunk {
        log::info!("usb-qual aud: s={s} fidx={idx} off={off} hex={}", Hex(&abuf));
        return;
    }
    // One 128-byte chunk of a kept sector per call, once its session has been running a while.
    let mut buf = [0u8; CHUNK];
    let chunk = QUAL.lock(|c| {
        let mut q = c.borrow_mut();
        for slot in 0..SECTOR_SESSIONS {
            if q.sec_session[slot] == 0 || q.sec_seen[slot] != 0x0F {
                continue;
            }
            // Only the current or an earlier session: its start is not stored per slot, so use the age of the
            // last read as the quiet signal.
            if t.wrapping_sub(LAST_SECTOR_MS.load(Relaxed)) < 400
                && q.sec_session[slot] == q.session
            {
                continue;
            }
            let n = q.sec_next[slot] as usize;
            if n >= 4 * SECTOR_SIZE / CHUNK {
                continue;
            }
            let (idx, off) = (n * CHUNK / SECTOR_SIZE, n * CHUNK % SECTOR_SIZE);
            buf.copy_from_slice(&q.sectors[slot][idx][off..off + CHUNK]);
            q.sec_next[slot] += 1;
            return Some((q.sec_session[slot], idx, off));
        }
        None
    });
    if let Some((s, idx, off)) = chunk {
        log::info!("usb-qual sec: s={s} idx={idx} off={off} hex={}", Hex(&buf));
    }
}

/// The block device the MSC class reads, seen through the recorder. It forwards everything unchanged.
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
            line.push(
                NAMES[i],
                n,
                FIRST[i].load(Relaxed) - 1,
                LAST[i].load(Relaxed) - 1,
            );
        }
    }
    let (written, consumed) = crate::stream::STREAM.progress();
    let resume = crate::stream::STREAM.resume_stats();
    log::info!(
        "usb-qual {tag}: t={}{} written={written} consumed={consumed} resume={}/{}/{}/{}/{} maxidx={} lastsector={} sessions={} dropped={}",
        now() - 1,
        line.as_str(),
        resume.0 as u8,
        resume.1,
        resume.2,
        resume.3,
        resume.4,
        MAX_INDEX.load(Relaxed),
        LAST_SECTOR_MS.load(Relaxed),
        SESSIONS.load(Relaxed),
        QUAL.lock(|c| c.borrow().dropped)
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
            Self {
                buf: [0; 480],
                len: 0,
            }
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
