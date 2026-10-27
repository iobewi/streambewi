//! Diagnostic surface (#15): the recent operating log, sanitized and bounded, and the persisted log policy.
//!
//! `iobewi-log` owns capture and filtering; its ring is a destructive FIFO that drops new records once full,
//! and nobody reads it. A drain loop therefore moves its records into a small history of the application
//! that the HTTP side reads without consuming anything. Nothing here filters: the policy engine stays
//! `iobewi_log::LogPolicy`, persisted through `iobewi_log_config::LogConfig`.
//!
//! Records pass `sanitize` before they are stored, so no secret is ever kept in RAM for the HTTP side:
//! Wi-Fi owner targets are withheld, lines naming a credential are withheld, SSIDs are masked and URLs are
//! reduced to scheme, host and a bounded path.
use alloc::{boxed::Box, string::String, vec, vec::Vec};
use core::cell::RefCell;
use embassy_sync::{
    blocking_mutex::{Mutex as BlockingMutex, raw::NoopRawMutex},
    mutex::Mutex,
};
use embassy_time::{Duration, Timer};
use iobewi_config_space::{Budget, ConfigBackend, ConfigSpace};
use iobewi_log::LogPolicy;
use iobewi_log_config::{LogConfig, POLICY_BYTES_MAX, decode, encode};
use log::Level;

/// ConfigSpace holding the persisted log policy (`iobewi-log-config`).
pub const SPACE: &str = iobewi_log_config::SPACE_NAME;
pub const CONFIG_BUDGET: Budget = Budget::new(POLICY_BYTES_MAX);

/// Lines kept for the HTTP side. The ring of `iobewi-log` holds 24; the history outlives its overflow.
pub const HISTORY_LINES: usize = 32;
/// Bytes kept per line (`target: message`), cut on a character boundary.
pub const LINE_BYTES: usize = 160;
/// Lines returned when the request does not say.
pub const DEFAULT_LINES: usize = 16;
/// JSON around one line (`{"seq":4294967295,"l":"I","m":""},`) and around the whole answer (counters, field
/// names). Large enough for the incremental answer, the longer of the two.
const LINE_ENVELOPE: usize = 34;
const ANSWER_ENVELOPE: usize = 192;
/// Hard upper bound of one `GET /api/logs` answer, whatever the lines contain: control characters are
/// replaced when a line enters the history, so a stored byte costs at most 2 bytes once escaped (`\"`, `\\`).
pub const RESPONSE_MAX: usize = ANSWER_ENVELOPE + HISTORY_LINES * (LINE_ENVELOPE + 2 * LINE_BYTES);
const DRAIN_PERIOD: Duration = Duration::from_millis(400);
/// Longest path kept when a URL is logged.
const PATH_KEPT: usize = 48;
/// Targets owned by the Wi-Fi stack and Improv: their lines carry SSIDs.
const WITHHELD_TARGETS: [&str; 4] = [
    "iobewi_esp_wifi",
    "iobewi_wifi_manager",
    "iobewi_wifi_core",
    "improv",
];
/// A line naming one of these (any case) is not kept.
const WITHHELD_WORDS: [&str; 7] = [
    "password",
    "passwd",
    "token",
    "bearer",
    "secret",
    "authorization",
    "credential",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sanitized {
    Keep(String),
    Withhold,
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// `scheme://host:port/path` without user information, query or fragment; the path is bounded.
fn clean_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let after = &rest[authority_end..];
    let path_end = after.find(['?', '#']).unwrap_or(after.len());
    let (path, cut) = (&after[..path_end], path_end < after.len());
    let mut out = String::new();
    out.push_str(scheme);
    out.push_str("://");
    out.push_str(host);
    let mut kept = 0;
    for c in path.chars() {
        if kept + c.len_utf8() > PATH_KEPT {
            out.push('…');
            break;
        }
        out.push(c);
        kept += c.len_utf8();
    }
    if cut {
        out.push_str("?…");
    }
    out
}

fn redact_urls(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let start = ["http://", "https://"]
            .iter()
            .filter_map(|s| find_ignore_case(rest, s))
            .min();
        let Some(start) = start else { break };
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ',' | ';' | '>'))
            .unwrap_or(tail.len());
        out.push_str(&clean_url(&tail[..end]));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// An SSID may hold spaces: everything after `ssid=` on the line is dropped.
fn redact_ssid(text: &str) -> String {
    match find_ignore_case(text, "ssid=") {
        Some(at) => {
            let value = at + "ssid=".len();
            let mut out = String::from(&text[..value]);
            out.push_str("<redacted>");
            out
        }
        None => String::from(text),
    }
}

/// The text the HTTP side may show for one captured record, or `Withhold`.
pub fn sanitize(target: &str, message: &str) -> Sanitized {
    if WITHHELD_TARGETS.iter().any(|t| target.starts_with(t))
        || WITHHELD_WORDS
            .iter()
            .any(|w| contains_ignore_case(message, w) || contains_ignore_case(target, w))
    {
        return Sanitized::Withhold;
    }
    let mut text = String::new();
    text.push_str(target);
    text.push_str(": ");
    text.push_str(&redact_ssid(&redact_urls(message)));
    Sanitized::Keep(text)
}

#[derive(Clone, Copy)]
struct Entry {
    level: u8,
    len: u8,
    text: [u8; LINE_BYTES],
}

const EMPTY: Entry = Entry {
    level: 0,
    len: 0,
    text: [0; LINE_BYTES],
};

fn level_code(level: Level) -> u8 {
    match level {
        Level::Error => b'E',
        Level::Warn => b'W',
        Level::Info => b'I',
        Level::Debug => b'D',
        Level::Trace => b'T',
    }
}

fn decimal_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 10 {
        v /= 10;
        n += 1;
    }
    n
}

/// Bytes `push_escaped` adds for `text` (same rules as `serde_json`).
fn escaped_len(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        })
        .sum()
}

