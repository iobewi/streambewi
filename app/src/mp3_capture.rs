//! Diagnostic capture of the first audio sectors of a reopened file (#47, after the recorder of #31).
//! Compiled only with the `mp3-capture` feature.
//!
//! It wraps the virtual disk and looks at the sectors it hands to the MSC class, which sends them unchanged.
//! It writes nothing while a command is being served: the records and the bytes are exported later by the
//! `msc-trace` task, one short line at a time.
//!
//! An episode starts when sector 0 of `RADIO.MP3` is read with a window base that differs from that of the
//! current episode, which is what a rebase does. For the first [`SECTORS`] sectors of the file it keeps the
//! bytes first served, records every later read, and compares each with those bytes.
//!
//! - `mp3c E ep=N t=T base=B replaced=K`: new episode; `replaced` counts the items of the previous one that
//!   were not exported yet and are lost.
//! - `mp3c R ep=N n=I t=T idx=S crc=C same=first|yes|NO`: one read of file sector S.
//! - `mp3c D ep=N idx=S v=first|diff off=O hex=..`: 64 bytes of a sector as first served, or of the first
//!   later read that differed from it.
//! - `mp3c Z ep=N reads=R unrecorded=U differs=D notready=P stored=0xMASK`: everything pending is written.

use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt;

use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::Instant;
use iobewi_fat16::{ReadOnlyBlockDevice, ReadStatus, SECTOR_SIZE};

use streambewi_core::FAT16_CONFIG;

/// File sectors kept and compared, from the start of the file.
pub const SECTORS: usize = 16;
const RECORDS: usize = 64;
const CHUNK: usize = 64;
const CHUNKS: usize = SECTOR_SIZE / CHUNK;
/// The export waits this long after the start of an episode: the host has by then taken or left the file.
pub const EXPORT_AFTER_MS: u64 = 25_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Same {
    First,
    Yes,
    No,
}

#[derive(Clone, Copy, Debug)]
pub struct Record {
    t: u64,
    idx: u8,
    crc: u32,
    same: Same,
}

pub struct Episode {
    number: u32,
    start: u64,
    base: Option<u64>,
    replaced: u32,
    /// On the heap, `SECTORS` long.
    data: Vec<[u8; SECTOR_SIZE]>,
    stored: u16,
    differing: [u8; SECTOR_SIZE],
    differing_idx: Option<u8>,
    /// On the heap, `RECORDS` long.
    records: Vec<Record>,
    len: usize,
    reads: u32,
    differs: u32,
    not_ready: u32,
    header_done: bool,
    records_done: usize,
    chunks_done: [u8; SECTORS],
    differing_done: u8,
    summary: Option<(u32, u32)>,
}

/// A vector of `len` copies of `value`, if the heap has room for it.
fn filled<T: Clone>(value: T, len: usize) -> Option<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).ok()?;
    v.resize(len, value);
    Some(v)
}

impl Episode {
    /// The big buffers are made on the heap one element at a time: none of them ever sits on the stack.
    fn empty() -> Option<Self> {
        let data = filled([0; SECTOR_SIZE], SECTORS)?;
        let records = filled(
            Record {
                t: 0,
                idx: 0,
                crc: 0,
                same: Same::First,
            },
            RECORDS,
        )?;
        Some(Self {
            number: 0,
            start: 0,
            base: None,
            replaced: 0,
            data,
            stored: 0,
            differing: [0; SECTOR_SIZE],
            differing_idx: None,
            records,
            len: 0,
            reads: 0,
            differs: 0,
            not_ready: 0,
            header_done: true,
            records_done: 0,
            chunks_done: [0; SECTORS],
            differing_done: 0,
            summary: None,
        })
    }

    /// Items recorded or stored that the console has not had yet.
    fn pending(&self) -> u32 {
        if self.number == 0 {
            return 0;
        }
        let sectors = (0..SECTORS)
            .filter(|&i| self.stored & (1 << i) != 0 && usize::from(self.chunks_done[i]) < CHUNKS)
            .count();
        let differing = self.differing_idx.is_some() && usize::from(self.differing_done) < CHUNKS;
        (self.len - self.records_done) as u32 + sectors as u32 + u32::from(differing)
    }

    fn restart(&mut self, now: u64, base: Option<u64>) {
        let replaced = self.pending();
        self.number += 1;
        self.start = now;
        self.base = base;
        self.replaced = replaced;
        self.stored = 0;
        self.differing_idx = None;
        self.len = 0;
        self.reads = 0;
        self.differs = 0;
        self.not_ready = 0;
        self.header_done = false;
        self.records_done = 0;
        self.chunks_done = [0; SECTORS];
        self.differing_done = 0;
        self.summary = None;
    }

