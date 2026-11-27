//! Diagnostic trace of the MSC activity of a host (#47). Compiled only with the `msc-trace` feature.
//!
//! It observes the bulk endpoints through a wrapper of the USB driver: every command block wrapper (CBW)
//! the host sends and every command status wrapper (CSW) the device returns. It changes neither the bytes
//! nor the order of the transfers. Commands are aggregated so that the console stays readable: one line per
//! run of identical commands (or of contiguous READ(10)) in each window of [`WINDOW_MS`].
//!
//! Lines (all times are milliseconds since boot, on the device clock):
//!
//! - `msct R t=A-B n=N lba=X..Y blk=MIN/MAX gap=G svc=S st=K`: N contiguous READ(10), first command at A,
//!   last at B, sectors X to Y included, G the longest silence before one of these READ(10) (counted from
//!   the previous READ(10), so a pause of the player appears on the first line after it), S the longest
//!   time from a command to its status.
//! - `msct C op=0xNN cdb=.. t=A-B n=N gap=G svc=S st=K`: N identical commands other than READ(10).
//!   START STOP UNIT and PREVENT ALLOW MEDIUM REMOVAL are never aggregated.
//! - `msct B t=A <event>`: USB bus event.
//! - `msct S t=A live=L consumed=C open=.. lost=N lines=N tx_us=T txmax_us=M`: once per window; `open` is
//!   a command that has no status yet (opcode, first sector, age), `lost` the lines dropped because the
//!   queue was full. `lines`, `tx_us` and `txmax_us` are the cost of the trace itself: the `msct` lines
//!   written since the previous `S` line (that one included), the total and the longest time spent in the
//!   blocking write, during which the executor serves nothing else, the USB included.
//!
//! `st` is the CSW status (0 passed, 1 failed) or `abort` when the next command arrived without a status.

use core::cell::RefCell;
use core::fmt;

use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Instant, Timer};
use embassy_usb::driver::{
    Driver, Endpoint, EndpointAddress, EndpointAllocError, EndpointError, EndpointIn, EndpointInfo,
    EndpointOut, EndpointType,
};
use heapless::{Deque, Vec};

const READ_10: u8 = 0x28;
const START_STOP_UNIT: u8 = 0x1B;
const PREVENT_ALLOW: u8 = 0x1E;
const ABORTED: u8 = 0xFF;

/// Aggregation window.
pub const WINDOW_MS: u64 = 1_000;
const POLL_MS: u64 = 100;
const QUEUE: usize = 64;
const OPEN: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Command {
    pub cdb: [u8; 16],
    pub cdb_len: u8,
    pub start: u64,
}

impl Command {
    /// A command block wrapper, if `packet` is one.
    fn parse(packet: &[u8], now: u64) -> Option<Self> {
        if packet.len() != 31 || packet[..4] != *b"USBC" || packet[14] == 0 || packet[14] > 16 {
            return None;
        }
        let mut cdb = [0u8; 16];
        cdb.copy_from_slice(&packet[15..31]);
        Some(Self {
            cdb,
            cdb_len: packet[14],
            start: now,
        })
    }

    fn op(&self) -> u8 {
        self.cdb[0]
    }

    fn lba(&self) -> u32 {
        u32::from_be_bytes([self.cdb[2], self.cdb[3], self.cdb[4], self.cdb[5]])
    }