fn push_escaped(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\u00");
                out.push(HEX[(c as usize) >> 4] as char);
                out.push(HEX[(c as usize) & 15] as char);
            }
            c => out.push(c),
        }
    }
}

/// Fixed ring of sanitized lines: pushing never allocates, the oldest line is overwritten.
pub struct History {
    entries: Box<[Entry]>,
    next: usize,
    stored: usize,
    seen: u32,
    overwritten: u32,
    withheld: u32,
    /// Lines kept since the start (the sanitizer's refusals do not count). The newest line is number `total`
    /// and the `stored` ones go from `total - stored + 1` to `total`: no number is stored per entry.
    total: u32,
    /// Identifies this start of the product; a reader that sees another one starts again.
    boot: u32,
}

impl History {
    pub fn new() -> Self {
        Self {
            entries: vec![EMPTY; HISTORY_LINES].into_boxed_slice(),
            next: 0,
            stored: 0,
            seen: 0,
            overwritten: 0,
            withheld: 0,
            total: 0,
            boot: 0,
        }
    }
    pub fn push(&mut self, level: Level, target: &str, message: &str) {
        self.seen = self.seen.wrapping_add(1);
        let Sanitized::Keep(text) = sanitize(target, message) else {
            self.withheld = self.withheld.wrapping_add(1);
            return;
        };
        let mut end = text.len().min(LINE_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let entry = &mut self.entries[self.next];
        entry.level = level_code(level);
        entry.len = end as u8;
        entry.text[..end].copy_from_slice(&text.as_bytes()[..end]);
        // Control characters never enter the history (a log line is one line): the answer then never needs
        // a `\uXXXX` escape and its size stays under `RESPONSE_MAX`.
        for b in &mut entry.text[..end] {
            if *b < 0x20 || *b == 0x7f {
                *b = b' ';
            }
        }
        self.next = (self.next + 1) % HISTORY_LINES;
        self.total = self.total.wrapping_add(1);
        if self.stored == HISTORY_LINES {
            self.overwritten = self.overwritten.wrapping_add(1);
        } else {
            self.stored += 1;
        }
    }
    /// The newest `n` lines, oldest first.
    pub fn recent(&self, n: usize) -> Vec<(char, String)> {
        let n = n.min(self.stored);
        (0..n)
            .map(|i| {
                let at = (self.next + HISTORY_LINES - n + i) % HISTORY_LINES;
                let e = &self.entries[at];
                let text = core::str::from_utf8(&e.text[..e.len as usize]).unwrap_or("");
                (e.level as char, String::from(text))
            })
            .collect()
    }
    pub fn counters(&self) -> (u32, u32, u32) {
        (self.seen, self.overwritten, self.withheld)
    }

    fn line(&self, from_newest: usize) -> (char, &str) {
        let at = (self.next + HISTORY_LINES - 1 - from_newest) % HISTORY_LINES;
        let e = &self.entries[at];
        (
            e.level as char,
            core::str::from_utf8(&e.text[..e.len as usize]).unwrap_or(""),
        )
    }

    /// The JSON answer of `GET /api/logs` for the newest `n` lines, written into `out` with one exact
    /// reservation: no tree, no copy of the lines, no growth. Lines that would take the answer over
    /// `RESPONSE_MAX` are dropped, oldest first. Allocation failure is reported, never a panic.
    /// Keys are in the alphabetical order `serde_json` used, so the bytes did not change.
    pub fn write_json(
        &self,
        n: usize,
        out: &mut String,
    ) -> Result<(), alloc::collections::TryReserveError> {
        use core::fmt::Write as _;
        let mut count = n.min(self.stored);
        // `{"capacity":32,"lines":[` + `],"overwritten":N,"seen":N,"withheld":N}`
        let fixed = 24
            + 37
            + decimal_len(self.overwritten as u64)
            + decimal_len(self.seen as u64)
            + decimal_len(self.withheld as u64);
        // `{"l":"I","m":"` + text + `"}`; sizes[i] is the i-th newest line.
        let mut sizes = [0usize; HISTORY_LINES];
        for (i, slot) in sizes.iter_mut().enumerate().take(count) {
            *slot = 16 + escaped_len(self.line(i).1);
        }
        let total =
            |count: usize| fixed + sizes[..count].iter().sum::<usize>() + count.saturating_sub(1);
        // Drop the oldest lines while the answer is over the bound (cannot happen once control characters
        // are replaced at ingestion; kept so that the bound does not depend on that invariant).
        while count > 0 && total(count) > RESPONSE_MAX {
            count -= 1;
        }
        let size = total(count);
        out.clear();
        out.try_reserve_exact(size)?;
        write!(out, "{{\"capacity\":{HISTORY_LINES},\"lines\":[").ok();
        for i in (0..count).rev() {
            let (level, text) = self.line(i);
            out.push_str("{\"l\":\"");
            out.push(level);
            out.push_str("\",\"m\":\"");
            push_escaped(out, text);
            out.push_str(if i == 0 { "\"}" } else { "\"}," });
        }
        write!(
            out,
            "],\"overwritten\":{},\"seen\":{},\"withheld\":{}}}",
            self.overwritten, self.seen, self.withheld
        )
        .ok();
        Ok(())
    }
}

/// Counts what is written instead of keeping it: the exact size of an answer before the one reservation.
struct Counter(usize);

impl core::fmt::Write for Counter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// `push_escaped` for any writer (same rules as `serde_json`).
fn write_escaped<W: core::fmt::Write>(out: &mut W, text: &str) -> core::fmt::Result {
    for c in text.chars() {
        match c {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            '\u{8}' => out.write_str("\\b")?,
            '\u{c}' => out.write_str("\\f")?,
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32)?,
            c => out.write_char(c)?,
        }
    }
    Ok(())
}

/// What `History::read_after` found: which lines, and what to tell the reader.
struct After {
    /// Number of the first line to send, and how many.
    first: u32,
    count: usize,
    lost: u32,
    reset: bool,
}