    /// A sector of the file has been answered with `data`; `base` is the window base after the read.
    pub fn observe(
        &mut self,
        now: u64,
        index: u32,
        status: ReadStatus,
        data: &[u8; SECTOR_SIZE],
        base: Option<u64>,
    ) {
        let Ok(i) = usize::try_from(index) else {
            return;
        };
        if i >= SECTORS {
            return;
        }
        let ready = status == ReadStatus::Ready;
        if i == 0 && ready && (self.number == 0 || base != self.base) {
            self.restart(now, base);
        }
        if self.number == 0 || base != self.base {
            return;
        }
        if !ready {
            // Retried every few milliseconds by the MSC class: counted, not recorded.
            self.not_ready = self.not_ready.saturating_add(1);
            return;
        }
        let same = if self.stored & (1 << i) == 0 {
            self.stored |= 1 << i;
            self.data[i] = *data;
            Same::First
        } else if self.data[i] == *data {
            Same::Yes
        } else {
            self.differs += 1;
            if self.differing_idx.is_none() {
                self.differing_idx = Some(i as u8);
                self.differing = *data;
            }
            Same::No
        };
        self.reads += 1;
        if self.len < RECORDS {
            self.records[self.len] = Record {
                t: now,
                idx: i as u8,
                crc: crc32(data),
                same,
            };
            self.len += 1;
        }
    }

    /// The next line of the export, if one is due.
    pub fn next_line(&mut self, now: u64) -> Option<Line> {
        if self.number == 0 || now.saturating_sub(self.start) < EXPORT_AFTER_MS {
            return None;
        }
        let ep = self.number;
        if !self.header_done {
            self.header_done = true;
            return Some(Line::Episode(ep, self.start, self.base, self.replaced));
        }
        if self.records_done < self.len {
            let n = self.records_done;
            self.records_done += 1;
            return Some(Line::Read(ep, n, self.records[n]));
        }
        for i in 0..SECTORS {
            let done = usize::from(self.chunks_done[i]);
            if self.stored & (1 << i) != 0 && done < CHUNKS {
                self.chunks_done[i] += 1;
                return Some(Line::data(ep, i as u8, "first", done, &self.data[i]));
            }
        }
        if let Some(idx) = self.differing_idx {
            let done = usize::from(self.differing_done);
            if done < CHUNKS {
                self.differing_done += 1;
                return Some(Line::data(ep, idx, "diff", done, &self.differing));
            }
        }
        let state = (self.reads, self.not_ready);
        if self.summary != Some(state) {
            self.summary = Some(state);
            return Some(Line::Summary {
                ep,
                reads: self.reads,
                unrecorded: self.reads - self.len as u32,
                differs: self.differs,
                not_ready: self.not_ready,
                stored: self.stored,
            });
        }
        None
    }
}

pub enum Line {
    Episode(u32, u64, Option<u64>, u32),
    Read(u32, usize, Record),
    Data {
        ep: u32,
        idx: u8,
        version: &'static str,
        offset: usize,
        bytes: [u8; CHUNK],
    },
    Summary {
        ep: u32,
        reads: u32,
        unrecorded: u32,
        differs: u32,
        not_ready: u32,
        stored: u16,
    },
}

impl Line {
    fn data(ep: u32, idx: u8, version: &'static str, chunk: usize, s: &[u8; SECTOR_SIZE]) -> Self {
        let mut bytes = [0u8; CHUNK];
        bytes.copy_from_slice(&s[chunk * CHUNK..(chunk + 1) * CHUNK]);
        Line::Data {
            ep,
            idx,
            version,
            offset: chunk * CHUNK,
            bytes,
        }
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Line::Episode(ep, t, base, replaced) => {
                write!(f, "mp3c E ep={ep} t={t} base={base:?} replaced={replaced}")
            }
            Line::Read(ep, n, r) => write!(
                f,
                "mp3c R ep={ep} n={n} t={} idx={} crc={:08x} same={}",
                r.t,
                r.idx,
                r.crc,
                match r.same {
                    Same::First => "first",
                    Same::Yes => "yes",
                    Same::No => "NO",
                }
            ),
            Line::Data {
                ep,
                idx,
                version,
                offset,
                bytes,
            } => {
                write!(f, "mp3c D ep={ep} idx={idx} v={version} off={offset} hex=")?;
                for byte in bytes {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
            Line::Summary {
                ep,
                reads,
                unrecorded,
                differs,
                not_ready,
                stored,
            } => write!(
                f,
                "mp3c Z ep={ep} reads={reads} unrecorded={unrecorded} differs={differs} notready={not_ready} stored=0x{stored:04x}"
            ),
        }
    }
}

/// CRC-32 (IEEE, as `zlib.crc32`), to check the exported bytes against what was recorded at the read.
pub fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |crc, &byte| {
        (0..8).fold(crc ^ u32::from(byte), |c, _| {
            (c >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(c & 1))
        })
    })
}

/// The buffers are on the heap: the stack reservation has only a few KiB to spare above its minimum.
static CAPTURE: Mutex<CriticalSectionRawMutex, RefCell<Option<Episode>>> =
    Mutex::new(RefCell::new(None));

