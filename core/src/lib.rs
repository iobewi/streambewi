#![cfg_attr(not(test), no_std)]

//! StreamBeWI product policy on top of the IOBEWI USB radio framework: the 1 GiB virtual
//! `RADIO.MP3` geometry, the live-stream window and the far-ahead probe policy. FAT layout,
//! rolling window and USB MSC themselves live in `iobewi-fat16`, `iobewi-rolling-stream` and
//! `iobewi-usb-msc`.

pub use iobewi_fat16::{Fat16Config, Geometry};
use iobewi_fat16::{FileSource, ReadStatus, SECTOR_SIZE};
use iobewi_rolling_stream::{ReadStatus as WindowStatus, RollingStream};

/// Rolling window capacity in RAM.
pub const RING_CAPACITY: usize = 96 * 1024;
/// History kept behind a new USB session's origin, and the amount buffered before the USB
/// disk is presented.
pub const PREBUFFER_BYTES: u64 = 64 * 1024;
/// Producer lead over the consumer (backpressure).
pub const MAX_LEAD_BYTES: u64 = 80 * 1024;

/// A read starting more than this many bytes past the live edge of the stream is answered at
/// once with zeroes instead of waiting. The Metronic reads just behind the live edge; hosts such
/// as Windows probe arbitrary offsets (e.g. the tail of the 1 GiB file, ~18 h in the future),
/// which would otherwise block the whole MSC until the stream got there.
pub const FAR_AHEAD_BYTES: u64 = 1024 * 1024;

/// FAT16 with 32 KiB clusters and a 1 GiB virtual RADIO.MP3. The medium is virtual; only the
/// rolling stream window exists in RAM.
pub const FAT16_CONFIG: Fat16Config = Fat16Config {
    sectors_per_cluster: 64,
    file_cluster_count: 32_768,
    root_entries: 32,
    file_name: *b"RADIO   MP3",
    volume_label: *b"RADIOUSB   ",
    volume_serial: 0x5241_4449, // "RADI"
};

pub type Stream = RollingStream<RING_CAPACITY>;

pub const fn new_stream() -> Stream {
    RollingStream::new(PREBUFFER_BYTES, MAX_LEAD_BYTES)
}

/// True once enough data is buffered to present the USB disk.
pub fn is_ready(stream: &Stream) -> bool {
    stream.written() >= PREBUFFER_BYTES
}

/// `(written, consumed)` absolute positions, for progress logs.
pub fn progress(stream: &Stream) -> (u64, u64) {
    (stream.written(), stream.session().map_or(0, |s| s.read_end))
}

/// Whether a read of absolute stream bytes starting at `start` is a far-ahead probe.
pub fn is_far_ahead(start: u64, written: u64) -> bool {
    start > written.saturating_add(FAR_AHEAD_BYTES)
}

/// Sectors of the virtual file (1 GiB).
const FILE_SECTORS: u32 = (1024 * 1024 * 1024 / SECTOR_SIZE) as u32;
/// The first sectors of the file (4 KiB) are what a host reads to recognise it; a read there is never a resume.
pub const HEAD_SECTORS: u32 = 8;
/// A first audio read must lie at least this far past the live edge (the size of the window) to be a resume:
/// closer, it is the player pacing at the live edge, and waiting is right.
pub const REBASE_AHEAD_BYTES: u64 = RING_CAPACITY as u64;
/// Last sectors of the file (1 MiB): where Windows probes the tail. Never a resume.
pub const TAIL_GUARD_SECTORS: u32 = 2048;

/// What a host that resumes at a remembered file offset needs (#31), and the counters that tell what happened.
///
/// A live stream cannot serve a position that lies in its future, and a position that lies in its past is gone.
/// A host that reopens the disk and asks for its remembered offset first therefore finds nothing, whatever the
/// disk answers (waiting makes it reset the bus, zeroes make it give up). With `enabled`, the *first* audio read of
/// an MSC session fixes the origin of the session once: the offset the host asked for is bound to the live edge
/// (minus the retained history), exactly as an enumeration binds offset 0, and the audio that follows is real.
///
/// - **New session**: `begin_session` of the file (MSC connected, or the host reopening the file at sector 0).
/// - **Trigger**: the first read of a sector `>= HEAD_SECTORS` in the session, if it lies at least
///   `REBASE_AHEAD_BYTES` past the live edge and not in the last `TAIL_GUARD_SECTORS` of the file.
/// - **Refused** (nothing changes, `refused` counts it): the read is in the tail guard; or the origin is already
///   fixed by an earlier audio read of the session (a later far read is a seek, answered by the unchanged
///   policy: wait if near, zeroes if beyond `FAR_AHEAD_BYTES`); or the read is not far (pacing, or inside the window).
/// - **Sectors before the origin** (`offset < shift`) are `Expired`: zeroes, reported as such, never presented as
///   audio. A read of sector 0 there is the host reopening the file: the origin returns to sector 0.
/// - **Later reads** use the same offset-to-stream mapping until the window moves past them (then `Expired`).
#[derive(Clone, Copy, Debug)]
pub struct Resume {
    enabled: bool,
    /// File offset (bytes) bound to the session origin; 0 when the session was not rebased.
    shift: u64,
    /// The origin is fixed: by a rebase or by a first audio read that did not need one.
    committed: bool,
    /// Rebases done since boot, refusals since boot, and the sector of the last rebase.
    pub rebases: u32,
    pub refused: u32,
    pub last_index: u32,
}