impl History {
    /// Lines `after + 1 ..= total` that are still stored, oldest first, at most `n`.
    fn read_after(&self, after: u32, boot: Option<u32>, n: usize) -> After {
        use core::cmp::min;
        let reset = boot.is_some_and(|b| b != self.boot) || after > self.total;
        let after = if reset { 0 } else { after };
        // `stored <= total`: the oldest stored line, or `total + 1` when there is none.
        let oldest = self.total - self.stored as u32 + 1;
        let wanted = after + 1;
        // A reader that has seen nothing (`after == 0`) is not told that older lines were lost.
        let lost = if after > 0 && wanted < oldest {
            oldest - wanted
        } else {
            0
        };
        let first = wanted.max(oldest);
        let available = if first <= self.total {
            (self.total - first + 1) as usize
        } else {
            0
        };
        After {
            first,
            count: min(available, n),
            lost,
            reset,
        }
    }

    /// Writes the answer of `GET /api/logs?after=` (see `docs/diagnostic-logs.md`) into `out`, in one exact
    /// reservation, like `write_json`. `next` is the number of the last line this answer carries (`after`
    /// when it carries none) and `more` says that lines remain after it: the reader asks again at once.
    #[inline(never)]
    pub fn write_json_after(
        &self,
        after: u32,
        boot: Option<u32>,
        n: usize,
        up: u64,
        out: &mut String,
    ) -> Result<(), alloc::collections::TryReserveError> {
        let found = self.read_after(after, boot, n.clamp(1, HISTORY_LINES));
        let mut count = found.count;
        let after = if found.reset { 0 } else { after };
        let size = loop {
            let mut counter = Counter(0);
            self.emit_after(&mut counter, &found, count, after, up).ok();
            if counter.0 <= RESPONSE_MAX || count == 0 {
                break counter.0;
            }
            count -= 1;
        };
        out.clear();
        out.try_reserve_exact(size)?;
        self.emit_after(out, &found, count, after, up).ok();
        Ok(())
    }

