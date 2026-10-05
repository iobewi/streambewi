#![cfg_attr(not(test), no_std)]

//! StreamBeWI product policy on top of the IOBEWI USB radio framework: the 1 GiB virtual
//! `RADIO.MP3` geometry, the live-stream window and the far-ahead probe policy. FAT layout,
//! rolling window and USB MSC themselves live in `iobewi-fat16`, `iobewi-rolling-stream` and
//! `iobewi-usb-msc`.

use iobewi_fat16::{FileSource, ReadStatus, SECTOR_SIZE};
pub use iobewi_fat16::{Fat16Config, Geometry};
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

/// Serves the virtual file from the live stream window, applying the far-ahead policy.
pub struct StreamFile<'a> {
    stream: &'a mut Stream,
}

impl<'a> StreamFile<'a> {
    pub fn new(stream: &'a mut Stream) -> Self {
        Self { stream }
    }
}

impl FileSource for StreamFile<'_> {
    fn begin_session(&mut self) {
        self.stream.begin_session();
    }

    fn end_session(&mut self) {
        self.stream.end_session();
    }

    fn read_file_sector(&mut self, index: u32, out: &mut [u8; SECTOR_SIZE]) -> ReadStatus {
        let offset = index as u64 * SECTOR_SIZE as u64;
        if let Some(session) = self.stream.session() {
            if is_far_ahead(session.base.saturating_add(offset), self.stream.written()) {
                // Probe far beyond the live edge (e.g. Windows reading the file tail):
                // answer with zeroes now instead of blocking the MSC for hours.
                out.fill(0);
                return ReadStatus::Ready;
            }
        }
        match self.stream.read(offset, out) {
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
}