impl Resume {
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            shift: 0,
            committed: false,
            rebases: 0,
            refused: 0,
            last_index: 0,
        }
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    /// File offset bound to the origin of the current session.
    pub fn shift(&self) -> u64 {
        self.shift
    }
    fn new_session(&mut self) {
        self.shift = 0;
        self.committed = false;
    }
}

/// Serves the virtual file from the live stream window, applying the far-ahead policy.
pub struct StreamFile<'a> {
    stream: &'a mut Stream,
    resume: Option<&'a mut Resume>,
}

impl<'a> StreamFile<'a> {
    pub fn new(stream: &'a mut Stream) -> Self {
        Self {
            stream,
            resume: None,
        }
    }

    /// As `new`, with the resume policy of the session (`Resume::new(false)` changes nothing).
    pub fn with_resume(stream: &'a mut Stream, resume: &'a mut Resume) -> Self {
        Self {
            stream,
            resume: Some(resume),
        }
    }
}

impl FileSource for StreamFile<'_> {
    fn begin_session(&mut self) {
        self.stream.begin_session();
        if let Some(r) = self.resume.as_deref_mut() {
            r.new_session();
        }
    }

    fn end_session(&mut self) {
        self.stream.end_session();
    }

    fn read_file_sector(&mut self, index: u32, out: &mut [u8; SECTOR_SIZE]) -> ReadStatus {
        let mut offset = index as u64 * SECTOR_SIZE as u64;
        if let Some(r) = self.resume.as_deref_mut().filter(|r| r.enabled) {
            if !r.committed
                && index >= HEAD_SECTORS
                && let Some(session) = self.stream.session()
            {
                // The first audio read of the session decides, once, where its origin is.
                let ahead = session
                    .base
                    .saturating_add(offset)
                    .saturating_sub(self.stream.written());
                if ahead >= REBASE_AHEAD_BYTES && index < FILE_SECTORS - TAIL_GUARD_SECTORS {
                    self.stream.begin_session();
                    r.shift = offset;
                    r.rebases += 1;
                    r.last_index = index;
                } else if ahead >= REBASE_AHEAD_BYTES {
                    r.refused += 1;
                }
                r.committed = true;
            }
            if r.shift > 0 {
                if offset < r.shift {
                    if index == 0 {
                        // The host reopened the file: the origin goes back to sector 0, as at an enumeration.
                        self.stream.begin_session();
                        r.new_session();
                    } else {
                        out.fill(0);
                        return ReadStatus::Expired;
                    }
                } else {
                    offset -= r.shift;
                }
            }
        }
        if let Some(session) = self.stream.session()
            && is_far_ahead(session.base.saturating_add(offset), self.stream.written())
        {
            // Probe far beyond the live edge (e.g. Windows reading the file tail):
            // answer with zeroes now instead of blocking the MSC for hours.
            out.fill(0);
            return ReadStatus::Ready;
        }
        let mut status = self.stream.read(offset, out);
        // Reading the first sector of the file means the player (re)opened it (e.g. stop, then
        // play on the Metronic). By then the head of the session has left the window, and
        // answering zeroes makes the player give up. Position the session exactly as an
        // enumeration does (`begin_session`: retain behind the live edge) and read again.
        // Window, prebuffer and lead are unchanged; any other expired sector is still answered
        // as expired.
        if status == WindowStatus::Expired && index == 0 {
            self.stream.begin_session();
            if let Some(r) = self.resume.as_deref_mut() {
                r.new_session();
            }
            status = self.stream.read(offset, out);
        }
        match status {
            WindowStatus::Ready => ReadStatus::Ready,
            WindowStatus::Pending => ReadStatus::Pending,
            WindowStatus::Expired => ReadStatus::Expired,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iobewi_fat16::VirtualFat16;

    #[test]
    fn geometry_is_the_p3_one_gib_fat16() {
        let g = FAT16_CONFIG.geometry().unwrap();
        assert_eq!(g.file_size, 1024 * 1024 * 1024);
        assert_eq!(g.total_sectors, g.data_start_lba + 32_768 * 64);
        assert_eq!(g.fat_sectors, 129);
        assert_eq!(g.data_start_lba, 1 + 2 * 129 + 2);
    }

    fn filled(n: usize) -> Stream {
        let mut s = new_stream();
        let data: [u8; 1024] = core::array::from_fn(|i| i as u8);
        let mut left = n;
        while left > 0 {
            let k = left.min(1024);
            assert_eq!(s.push(&data[..k]), k);
            left -= k;
        }
        s
    }

    #[test]
    fn readiness_follows_prebuffer() {
        assert!(!is_ready(&filled(PREBUFFER_BYTES as usize - 1)));
        assert!(is_ready(&filled(PREBUFFER_BYTES as usize)));
    }

    #[test]
    fn session_rebases_behind_live_edge() {
        let mut s = filled(80 * 1024);
        let mut file = StreamFile::new(&mut s);
        file.begin_session();
        let mut sector = [0u8; SECTOR_SIZE];
        // base = live - 64 KiB = 16 KiB; byte 16 KiB of the stream is (16384 % 256)=0 pattern.
        assert_eq!(file.read_file_sector(0, &mut sector), ReadStatus::Ready);
        assert_eq!(sector[1], 1);
    }

    #[test]
    fn read_classification_matches_the_reference() {
        let mut s = filled(80 * 1024);
        let mut file = StreamFile::new(&mut s);
        file.begin_session();
        let mut sector = [0u8; SECTOR_SIZE];
        // Just past the live edge (the Metronic pacing case): wait.
        assert_eq!(file.read_file_sector(128, &mut sector), ReadStatus::Pending);
        // Windows tail probe of the 1 GiB file: answered at once with zeroes.
        let tail = FAT16_CONFIG.geometry().unwrap().file_size / SECTOR_SIZE as u32 - 1;
        sector.fill(0xAA);
        assert_eq!(file.read_file_sector(tail, &mut sector), ReadStatus::Ready);
        assert!(sector.iter().all(|b| *b == 0));
    }

    #[test]
    fn reopening_the_file_rebases_instead_of_serving_zeroes() {
        let mut s = filled(80 * 1024);
        let mut file = StreamFile::new(&mut s);
        file.begin_session();
        let base = file.stream.session().unwrap().base;
        // A player consuming sequentially lets the live edge run far past the 96 KiB window.
        let data: [u8; 1024] = core::array::from_fn(|i| (i as u8) | 1);
        let mut sector = [0u8; SECTOR_SIZE];
        let mut pos = 0u32;
        while file.stream.written() < base + 300 * 1024 {
            while file.read_file_sector(pos, &mut sector) == ReadStatus::Ready {
                pos += 1;
            }
            let n = file.stream.writable().min(1024);
            file.stream.push(&data[..n]);
        }
        // Any other expired sector is still answered as Expired (zeroes): no hidden rebase.
        assert_eq!(file.read_file_sector(5, &mut sector), ReadStatus::Expired);
        assert_eq!(file.stream.session().unwrap().base, base);
        // Reopening the file (first sector) rebases on the live edge and serves valid data.
        sector.fill(0);
        assert_eq!(file.read_file_sector(0, &mut sector), ReadStatus::Ready);
        assert!(sector.iter().all(|b| *b != 0));
        assert!(file.stream.session().unwrap().base > base);
        assert_eq!(file.read_file_sector(1, &mut sector), ReadStatus::Ready);
    }

    #[test]
    fn no_session_is_pending() {
        let mut s = filled(80 * 1024);
        let mut file = StreamFile::new(&mut s);
        let mut sector = [0u8; SECTOR_SIZE];
        assert_eq!(file.read_file_sector(0, &mut sector), ReadStatus::Pending);
    }

    #[test]
    fn volume_builds_from_the_product_config() {
        let mut s = new_stream();
        let mut vol = VirtualFat16::new(StreamFile::new(&mut s), FAT16_CONFIG).unwrap();
        let mut sector = [0u8; SECTOR_SIZE];
        assert_eq!(vol.read_sector(0, &mut sector), ReadStatus::Ready);
        assert_eq!(&sector[510..512], &[0x55, 0xAA]);
    }

    // ---- the resume policy (#31) ----

    /// A stream whose byte at absolute position `p` is `(p / 512) as u8`: a sector read says where it came from.
    fn tagged(n: usize) -> Stream {
        let mut s = new_stream();
        let mut pos = 0usize;
        while pos < n {
            let k = 1024.min(n - pos);
            let chunk: Vec<u8> = (pos..pos + k).map(|p| (p / 512) as u8).collect();
            assert_eq!(s.push(&chunk), k);
            pos += k;
        }
        s
    }

    fn read(file: &mut StreamFile<'_>, index: u32) -> (ReadStatus, [u8; SECTOR_SIZE]) {
        let mut sector = [0xAAu8; SECTOR_SIZE];
        let status = file.read_file_sector(index, &mut sector);
        (status, sector)
    }

    /// The tag of the stream sector `index` of the file maps to, given the session base.
    fn tag(base: u64, delta: u64) -> u8 {
        ((base + delta * SECTOR_SIZE as u64) / 512) as u8
    }

    #[test]
    fn resume_off_changes_nothing() {
        for enabled in [None, Some(false)] {
            let mut s = tagged(200 * 1024);
            let mut r = Resume::new(false);
            let mut file = match enabled {
                None => StreamFile::new(&mut s),
                Some(_) => StreamFile::with_resume(&mut s, &mut r),
            };
            file.begin_session();
            assert_eq!(read(&mut file, 0).0, ReadStatus::Ready);
            // 800 KiB past the edge: waits, as before; 2 MiB past: zeroes at once, as before.
            assert_eq!(read(&mut file, 1688).0, ReadStatus::Pending);
            let (status, sector) = read(&mut file, 4089);
            assert_eq!(status, ReadStatus::Ready);
            assert!(sector.iter().all(|b| *b == 0));
        }
        assert_eq!(Resume::new(false).rebases, 0);
    }

    #[test]
    fn the_first_far_audio_read_fixes_the_origin_once_and_serves_real_audio() {
        let mut s = tagged(200 * 1024);
        let mut r = Resume::new(true);
        let mut file = StreamFile::with_resume(&mut s, &mut r);
        file.begin_session();
        let base = file.stream.session().unwrap().base;
        // The header probe: sectors 0 to 4, served from the session origin as always.
        for i in 0..5 {
            let (status, sector) = read(&mut file, i);
            assert_eq!(status, ReadStatus::Ready);
            assert_eq!(sector[0], tag(base, i as u64));
        }
        // The remembered position, 800 KiB past the edge: the origin is bound to the live edge, real audio comes.
        let (status, sector) = read(&mut file, 1688);
        assert_eq!(status, ReadStatus::Ready);
        let origin = file.stream.session().unwrap().base;
        assert_eq!(origin, file.stream.written() - PREBUFFER_BYTES);
        assert_eq!(sector[0], tag(origin, 0));
        assert!(
            sector.iter().all(|b| *b == sector[0]),
            "a whole sector of the stream, not a mix"
        );
        // Following sectors are the following stream sectors; a re-read answers the same bytes.
        for i in 1..4 {
            let (status, sector) = read(&mut file, 1688 + i);
            assert_eq!(status, ReadStatus::Ready);
            assert_eq!(sector[0], tag(origin, i as u64));
        }
        assert_eq!(read(&mut file, 1688).1[0], tag(origin, 0));
        // At the live edge the player paces as usual.
        let to_edge = (PREBUFFER_BYTES / SECTOR_SIZE as u64) as u32;
        assert_eq!(read(&mut file, 1688 + to_edge).0, ReadStatus::Pending);
        assert_eq!(r.rebases, 1);
        assert_eq!(r.last_index, 1688);
        assert_eq!(r.shift(), 1688 * 512);
    }

    #[test]
    fn a_multi_sector_command_that_straddles_the_origin_is_answered_sector_by_sector() {
        let mut s = tagged(200 * 1024);
        let mut r = Resume::new(true);
        let mut file = StreamFile::with_resume(&mut s, &mut r);
        file.begin_session();
        assert_eq!(read(&mut file, 1688).0, ReadStatus::Ready);
        let origin = file.stream.session().unwrap().base;
        // Sectors 1686..=1689 as one READ(10) would ask them: two before the origin, two after.
        assert_eq!(read(&mut file, 1686).0, ReadStatus::Expired);
        assert_eq!(read(&mut file, 1687).0, ReadStatus::Expired);
        assert_eq!(read(&mut file, 1688).1[0], tag(origin, 0));
        assert_eq!(read(&mut file, 1689).1[0], tag(origin, 1));
    }

    #[test]
    fn sectors_before_the_origin_are_expired_never_audio_and_sector_zero_reopens_the_file() {
        let mut s = tagged(200 * 1024);
        let mut r = Resume::new(true);
        let mut file = StreamFile::with_resume(&mut s, &mut r);
        file.begin_session();
        assert_eq!(read(&mut file, 1688).0, ReadStatus::Ready);
        let (status, sector) = read(&mut file, 100);
        assert_eq!(status, ReadStatus::Expired);
        assert!(sector.iter().all(|b| *b == 0));
        // Sector 0: the host reopened the file; the origin returns to sector 0, as at an enumeration.
        let (status, sector) = read(&mut file, 0);
        assert_eq!(status, ReadStatus::Ready);
        let origin = file.stream.session().unwrap().base;
        assert_eq!(sector[0], tag(origin, 0));
        assert_eq!(
            file.stream.session().unwrap().base,
            file.stream.written() - PREBUFFER_BYTES
        );
        assert_eq!(read(&mut file, 1).1[0], tag(origin, 1));
        assert_eq!(r.shift(), 0);
    }

    #[test]
    fn later_far_reads_are_seeks_answered_by_the_unchanged_policy() {
        let mut s = tagged(200 * 1024);
        let mut r = Resume::new(true);
        let mut file = StreamFile::with_resume(&mut s, &mut r);
        file.begin_session();
        assert_eq!(read(&mut file, 1688).0, ReadStatus::Ready);
        let origin = file.stream.session().unwrap().base;
        // 400 sectors past the edge: waits; 2 MiB past: zeroes at once. Neither moves the origin.
        assert_eq!(read(&mut file, 1688 + 128 + 400).0, ReadStatus::Pending);
        let (status, sector) = read(&mut file, 1688 + 128 + 5000);
        assert_eq!(status, ReadStatus::Ready);
        assert!(sector.iter().all(|b| *b == 0));
        assert_eq!(file.stream.session().unwrap().base, origin);
        assert_eq!(r.rebases, 1);
    }

    #[test]
    fn an_ordinary_audio_read_fixes_the_origin_and_a_later_far_read_does_not_rebase() {
        let mut s = tagged(200 * 1024);
        let mut r = Resume::new(true);
        let mut file = StreamFile::with_resume(&mut s, &mut r);
        file.begin_session();
        let base = file.stream.session().unwrap().base;
        for i in 0..5 {
            assert_eq!(read(&mut file, i).0, ReadStatus::Ready);
        }
        assert_eq!(read(&mut file, 8).1[0], tag(base, 8));
        // The origin is fixed by that read: a seek far ahead later waits, it does not rebase.
        assert_eq!(read(&mut file, 1688).0, ReadStatus::Pending);
        assert_eq!(r.rebases, 0);
        assert_eq!(r.shift(), 0);
    }

    #[test]
    fn pacing_at_the_live_edge_and_the_file_tail_are_not_resumes() {
        let mut s = tagged(200 * 1024);
        let mut r = Resume::new(true);
        let mut file = StreamFile::with_resume(&mut s, &mut r);
        file.begin_session();
        // Just past the edge (64 KiB of history behind the origin): the player waits for the stream.
        assert_eq!(read(&mut file, 128).0, ReadStatus::Pending);
        assert_eq!(r.rebases, 0);
        // A new session whose first audio read is the file tail (Windows): zeroes at once, refused, no rebase.
        let mut file = StreamFile::with_resume(&mut s, &mut r);
        file.begin_session();
        let tail = FILE_SECTORS - 1;
        let (status, sector) = read(&mut file, tail);
        assert_eq!(status, ReadStatus::Ready);
        assert!(sector.iter().all(|b| *b == 0));
        assert_eq!((r.rebases, r.refused), (0, 1));
        assert_eq!(r.shift(), 0);
    }

    #[test]
    fn each_session_has_its_own_origin() {
        let mut s = tagged(200 * 1024);
        let mut r = Resume::new(true);
        let mut file = StreamFile::with_resume(&mut s, &mut r);
        file.begin_session();
        assert_eq!(read(&mut file, 1688).0, ReadStatus::Ready);
        file.end_session();
        // The host reconnects and asks for another remembered position: a new origin, not the old shift.
        file.begin_session();
        assert_eq!(r.shift(), 0);
        let mut file = StreamFile::with_resume(&mut s, &mut r);
        file.begin_session();
        assert_eq!(read(&mut file, 3000).0, ReadStatus::Ready);
        assert_eq!(r.rebases, 2);
        assert_eq!(r.shift(), 3000 * 512);
    }
}