    #[inline(never)]
    fn emit_after<W: core::fmt::Write>(
        &self,
        out: &mut W,
        found: &After,
        count: usize,
        after: u32,
        up: u64,
    ) -> core::fmt::Result {
        write!(out, "{{\"boot\":{},\"lines\":[", self.boot)?;
        for i in 0..count {
            let seq = found.first + i as u32;
            let (level, text) = self.line((self.total - seq) as usize);
            if i > 0 {
                out.write_char(',')?;
            }
            write!(out, "{{\"seq\":{seq},\"l\":\"{level}\",\"m\":\"")?;
            write_escaped(out, text)?;
            out.write_str("\"}")?;
        }
        let next = if count > 0 {
            found.first + count as u32 - 1
        } else {
            after
        };
        write!(
            out,
            "],\"more\":{},\"next\":{next},\"lost\":{},\"reset\":{},\"up\":{up},\"capacity\":{HISTORY_LINES},\"seen\":{},\"overwritten\":{},\"withheld\":{}}}",
            next < self.total,
            found.lost,
            found.reset,
            self.seen,
            self.overwritten,
            self.withheld
        )
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// No stored policy: the legacy fallback of `iobewi-log`.
    Default,
    Persisted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutError {
    /// The policy can only be changed in the AP (setup) boot.
    NotAllowed,
    TooLarge,
    Invalid,
    Storage,
}

struct PolicyState {
    policy: LogPolicy,
    source: Source,
    /// The stored policy was unreadable or invalid at boot: the fallback runs and nothing was overwritten.
    load_error: bool,
}

pub struct Diag<B: ConfigBackend> {
    config: Mutex<NoopRawMutex, LogConfig<B>>,
    state: BlockingMutex<NoopRawMutex, RefCell<Box<PolicyState>>>,
    history: BlockingMutex<NoopRawMutex, RefCell<History>>,
}

impl<B: ConfigBackend> Diag<B> {
    /// Reads the `log` space once. An absent, unreadable or invalid policy keeps the fallback already
    /// installed (and any policy the target applied itself); it is never written back or formatted.
    pub async fn load(space: ConfigSpace<B>, application_target: &str) -> Self {
        let mut state = Box::new(PolicyState {
            policy: iobewi_log::default_policy(application_target)
                .unwrap_or_else(|_| LogPolicy::new(log::LevelFilter::Info)),
            source: Source::Default,
            load_error: false,
        });
        match space.load().await {
            Ok(None) => {}
            Ok(Some(stored)) => match decode(&stored.data) {
                Ok(policy) => {
                    iobewi_log::apply_policy(policy.clone());
                    state.policy = policy;
                    state.source = Source::Persisted;
                }
                Err(_) => {
                    state.load_error = true;
                    log::warn!("log: stored policy invalid, fallback kept");
                }
            },
            Err(_) => {
                state.load_error = true;
                log::warn!("log: stored policy unreadable, fallback kept");
            }
        }
        Self {
            config: Mutex::new(LogConfig::new(space)),
            state: BlockingMutex::new(RefCell::new(state)),
            history: BlockingMutex::new(RefCell::new(History::new())),
        }
    }

    /// The YAML of the active policy, whether it is stored, and whether the stored one was refused.
    pub fn policy_yaml(&self) -> (String, Source, bool) {
        self.state.lock(|s| {
            let s = s.borrow();
            let yaml = encode(&s.policy)
                .map(|y| String::from(y.as_str()))
                .unwrap_or_default();
            (yaml, s.source, s.load_error)
        })
    }

    /// Validate, persist, then apply. A refused body or a failed commit leaves the active policy as it was.
    pub async fn replace_yaml(&self, body: &str, allowed: bool) -> Result<u64, PutError> {
        if !allowed {
            return Err(PutError::NotAllowed);
        }
        if body.len() > POLICY_BYTES_MAX {
            return Err(PutError::TooLarge);
        }
        let policy = decode(body.as_bytes()).map_err(|_| PutError::Invalid)?;
        let generation = self
            .config
            .lock()
            .await
            .replace(policy.clone())
            .await
            .map_err(|_| PutError::Storage)?;
        self.state.lock(|s| {
            let mut state = s.borrow_mut();
            state.policy = policy;
            state.source = Source::Persisted;
            state.load_error = false;
        });
        Ok(generation)
    }

    /// Moves the ring of `iobewi-log` into the history. Called by the drain loop.
    pub fn drain(&self) -> usize {
        let mut moved = 0;
        while let Some(record) = iobewi_log::pop_record() {
            self.history.lock(|h| {
                h.borrow_mut()
                    .push(record.level, &record.target, &record.message)
            });
            moved += 1;
        }
        moved
    }

    /// The answer of `GET /api/logs`, built under one short borrow of the history (no await, no I/O): the
    /// borrow is released before the answer is sent.
    pub fn write_recent(
        &self,
        n: usize,
        out: &mut String,
    ) -> Result<(), alloc::collections::TryReserveError> {
        self.history.lock(|h| h.borrow().write_json(n, out))
    }

    /// Tells the history which start of the product this is. The target draws it at boot (it is not stored
    /// anywhere); 0 until then.
    pub fn set_boot_id(&self, id: u32) {
        self.history.lock(|h| h.borrow_mut().boot = id);
    }

    /// The answer of `GET /api/logs?after=`, built like `write_recent`: one short borrow, no await, no I/O.
    #[inline(never)]
    pub fn write_after(
        &self,
        after: u32,
        boot: Option<u32>,
        n: usize,
        up: u64,
        out: &mut String,
    ) -> Result<(), alloc::collections::TryReserveError> {
        self.history
            .lock(|h| h.borrow().write_json_after(after, boot, n, up, out))
    }

    /// Bounded copy of the newest lines plus the counters (seen, overwritten, withheld).
    pub fn recent(&self, n: usize) -> (Vec<(char, String)>, (u32, u32, u32)) {
        self.history
            .lock(|h| (h.borrow().recent(n), h.borrow().counters()))
    }
}

/// Never returns. Cheap: one short critical section per pass, no network, no flash.
pub async fn drain_loop<B: ConfigBackend>(diag: &Diag<B>) -> ! {
    loop {
        diag.drain();
        Timer::after(DRAIN_PERIOD).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(target: &str, message: &str) -> String {
        match sanitize(target, message) {
            Sanitized::Keep(t) => t,
            Sanitized::Withhold => panic!("withheld: {target} {message}"),
        }
    }

    #[test]
    fn wifi_owners_and_credential_words_are_withheld() {
        assert_eq!(
            sanitize("iobewi_esp_wifi", "Wi-Fi: connection to home failed"),
            Sanitized::Withhold
        );
        assert_eq!(
            sanitize("iobewi_wifi_manager", "wifi: connect failed"),
            Sanitized::Withhold
        );
        assert_eq!(sanitize("streambewi::x", "Bearer abc"), Sanitized::Withhold);
        assert_eq!(
            sanitize("streambewi::x", "PASSWORD set"),
            Sanitized::Withhold
        );
        assert_eq!(
            sanitize("streambewi::x", "wifi: no saved credentials"),
            Sanitized::Withhold
        );
    }

    #[test]
    fn ssid_values_are_masked() {
        assert_eq!(
            kept(
                "streambewi::provisioning",
                "improv: provisioning ssid=My Net x"
            ),
            "streambewi::provisioning: improv: provisioning ssid=<redacted>"
        );
        assert_eq!(
            kept("streambewi::p", "SSID=home net"),
            "streambewi::p: SSID=<redacted>"
        );
    }

    #[test]
    fn urls_lose_credentials_query_and_fragment() {
        assert_eq!(
            kept(
                "streambewi::stream",
                "stream: connecting attempt=1 url=http://user:pw@host.example:8000/live.mp3?key=abc#x end"
            ),
            "streambewi::stream: stream: connecting attempt=1 url=http://host.example:8000/live.mp3?… end"
        );
        assert_eq!(
            kept("t", "http://icecast.radiofrance.fr/fip-midfi.mp3"),
            "t: http://icecast.radiofrance.fr/fip-midfi.mp3"
        );
        let long = alloc::format!("http://h/{}", "a".repeat(100));
        let out = kept("t", &long);
        assert!(out.ends_with('…') && out.len() < 90);
        assert_eq!(kept("t", "no url here"), "t: no url here");
        assert_eq!(kept("t", "(https://h/p?q=1)"), "t: (https://h/p?…)");
    }

    #[test]
    fn history_is_bounded_ordered_and_counts_what_it_drops() {
        let mut h = History::new();
        for i in 0..(HISTORY_LINES + 5) {
            h.push(Level::Info, "t", &alloc::format!("line {i}"));
        }
        h.push(Level::Warn, "iobewi_esp_wifi", "ssid leak");
        let (seen, overwritten, withheld) = h.counters();
        assert_eq!((seen, overwritten, withheld), (38, 5, 1));
        let all = h.recent(1000);
        assert_eq!(all.len(), HISTORY_LINES);
        assert_eq!(all[0].1, "t: line 5");
        assert_eq!(all[HISTORY_LINES - 1].1, "t: line 36");
        let last3 = h.recent(3);
        assert_eq!(
            last3.iter().map(|l| l.1.as_str()).collect::<Vec<_>>(),
            ["t: line 34", "t: line 35", "t: line 36"]
        );
        assert!(h.recent(0).is_empty());
    }

    #[test]
    fn long_lines_are_cut_on_a_character_boundary() {
        let mut h = History::new();
        h.push(Level::Error, "t", &"é".repeat(200));
        let line = &h.recent(1)[0];
        assert_eq!(line.0, 'E');
        assert!(line.1.len() <= LINE_BYTES);
        assert!(line.1.chars().skip(3).all(|c| c == 'é'));
    }

    use core::cell::Cell;
    use iobewi_config_space::{ConfigManager, Snapshot};
    use std::rc::Rc;

    const WARN_DEBUG: &str = "log:\n  default_level: warn\n  targets:\n    streambewi: debug\n";

    #[derive(Clone, Default)]
    struct Backend {
        bytes: Rc<RefCell<Option<Vec<u8>>>>,
        fail: Rc<Cell<bool>>,
        commits: Rc<Cell<u32>>,
    }
    impl ConfigBackend for Backend {
        type Error = ();
        fn capacity_units(&self) -> usize {
            4096
        }
        fn reservation_units(&self, _: &str, budget: Budget) -> Option<usize> {
            Some(budget.max_bytes())
        }
        async fn load(&self, _: &str) -> Result<Option<iobewi_config_space::Snapshot>, ()> {
            if self.fail.get() && self.bytes.borrow().is_none() {
                return Err(());
            }
            Ok(self.bytes.borrow().as_ref().map(|data| Snapshot {
                generation: u64::from(self.commits.get()),
                data: data.clone(),
            }))
        }
        async fn commit(&self, _: &str, data: &[u8]) -> Result<u64, ()> {
            if self.fail.get() {
                return Err(());
            }
            self.commits.set(self.commits.get() + 1);
            *self.bytes.borrow_mut() = Some(data.to_vec());
            Ok(u64::from(self.commits.get()))
        }
        async fn clear(&self, _: &str) -> Result<u64, ()> {
            unreachable!()
        }
    }
    async fn diag(backend: &Backend) -> Diag<Backend> {
        let space = ConfigManager::new(backend.clone())
            .claim(SPACE, CONFIG_BUDGET)
            .unwrap();
        Diag::load(space, "streambewi").await
    }
    fn block<F: core::future::Future>(f: F) -> F::Output {
        embassy_futures::block_on(f)
    }
    fn default_yaml() -> String {
        String::from(
            encode(&iobewi_log::default_policy("streambewi").unwrap())
                .unwrap()
                .as_str(),
        )
    }

    #[test]
    fn absent_policy_keeps_the_default_without_writing() {
        block(async {
            let backend = Backend::default();
            let diag = diag(&backend).await;
            let (yaml, source, load_error) = diag.policy_yaml();
            assert_eq!((source, load_error), (Source::Default, false));
            assert_eq!(yaml, default_yaml());
            assert_eq!(backend.commits.get(), 0);
        });
    }

    #[test]
    fn invalid_or_unreadable_stored_policy_keeps_the_default_and_the_bytes() {
        block(async {
            let backend = Backend::default();
            *backend.bytes.borrow_mut() = Some(b"log:\n  default_level: loud\n".to_vec());
            let diag = diag(&backend).await;
            let (yaml, source, load_error) = diag.policy_yaml();
            assert_eq!((source, load_error), (Source::Default, true));
            assert_eq!(yaml, default_yaml());
            assert_eq!(backend.commits.get(), 0);
            assert_eq!(
                backend.bytes.borrow().as_deref(),
                Some(&b"log:\n  default_level: loud\n"[..])
            );
        });
    }

    #[test]
    fn replace_validates_persists_and_survives_a_reload() {
        block(async {
            let backend = Backend::default();
            let diag = diag(&backend).await;
            assert_eq!(
                diag.replace_yaml(WARN_DEBUG, false).await,
                Err(PutError::NotAllowed)
            );
            assert_eq!(
                diag.replace_yaml("log:\n  default_level: loud\n", true)
                    .await,
                Err(PutError::Invalid)
            );
            assert_eq!(
                diag.replace_yaml(&"x".repeat(POLICY_BYTES_MAX + 1), true)
                    .await,
                Err(PutError::TooLarge)
            );
            assert_eq!(
                (backend.commits.get(), diag.policy_yaml().0),
                (0, default_yaml())
            );
            assert_eq!(diag.replace_yaml(WARN_DEBUG, true).await, Ok(1));
            let (yaml, source, _) = diag.policy_yaml();
            assert_eq!((yaml.as_str(), source), (WARN_DEBUG, Source::Persisted));
            let reloaded = self::diag(&backend).await;
            assert_eq!(reloaded.policy_yaml().0, WARN_DEBUG);
            assert_eq!(reloaded.policy_yaml().1, Source::Persisted);
        });
    }

    #[test]
    fn a_failed_commit_keeps_the_active_policy() {
        block(async {
            let backend = Backend::default();
            let diag = diag(&backend).await;
            assert_eq!(diag.replace_yaml(WARN_DEBUG, true).await, Ok(1));
            backend.fail.set(true);
            assert_eq!(
                diag.replace_yaml("log:\n  default_level: error\n", true)
                    .await,
                Err(PutError::Storage)
            );
            assert_eq!(diag.policy_yaml().0, WARN_DEBUG);
            assert_eq!(backend.commits.get(), 1);
        });
    }

    struct Pages128;
    impl ekv::flash::Flash for Pages128 {
        type Error = core::convert::Infallible;
        fn page_count(&self) -> usize {
            128
        }
        async fn erase(&mut self, _: ekv::flash::PageID) -> Result<(), Self::Error> {
            Ok(())
        }
        async fn read(
            &mut self,
            _: ekv::flash::PageID,
            _: usize,
            _: &mut [u8],
        ) -> Result<(), Self::Error> {
            Ok(())
        }
        async fn write(
            &mut self,
            _: ekv::flash::PageID,
            _: usize,
            _: &[u8],
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    fn spaces_fit_the_config_partition() {
        // Admission arithmetic of the shipped 128-page config partition (OTA layout), with the `log` space.
        let store =
            iobewi_config_space::storage::Store::new(Pages128, 1, crate::boot_policy::SPACES);
        assert!(store.is_ok(), "{:?}", store.err());
        assert!(
            crate::boot_policy::SPACES
                .iter()
                .any(|s| s.name == SPACE && s.max_bytes == POLICY_BYTES_MAX)
        );
    }

    #[test]
    fn drain_moves_the_ring_into_the_history_without_secrets() {
        block(async {
            let backend = Backend::default();
            let diag = diag(&backend).await;
            // The process-wide ring is shared by every test of this binary: only what we add is asserted.
            iobewi_log::discard();
            iobewi_log::install(|_| {}, "streambewi");
            log::info!(target: "streambewi::stream", "stream: connecting url=http://h/live.mp3?key=abc");
            log::warn!(target: "iobewi_esp_wifi", "Wi-Fi: connection to MyNet failed");
            diag.drain();
            // Other tests log through the same ring while this one runs: what is asserted is what this test
            // added (kept once and redacted, withheld never shown), not the exact totals.
            let ours = "streambewi::stream: stream: connecting url=http://h/live.mp3?…";
            let (lines, (seen, _, withheld)) = diag.recent(64);
            assert!(seen >= 2);
            assert!(withheld >= 1);
            assert_eq!(lines.iter().filter(|l| l.1 == ours).count(), 1);
            assert!(lines.iter().all(|l| !l.1.contains("MyNet")));
            diag.drain();
            let (lines, _) = diag.recent(64);
            assert_eq!(lines.iter().filter(|l| l.1 == ours).count(), 1);
        });
    }

    // ---- GET /api/logs answer: contract, escaping, bound and allocations -------------------------------

    mod probe {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;
        thread_local! {
            static ON: Cell<bool> = const { Cell::new(false) };
            static LIVE: Cell<isize> = const { Cell::new(0) };
            static PEAK: Cell<isize> = const { Cell::new(0) };
            static TOTAL: Cell<usize> = const { Cell::new(0) };
            static COUNT: Cell<usize> = const { Cell::new(0) };
        }
        /// Counts the allocations of the measured thread only: the other tests run in parallel.
        pub struct Counting;
        // SAFETY: forwards to `System`; the counters are thread-local plain integers.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let p = unsafe { System.alloc(layout) };
                let _ = ON.try_with(|on| {
                    if on.get() {
                        LIVE.with(|l| l.set(l.get() + layout.size() as isize));
                        PEAK.with(|k| LIVE.with(|l| k.set(k.get().max(l.get()))));
                        TOTAL.with(|t| t.set(t.get() + layout.size()));
                        COUNT.with(|c| c.set(c.get() + 1));
                    }
                });
                p
            }
            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                let _ = ON.try_with(|on| {
                    if on.get() {
                        LIVE.with(|l| l.set(l.get() - layout.size() as isize));
                    }
                });
                unsafe { System.dealloc(ptr, layout) }
            }
            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
                let p = unsafe { System.realloc(ptr, layout, new) };
                let _ = ON.try_with(|on| {
                    if on.get() {
                        LIVE.with(|l| l.set(l.get() + new as isize - layout.size() as isize));
                        PEAK.with(|k| LIVE.with(|l| k.set(k.get().max(l.get()))));
                        TOTAL.with(|t| t.set(t.get() + new));
                        COUNT.with(|c| c.set(c.get() + 1));
                    }
                });
                p
            }
        }
        /// (peak live bytes above the start, bytes requested in total, allocation calls) of `f`.
        pub fn measure<R>(f: impl FnOnce() -> R) -> (R, usize, usize, usize) {
            LIVE.with(|l| l.set(0));
            PEAK.with(|k| k.set(0));
            TOTAL.with(|t| t.set(0));
            COUNT.with(|c| c.set(0));
            ON.with(|on| on.set(true));
            let r = f();
            ON.with(|on| on.set(false));
            (
                r,
                PEAK.with(Cell::get) as usize,
                TOTAL.with(Cell::get),
                COUNT.with(Cell::get),
            )
        }
    }
    #[global_allocator]
    static ALLOCATOR: probe::Counting = probe::Counting;

    /// The answer as it was built before: copies, then a `serde_json::Value` tree, then the string.
    fn old_value_tree<B: ConfigBackend>(diag: &Diag<B>, n: usize) -> String {
        let (lines, (seen, overwritten, withheld)) = diag.recent(n);
        let lines: Vec<_> = lines
            .iter()
            .map(|(level, text)| serde_json::json!({"l": level.to_string(), "m": text}))
            .collect();
        serde_json::json!({
            "lines": lines, "capacity": HISTORY_LINES,
            "seen": seen, "overwritten": overwritten, "withheld": withheld,
        })
        .to_string()
    }
    fn filled(lines: &[(&str, &str)]) -> Diag<Backend> {
        let diag = block(diag(&Backend::default()));
        for (target, message) in lines {
            diag.history
                .lock(|h| h.borrow_mut().push(Level::Info, target, message));
        }
        diag
    }
    fn answer(diag: &Diag<Backend>, n: usize) -> String {
        let mut out = String::new();
        diag.write_recent(n, &mut out).unwrap();
        out
    }

    #[test]
    fn answer_is_byte_identical_to_the_old_tree_for_every_size() {
        let texts: Vec<String> = (0..40)
            .map(|i| match i % 5 {
                0 => alloc::format!("line {i} with \"quotes\" and a \\ backslash"),
                1 => alloc::format!("utf-8 é à ü 日本語 🎵 {i}"),
                2 => alloc::format!("tab\there\nnewline\r\u{1}\u{1f}\u{7f} {i}"),
                3 => "x".repeat(300),
                _ => alloc::format!("plain {i}"),
            })
            .collect();
        for count in [0usize, 1, 2, 16, 31, 32, 33, 40] {
            let input: Vec<(&str, &str)> = texts[..count]
                .iter()
                .map(|t| ("streambewi::t", t.as_str()))
                .collect();
            let diag = filled(&input);
            for n in [1usize, 4, DEFAULT_LINES, HISTORY_LINES] {
                assert_eq!(
                    answer(&diag, n),
                    old_value_tree(&diag, n),
                    "count {count} n {n}"
                );
            }
        }
    }

    #[test]
    fn answer_is_valid_json_with_the_documented_contract() {
        let diag = filled(&[
            ("t", "one"),
            ("t", "two \"x\""),
            ("iobewi_esp_wifi", "ssid leak"),
        ]);
        let value: serde_json::Value = serde_json::from_str(&answer(&diag, 16)).unwrap();
        assert_eq!(value["capacity"], HISTORY_LINES);
        assert_eq!(
            (
                value["seen"].as_u64(),
                value["overwritten"].as_u64(),
                value["withheld"].as_u64()
            ),
            (Some(3), Some(0), Some(1))
        );
        let lines = value["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[0]["l"].as_str(), lines[0]["m"].as_str()),
            (Some("I"), Some("t: one"))
        );
        assert_eq!(lines[1]["m"], "t: two \"x\"");
        assert!(!answer(&diag, 16).contains("leak"));
        let empty = filled(&[]);
        assert_eq!(
            answer(&empty, 16),
            r#"{"capacity":32,"lines":[],"overwritten":0,"seen":0,"withheld":0}"#
        );
    }

    #[test]
    fn escaping_matches_serde_json_for_every_character_class() {
        let mut all = String::new();
        all.extend((0u32..0x80).filter_map(char::from_u32));
        all.push_str("é日本語🎵\u{80}\u{ffff}");
        let mut out = String::new();
        push_escaped(&mut out, &all);
        assert_eq!(out.len(), escaped_len(&all));
        assert_eq!(
            alloc::format!("\"{out}\""),
            serde_json::to_string(&all).unwrap()
        );
        let parsed: String = serde_json::from_str(&alloc::format!("\"{out}\"")).unwrap();
        assert_eq!(parsed, all);
    }

    #[test]
    fn control_characters_never_enter_the_history() {
        let diag = filled(&[("t", "a\u{0}b\u{1b}[31mc\nd\u{7f}e")]);
        let (lines, _) = diag.recent(1);
        assert_eq!(lines[0].1, "t: a b [31mc d e");
    }

    #[test]
    fn worst_case_answer_stays_under_the_bound() {
        // 32 lines of 160 bytes, every byte a character that doubles when escaped.
        let heavy = "\"".repeat(LINE_BYTES);
        let diag = filled(&vec![("", heavy.as_str()); HISTORY_LINES + 5]);
        let out = answer(&diag, HISTORY_LINES);
        assert!(out.len() <= RESPONSE_MAX, "{} > {RESPONSE_MAX}", out.len());
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["lines"].as_array().unwrap().len(), HISTORY_LINES);
        assert_eq!(value["overwritten"], 5);
        // A stored line that could not fit is dropped oldest first, never truncated mid-escape.
        const { assert!(RESPONSE_MAX < 12 * 1024) };
    }