/// Allocates the buffers. Without them the product runs as usual and nothing is captured.
fn allocate() {
    let Some(episode) = Episode::empty() else {
        log::warn!("mp3c: no memory for the capture");
        return;
    };
    CAPTURE.lock(|c| *c.borrow_mut() = Some(episode));
    log::info!(
        "mp3c: capture ready, {} bytes",
        SECTORS * SECTOR_SIZE + RECORDS * core::mem::size_of::<Record>()
    );
}

pub(crate) fn next_line() -> Option<Line> {
    let now = Instant::now().as_millis();
    CAPTURE.lock(|c| c.borrow_mut().as_mut().and_then(|e| e.next_line(now)))
}

/// The virtual disk, observed. Every call and every byte is that of the inner disk.
pub struct CaptureDisk<D, B> {
    inner: D,
    base: B,
}

impl<D: ReadOnlyBlockDevice, B: Fn() -> Option<u64>> CaptureDisk<D, B> {
    pub fn new(inner: D, base: B) -> Self {
        allocate();
        Self { inner, base }
    }
}

impl<D: ReadOnlyBlockDevice, B: Fn() -> Option<u64>> ReadOnlyBlockDevice for CaptureDisk<D, B> {
    fn last_lba(&self) -> u32 {
        self.inner.last_lba()
    }

    fn begin_session(&mut self) {
        self.inner.begin_session();
    }

    fn end_session(&mut self) {
        self.inner.end_session();
    }

    fn read_sector(&mut self, lba: u32, out: &mut [u8; SECTOR_SIZE]) -> ReadStatus {
        let status = self.inner.read_sector(lba, out);
        let data_start = FAT16_CONFIG.geometry().map(|g| g.data_start_lba);
        if let Ok(start) = data_start
            && let Some(index) = lba.checked_sub(start)
            && (index as usize) < SECTORS
        {
            let base = (self.base)();
            let now = Instant::now().as_millis();
            CAPTURE.lock(|c| {
                if let Some(episode) = c.borrow_mut().as_mut() {
                    episode.observe(now, index, status, out, base);
                }
            });
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(e: &mut Episode, now: u64) -> std::vec::Vec<std::string::String> {
        std::iter::from_fn(|| e.next_line(now))
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn crc_matches_zlib() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn a_new_base_on_sector_zero_starts_an_episode_and_rereads_are_compared() {
        let mut e = Episode::empty().unwrap();
        let (a, b) = ([0xAA; SECTOR_SIZE], [0xBB; SECTOR_SIZE]);
        // Nothing before sector 0 has been read.
        e.observe(10, 3, ReadStatus::Ready, &a, Some(100));
        assert!(export(&mut e, 100_000).is_empty());
        e.observe(1_000, 0, ReadStatus::Ready, &a, Some(100));
        e.observe(1_010, 0, ReadStatus::Ready, &a, Some(100));
        // A later read of sector 0 with the same base is a reread, not a new episode.
        e.observe(21_000, 0, ReadStatus::Ready, &b, Some(100));
        e.observe(21_001, 1, ReadStatus::Pending, &a, Some(100));
        e.observe(21_002, 99, ReadStatus::Ready, &a, Some(100));
        assert!(export(&mut e, 1_000 + EXPORT_AFTER_MS - 1).is_empty());
        let out = export(&mut e, 1_000 + EXPORT_AFTER_MS);
        assert_eq!(out[0], "mp3c E ep=1 t=1000 base=Some(100) replaced=0");
        assert!(out[1].ends_with("same=first"), "{out:?}");
        assert!(out[2].ends_with("same=yes"));
        assert!(out[3].ends_with("same=NO"));
        assert_eq!(
            out.iter().filter(|l| l.contains(" v=first ")).count(),
            CHUNKS
        );
        assert_eq!(
            out.iter().filter(|l| l.contains(" v=diff ")).count(),
            CHUNKS
        );
        assert_eq!(
            out.last().unwrap(),
            "mp3c Z ep=1 reads=3 unrecorded=0 differs=1 notready=1 stored=0x0001"
        );
        assert!(export(&mut e, 200_000).is_empty());
    }

    #[test]
    fn a_rebase_before_the_export_says_what_it_replaced() {
        let mut e = Episode::empty().unwrap();
        let a = [1; SECTOR_SIZE];
        e.observe(0, 0, ReadStatus::Ready, &a, Some(1));
        e.observe(1, 0, ReadStatus::Ready, &a, Some(2));
        let out = export(&mut e, 100_000);
        assert_eq!(out[0], "mp3c E ep=2 t=1 base=Some(2) replaced=2");
    }

    #[test]
    fn the_records_are_bounded_and_the_rest_is_counted() {
        let mut e = Episode::empty().unwrap();
        let a = [1; SECTOR_SIZE];
        for t in 0..RECORDS as u64 + 5 {
            e.observe(t, 0, ReadStatus::Ready, &a, Some(1));
        }
        let out = export(&mut e, 100_000);
        assert!(
            out.last()
                .unwrap()
                .contains("reads=69 unrecorded=5 differs=0")
        );
    }
}