    fn blocks(&self) -> u32 {
        u16::from_be_bytes([self.cdb[7], self.cdb[8]]) as u32
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Run {
    command: Command,
    first: u64,
    last: u64,
    count: u32,
    lba: u32,
    next_lba: u32,
    min_blocks: u32,
    max_blocks: u32,
    gap: u64,
    service: u64,
    status: u8,
}

impl Run {
    fn new(command: Command, gap: u64, service: u64, status: u8) -> Self {
        let (lba, blocks) = (command.lba(), command.blocks());
        Self {
            command,
            first: command.start,
            last: command.start,
            count: 1,
            lba,
            next_lba: lba.wrapping_add(blocks),
            min_blocks: blocks,
            max_blocks: blocks,
            gap,
            service,
            status,
        }
    }

    /// The same stream of commands: contiguous READ(10), or the very same command block.
    fn continues(&self, command: &Command, status: u8) -> bool {
        if self.status != status || self.command.op() != command.op() {
            return false;
        }
        match command.op() {
            READ_10 => command.lba() == self.next_lba,
            START_STOP_UNIT | PREVENT_ALLOW => false,
            _ => self.command.cdb == command.cdb,
        }
    }

    /// One run per READ(10) stream, one per distinct other command.
    fn same_slot(&self, command: &Command) -> bool {
        self.command.op() == command.op()
            && (command.op() == READ_10 || self.command.cdb == command.cdb)
    }

    fn extend(&mut self, command: &Command, gap: u64, service: u64) {
        let blocks = command.blocks();
        self.last = command.start;
        self.count = self.count.saturating_add(1);
        self.next_lba = command.lba().wrapping_add(blocks);
        self.min_blocks = self.min_blocks.min(blocks);
        self.max_blocks = self.max_blocks.max(blocks);
        self.gap = self.gap.max(gap);
        self.service = self.service.max(service);
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Line {
    Run(Run),
    Bus(u64, &'static str, u32),
}

struct Status(u8);

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == ABORTED {
            f.write_str("abort")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Line::Bus(t, event, value) => write!(f, "msct B t={t} {event}={value}"),
            Line::Run(r) if r.command.op() == READ_10 => write!(
                f,
                "msct R t={}-{} n={} lba={}..{} blk={}/{} gap={} svc={} st={}",
                r.first,
                r.last,
                r.count,
                r.lba,
                r.next_lba.wrapping_sub(1),
                r.min_blocks,
                r.max_blocks,
                r.gap,
                r.service,
                Status(r.status)
            ),
            Line::Run(r) => {
                write!(f, "msct C op=0x{:02x} cdb=", r.command.op())?;
                for byte in &r.command.cdb[..r.command.cdb_len as usize] {
                    write!(f, "{byte:02x}")?;
                }
                write!(
                    f,
                    " t={}-{} n={} gap={} svc={} st={}",
                    r.first,
                    r.last,
                    r.count,
                    r.gap,
                    r.service,
                    Status(r.status)
                )
            }
        }
    }
}

/// The aggregation, independent of the clock and of the console.
pub struct Trace {
    inflight: Option<(Command, u64)>,
    open: Vec<Run, OPEN>,
    queue: Deque<Line, QUEUE>,
    lost: u32,
    last_read: Option<u64>,
    last_other: Option<u64>,
}

impl Trace {
    pub const fn new() -> Self {
        Self {
            inflight: None,
            open: Vec::new(),
            queue: Deque::new(),
            lost: 0,
            last_read: None,
            last_other: None,
        }
    }

    fn emit(&mut self, line: Line) {
        if self.queue.push_back(line).is_err() {
            self.lost = self.lost.saturating_add(1);
        }
    }

    /// A packet received on the bulk OUT endpoint.
    pub fn received(&mut self, packet: &[u8], now: u64) {
        let Some(command) = Command::parse(packet, now) else {
            return;
        };
        if let Some((previous, gap)) = self.inflight.take() {
            self.complete(previous, gap, now, ABORTED);
        }
        let last = if command.op() == READ_10 {
            &mut self.last_read
        } else {
            &mut self.last_other
        };
        let gap = last.map_or(0, |t| now.saturating_sub(t));
        *last = Some(now);
        self.inflight = Some((command, gap));
    }

    /// A packet the device has sent on the bulk IN endpoint.
    pub fn sent(&mut self, packet: &[u8], now: u64) {
        if packet.len() == 13
            && packet[..4] == *b"USBS"
            && let Some((command, gap)) = self.inflight.take()
        {
            self.complete(command, gap, now, packet[12]);
        }
    }

    fn complete(&mut self, command: Command, gap: u64, now: u64, status: u8) {
        let service = now.saturating_sub(command.start);
        if let Some(i) = self.open.iter().position(|r| r.same_slot(&command)) {
            if self.open[i].continues(&command, status) {
                self.open[i].extend(&command, gap, service);
                return;
            }
            let ended = self.open.remove(i);
            self.emit(Line::Run(ended));
        }
        let run = Run::new(command, gap, service, status);
        if matches!(command.op(), START_STOP_UNIT | PREVENT_ALLOW) {
            // Written at once, after whatever was open, so that its place in the sequence is readable.
            self.flush();
            self.emit(Line::Run(run));
            return;
        }
        if self.open.is_full() {
            let oldest = self.open.remove(0);
            self.emit(Line::Run(oldest));
        }
        // Cannot fail: a place was just made.
        let _ = self.open.push(run);
    }

    pub fn bus(&mut self, now: u64, event: &'static str, value: u32) {
        self.flush();
        self.emit(Line::Bus(now, event, value));
    }

    /// Ends the runs in progress, in the order of their first command.
    pub fn flush(&mut self) {
        while let Some(i) = (0..self.open.len()).min_by_key(|&i| self.open[i].first) {
            let run = self.open.remove(i);
            self.emit(Line::Run(run));
        }
    }

    pub fn pop(&mut self) -> Option<Line> {
        self.queue.pop_front()
    }

    /// The command that has no status yet: opcode, first sector, age.
    pub fn pending(&self, now: u64) -> Option<(u8, u32, u64)> {
        self.inflight.map(|(c, _)| {
            let lba = if c.op() == READ_10 { c.lba() } else { 0 };
            (c.op(), lba, now.saturating_sub(c.start))
        })
    }

    pub fn lost(&self) -> u32 {
        self.lost
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

static TRACE: Mutex<CriticalSectionRawMutex, RefCell<Trace>> =
    Mutex::new(RefCell::new(Trace::new()));

fn with<R>(f: impl FnOnce(&mut Trace) -> R) -> R {
    TRACE.lock(|t| f(&mut t.borrow_mut()))
}

fn now() -> u64 {
    Instant::now().as_millis()
}

pub(crate) fn bus(event: &'static str, value: u32) {
    with(|t| t.bus(now(), event, value));
}

/// The time the trace spends writing its own lines.
#[derive(Default)]
struct Cost {
    lines: u32,
    total_us: u64,
    max_us: u64,
}

impl Cost {
    fn add(&mut self, us: u64) {
        self.lines = self.lines.saturating_add(1);
        self.total_us = self.total_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }

    /// Runs `write` and counts the time it took.
    fn measure(&mut self, write: impl FnOnce()) {
        let before = Instant::now();
        write();
        self.add(before.elapsed().as_micros());
    }
}

/// Writes the lines on the operating log. `progress` gives the bytes received and consumed of the stream.
/// The console write blocks: lines are written one at a time, between two polls.
pub(crate) async fn run(progress: impl Fn() -> (u64, u64)) -> ! {
    let mut window = now();
    let mut cost = Cost::default();
    #[cfg(feature = "mp3-capture")]
    let mut turn = false;
    loop {
        Timer::after_millis(POLL_MS).await;
        let t = now();
        let tick = t.saturating_sub(window) >= WINDOW_MS;
        if tick {
            window = t;
            with(|trace| trace.flush());
        }
        while let Some(line) = with(|trace| trace.pop()) {
            cost.measure(|| log::info!("{line}"));
        }
        // At most one line of the audio capture every other poll: its cost is part of `tx_us`.
        #[cfg(feature = "mp3-capture")]
        {
            turn = !turn;
            if turn && let Some(line) = crate::mp3_capture::next_line() {
                cost.measure(|| log::info!("{line}"));
            }
        }
        if tick {
            let (live, consumed) = progress();
            let (pending, lost) = with(|trace| (trace.pending(t), trace.lost()));
            let Cost {
                lines,
                total_us,
                max_us,
            } = core::mem::take(&mut cost);
            let open = Open(pending);
            cost.measure(|| {
                log::info!(
                    "msct S t={t} live={live} consumed={consumed} open={open} lost={lost} lines={lines} tx_us={total_us} txmax_us={max_us}"
                )
            });
        }
    }
}

/// The command in flight, as written on the `S` line.
struct Open(Option<(u8, u32, u64)>);

impl fmt::Display for Open {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some((op, lba, age)) => write!(f, "0x{op:02x}@{lba}+{age}ms"),
            None => f.write_str("none"),
        }
    }
}

/// A USB driver (or one of its bulk endpoints) whose MSC transfers are observed.
pub struct Traced<T>(pub T);

impl<'a, D: Driver<'a>> Driver<'a> for Traced<D> {
    type EndpointOut = Traced<D::EndpointOut>;
    type EndpointIn = Traced<D::EndpointIn>;
    type ControlPipe = D::ControlPipe;
    type Bus = D::Bus;

    fn alloc_endpoint_out(
        &mut self,
        ep_type: EndpointType,
        ep_addr: Option<EndpointAddress>,
        max_packet_size: u16,
        interval_ms: u8,
    ) -> Result<Self::EndpointOut, EndpointAllocError> {
        self.0
            .alloc_endpoint_out(ep_type, ep_addr, max_packet_size, interval_ms)
            .map(Traced)
    }

    fn alloc_endpoint_in(
        &mut self,
        ep_type: EndpointType,
        ep_addr: Option<EndpointAddress>,
        max_packet_size: u16,
        interval_ms: u8,
    ) -> Result<Self::EndpointIn, EndpointAllocError> {
        self.0
            .alloc_endpoint_in(ep_type, ep_addr, max_packet_size, interval_ms)
            .map(Traced)
    }

    fn start(self, control_max_packet_size: u16) -> (Self::Bus, Self::ControlPipe) {
        self.0.start(control_max_packet_size)
    }
}

impl<E: Endpoint> Endpoint for Traced<E> {
    fn info(&self) -> &EndpointInfo {
        self.0.info()
    }

    async fn wait_enabled(&mut self) {
        self.0.wait_enabled().await
    }
}

impl<E: EndpointOut> EndpointOut for Traced<E> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, EndpointError> {
        let n = self.0.read(buf).await?;
        let t = now();
        with(|trace| trace.received(&buf[..n], t));
        Ok(n)
    }
}

impl<E: EndpointIn> EndpointIn for Traced<E> {
    async fn write(&mut self, buf: &[u8]) -> Result<(), EndpointError> {
        self.0.write(buf).await?;
        if buf.len() == 13 {
            let t = now();
            with(|trace| trace.sent(buf, t));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cbw(cdb: &[u8]) -> [u8; 31] {
        let mut packet = [0u8; 31];
        packet[..4].copy_from_slice(b"USBC");
        packet[14] = cdb.len() as u8;
        packet[15..15 + cdb.len()].copy_from_slice(cdb);
        packet
    }

    fn read10(lba: u32, blocks: u16) -> [u8; 31] {
        let mut cdb = [0u8; 10];
        cdb[0] = READ_10;
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
        cbw(&cdb)
    }

    fn csw(status: u8) -> [u8; 13] {
        let mut packet = [0u8; 13];
        packet[..4].copy_from_slice(b"USBS");
        packet[12] = status;
        packet
    }

    fn command(trace: &mut Trace, packet: &[u8], start: u64, end: u64) {
        trace.received(packet, start);
        trace.sent(&csw(0), end);
    }

    fn lines(trace: &mut Trace) -> std::vec::Vec<std::string::String> {
        std::iter::from_fn(|| trace.pop())
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn contiguous_reads_make_one_line_even_with_polls_between_them() {
        let mut trace = Trace::new();
        let ready = cbw(&[0; 6]);
        command(&mut trace, &read10(261, 8), 100, 102);
        command(&mut trace, &ready, 150, 151);
        command(&mut trace, &read10(269, 8), 300, 309);
        command(&mut trace, &ready, 350, 351);
        trace.flush();
        assert_eq!(
            lines(&mut trace),
            [
                "msct R t=100-300 n=2 lba=261..276 blk=8/8 gap=200 svc=9 st=0",
                "msct C op=0x00 cdb=000000000000 t=150-350 n=2 gap=200 svc=1 st=0",
            ]
        );
    }

    #[test]
    fn a_silence_is_carried_by_the_first_read_after_it_across_windows() {
        let mut trace = Trace::new();
        command(&mut trace, &read10(261, 8), 1_000, 1_001);
        trace.flush();
        command(&mut trace, &read10(269, 8), 21_000, 21_001);
        trace.flush();
        assert!(lines(&mut trace)[1].contains(" gap=20000 "));
    }

    #[test]
    fn a_jump_ends_the_run() {
        let mut trace = Trace::new();
        command(&mut trace, &read10(261, 8), 0, 1);
        command(&mut trace, &read10(261, 1), 10, 11);
        trace.flush();
        let out = lines(&mut trace);
        assert_eq!(out.len(), 2);
        assert!(out[1].contains("lba=261..261"));
    }

    #[test]
    fn start_stop_unit_is_never_aggregated_and_keeps_its_place() {
        let mut trace = Trace::new();
        let stop = cbw(&[START_STOP_UNIT, 0, 0, 0, 0x02, 0]);
        command(&mut trace, &read10(261, 8), 0, 1);
        command(&mut trace, &stop, 10, 11);
        command(&mut trace, &stop, 20, 21);
        let out = lines(&mut trace);
        assert_eq!(out.len(), 3);
        assert!(out[0].starts_with("msct R "));
        assert!(out[1].starts_with("msct C op=0x1b cdb=1b0000000200 t=10-10 n=1"));
    }

    #[test]
    fn a_command_without_status_is_pending_then_aborted() {
        let mut trace = Trace::new();
        trace.received(&read10(500, 8), 100);
        assert_eq!(trace.pending(5_100), Some((READ_10, 500, 5_000)));
        command(&mut trace, &read10(500, 8), 6_000, 6_001);
        trace.flush();
        let out = lines(&mut trace);
        assert!(out[0].ends_with("st=abort"), "{out:?}");
        assert!(out[1].ends_with("st=0"), "{out:?}");
    }

    #[test]
    fn data_packets_and_a_status_without_command_are_ignored() {
        let mut trace = Trace::new();
        trace.received(&[0u8; 31], 0);
        trace.sent(&csw(0), 1);
        trace.sent(&[0u8; 64], 2);
        trace.bus(3, "suspended", 1);
        assert_eq!(lines(&mut trace), ["msct B t=3 suspended=1"]);
    }

    #[test]
    fn the_cost_keeps_the_total_and_the_longest_write() {
        let mut cost = Cost::default();
        for us in [900, 12_000, 700] {
            cost.add(us);
        }
        assert_eq!(
            (cost.lines, cost.total_us, cost.max_us),
            (3, 13_600, 12_000)
        );
        assert_eq!(Open(None).to_string(), "none");
        assert_eq!(Open(Some((READ_10, 500, 40))).to_string(), "0x28@500+40ms");
    }

    #[test]
    fn a_full_queue_counts_what_it_drops() {
        let mut trace = Trace::new();
        for t in 0..QUEUE as u64 + 3 {
            trace.bus(t, "reset", 1);
        }
        assert_eq!(trace.lost(), 3);
    }
}