    #[test]
    fn the_policy_answer_stays_small() {
        let diag = block(diag(&Backend::default()));
        let (_, peak, total, calls) = probe::measure(|| {
            let (yaml, source, load_error) = diag.policy_yaml();
            serde_json::json!({"yaml": yaml, "source": if source == Source::Persisted { "persisted" } else { "default" }, "load_error": load_error, "writable": false, "max_bytes": POLICY_BYTES_MAX}).to_string()
        });
        std::println!("GET /api/logs/config: peak {peak} B, total {total} B, {calls} calls");
        assert!(peak < 2048, "{peak}");
    }

    #[test]
    fn the_new_path_allocates_once_and_exactly_where_the_old_one_peaked_high() {
        let heavy = "a \"quoted\" log line with a path /x/y and 日本語 ".repeat(5);
        let diag = filled(&vec![("streambewi::stream", heavy.as_str()); HISTORY_LINES]);
        let (old, old_peak, old_total, old_calls) =
            probe::measure(|| old_value_tree(&diag, HISTORY_LINES));
        let (new, new_peak, new_total, new_calls) = probe::measure(|| answer(&diag, HISTORY_LINES));
        assert_eq!(old, new);
        std::println!(
            "GET /api/logs?n=32, answer {} B: old path peak {old_peak} B, total {old_total} B, {old_calls} calls; new path peak {new_peak} B, total {new_total} B, {new_calls} calls",
            new.len()
        );
        assert_eq!(
            (new_calls, new_peak, new_total),
            (1, new.len(), new.len()),
            "one exact allocation"
        );
        assert!(
            old_peak >= 3 * new_peak,
            "the old path peaked at {old_peak} B, the new one at {new_peak} B"
        );
        assert!(old_calls > 20 * new_calls);
    }
    // ---- GET /api/logs?after=: numbering, incremental reads, losses, restarts, bounds --------------------

    fn numbered(count: usize) -> History {
        let mut h = History::new();
        h.boot = 77;
        for i in 1..=count {
            h.push(Level::Info, "streambewi::t", &format!("line {i}"));
        }
        h
    }
    fn after(h: &History, after: u32, boot: Option<u32>, n: usize) -> serde_json::Value {
        let mut out = String::new();
        h.write_json_after(after, boot, n, 12, &mut out).unwrap();
        assert!(out.len() <= RESPONSE_MAX, "{} > {RESPONSE_MAX}", out.len());
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
    }
    fn seqs(answer: &serde_json::Value) -> Vec<u64> {
        answer["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["seq"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn the_lines_are_numbered_from_one_and_the_answer_says_where_it_stops() {
        let h = numbered(3);
        let a = after(&h, 0, None, 32);
        assert_eq!(seqs(&a), [1, 2, 3]);
        assert_eq!(a["lines"][0]["m"], "streambewi::t: line 1");
        assert_eq!(a["lines"][0]["l"], "I");
        assert_eq!(a["boot"], 77);
        assert_eq!(a["next"], 3);
        assert_eq!(a["more"], false);
        assert_eq!(a["lost"], 0);
        assert_eq!(a["reset"], false);
        assert_eq!(a["up"], 12);
        assert_eq!(a["capacity"], 32);
        assert_eq!(a["seen"], 3);
    }

    #[test]
    fn a_reader_gets_each_line_once() {
        let mut h = numbered(3);
        assert_eq!(seqs(&after(&h, 2, None, 32)), [3]);
        let caught_up = after(&h, 3, None, 32);
        assert!(seqs(&caught_up).is_empty());
        assert_eq!(caught_up["next"], 3, "next stays where the reader is");
        h.push(Level::Warn, "streambewi::t", "line 4");
        let a = after(&h, 3, Some(77), 32);
        assert_eq!(seqs(&a), [4]);
        assert_eq!(a["lines"][0]["l"], "W");
        assert_eq!(a["next"], 4);
    }

    #[test]
    fn identical_lines_have_distinct_numbers() {
        let mut h = History::new();
        for _ in 0..3 {
            h.push(Level::Info, "streambewi::t", "same");
        }
        let a = after(&h, 0, None, 32);
        assert_eq!(seqs(&a), [1, 2, 3]);
        assert_eq!(seqs(&after(&h, 1, None, 32)), [2, 3]);
    }

    #[test]
    fn a_withheld_line_takes_no_number() {
        let mut h = History::new();
        h.push(Level::Info, "streambewi::t", "one");
        h.push(Level::Info, "streambewi::t", "the password is x");
        h.push(Level::Info, "streambewi::t", "two");
        let a = after(&h, 0, None, 32);
        assert_eq!(seqs(&a), [1, 2]);
        assert_eq!(a["withheld"], 1);
        assert_eq!(a["seen"], 3);
    }

    #[test]
    fn the_first_read_does_not_report_old_lines_as_lost_but_a_late_reader_does() {
        let h = numbered(40); // 32 stored: lines 9 to 40
        let first = after(&h, 0, None, 32);
        assert_eq!(seqs(&first).first(), Some(&9));
        assert_eq!(first["lost"], 0, "history already gone is not an incident");
        assert_eq!(first["next"], 40);
        assert_eq!(first["overwritten"], 8);
        // A reader that had line 5 missed 6, 7 and 8.
        let late = after(&h, 5, Some(77), 32);
        assert_eq!(late["lost"], 3);
        assert_eq!(seqs(&late).first(), Some(&9));
        // One that had line 8 missed nothing.
        let exact = after(&h, 8, Some(77), 32);
        assert_eq!(exact["lost"], 0);
        assert_eq!(seqs(&exact).len(), 32);
    }

    #[test]
    fn another_start_of_the_product_makes_the_reader_start_again() {
        let h = numbered(4);
        let other_boot = after(&h, 4, Some(5), 32);
        assert_eq!(other_boot["reset"], true);
        assert_eq!(seqs(&other_boot), [1, 2, 3, 4]);
        assert_eq!(other_boot["lost"], 0);
        // A number the product has not reached: the same, even without a start identifier.
        let ahead = after(&h, 900, None, 32);
        assert_eq!(ahead["reset"], true);
        assert_eq!(seqs(&ahead), [1, 2, 3, 4]);
        assert_eq!(after(&h, 4, Some(77), 32)["reset"], false);
        // A restart whose first lines the reader had already passed: only the identifier tells.
        let restarted = numbered(10);
        let mut restarted = restarted;
        restarted.boot = 78;
        let a = after(&restarted, 4, Some(77), 32);
        assert_eq!(a["reset"], true);
        assert_eq!(seqs(&a).len(), 10);
        assert_eq!(a["boot"], 78);
    }

    #[test]
    fn an_empty_history_and_a_short_n_are_answered_and_the_rest_is_announced() {
        let empty = after(&History::new(), 0, None, 32);
        assert!(seqs(&empty).is_empty());
        assert_eq!(
            (empty["next"].clone(), empty["more"].clone()),
            (0.into(), false.into())
        );

        let h = numbered(5);
        let a = after(&h, 0, None, 2);
        assert_eq!(seqs(&a), [1, 2]);
        assert_eq!(a["next"], 2, "next is the last line delivered");
        assert_eq!(a["more"], true);
        let b = after(&h, 2, None, 2);
        assert_eq!(seqs(&b), [3, 4]);
        let c = after(&h, 4, None, 2);
        assert_eq!((seqs(&c), c["more"].clone()), (vec![5], false.into()));
        // Out of range values are brought back to 1..=32.
        assert_eq!(seqs(&after(&h, 0, None, 0)).len(), 1);
        assert_eq!(seqs(&after(&numbered(40), 0, None, 999)).len(), 32);
    }

    #[test]
    fn the_incremental_answer_stays_under_the_bound_with_the_longest_escaped_lines() {
        let mut h = History::new();
        let quotes = "\"".repeat(LINE_BYTES);
        for _ in 0..40 {
            h.push(Level::Error, "t", &quotes);
        }
        let mut out = String::new();
        h.write_json_after(u32::MAX, Some(u32::MAX), 32, u64::MAX, &mut out)
            .unwrap();
        assert!(out.len() <= RESPONSE_MAX, "{} > {RESPONSE_MAX}", out.len());
        let a: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(seqs(&a).len(), 32);
        const { assert!(RESPONSE_MAX < 12 * 1024) };
    }

    #[test]
    fn the_incremental_answer_is_one_exact_allocation() {
        let h = numbered(32);
        let mut out = String::new();
        h.write_json_after(0, None, 32, 1, &mut out).unwrap();
        assert_eq!(out.capacity(), out.len());
    }

    #[test]
    fn control_characters_and_quotes_are_escaped_like_serde_json() {
        let mut h = History::new();
        h.push(Level::Info, "t", "a \"quoted\" \\ line\tand\u{1}é");
        let mut out = String::new();
        h.write_json_after(0, None, 32, 0, &mut out).unwrap();
        let a: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(a["lines"][0]["m"], "t: a \"quoted\" \\ line and é");
    }
}
