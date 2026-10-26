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
use embassy_time::{Duration, Instant, Timer};
use iobewi_config_space::{Budget, ConfigBackend, ConfigSpace};
use iobewi_log::LogPolicy;
use iobewi_log_config::{LogConfig, POLICY_BYTES_MAX, decode, encode};
use log::Level;

/// ConfigSpace holding the persisted log policy (`iobewi-log-config`).
pub const SPACE: &str = iobewi_log_config::SPACE_NAME;
pub const CONFIG_BUDGET: Budget = Budget::new(POLICY_BYTES_MAX);

/// How long the temporary detailed diagnostic lasts, from its activation. Not renewable.
pub const DEBUG_SECONDS: u64 = 900;
/// The targets raised to `debug` while the detailed diagnostic is on: the product's own, never the Wi-Fi
/// stack's or a third party's, and never `trace`.
pub const DEBUG_TARGETS: [&str; 4] = [
    "streambewi::stream",
    "streambewi::usb",
    "streambewi::status",
    "streambewi::provisioning",
];

/// Lines kept for the HTTP side. The ring of `iobewi-log` holds 24; the history outlives its overflow.
pub const HISTORY_LINES: usize = 32;
/// Bytes kept per line (`target: message`), cut on a character boundary.
pub const LINE_BYTES: usize = 160;
/// Lines returned when the request does not say.
pub const DEFAULT_LINES: usize = 16;
/// JSON around one line (`{"l":"I","m":""},`) and around the whole answer (counters, field names).
const LINE_ENVELOPE: usize = 17;
const ANSWER_ENVELOPE: usize = 128;
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
        use core::fmt::Write;
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

/// The temporary detailed diagnostic. It lives in RAM only: nothing about it is ever stored.
#[derive(Default)]
struct DebugState {
    /// When it ends; `None` when it is off.
    until: Option<Instant>,
    /// The targets actually raised (fewer than `DEBUG_TARGETS` when the policy has no room for them all).
    targets: Vec<&'static str>,
}

/// What `/api/debug` says of the temporary detailed diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugStatus {
    pub enabled: bool,
    pub remaining_seconds: u64,
    /// The targets raised to `debug` (empty when off).
    pub targets: Vec<&'static str>,
    /// Some of `DEBUG_TARGETS` could not be raised: a policy holds at most 8 rules.
    pub partial: bool,
}

/// `base` with the debug targets raised to `debug`: the rules of `base` are kept (the ones for a debug
/// target are raised), then the debug targets that are missing are added as long as there is room. Built on
/// the heap: a policy is about 600 bytes.
fn debug_policy(base: &LogPolicy) -> (Box<LogPolicy>, Vec<&'static str>) {
    let mut policy = Box::new(LogPolicy::new(base.default_level));
    let mut raised = Vec::new();
    for rule in base.targets() {
        let debug_target = DEBUG_TARGETS
            .iter()
            .find(|target| **target == rule.target.as_str());
        let level = match debug_target {
            Some(target) => {
                raised.push(*target);
                log::LevelFilter::Debug
            }
            None => rule.level,
        };
        // The rules of `base` are distinct and at most 8: they fit.
        let _ = policy.add_target(rule.target.as_str(), level);
    }
    for target in DEBUG_TARGETS {
        if !raised.contains(&target) && policy.add_target(target, log::LevelFilter::Debug).is_ok() {
            raised.push(target);
        }
    }
    (policy, raised)
}

pub struct Diag<B: ConfigBackend> {
    config: Mutex<NoopRawMutex, LogConfig<B>>,
    /// `policy` is the **base** policy: what applies when no detailed diagnostic is on.
    state: BlockingMutex<NoopRawMutex, RefCell<Box<PolicyState>>>,
    debug: BlockingMutex<NoopRawMutex, RefCell<DebugState>>,
    /// Applies a policy to the logger. `iobewi_log::apply_policy`; the tests record what is applied instead.
    apply: fn(LogPolicy),
    history: BlockingMutex<NoopRawMutex, RefCell<History>>,
}

impl<B: ConfigBackend> Diag<B> {
    /// Reads the `log` space once. An absent, unreadable or invalid policy keeps the fallback already
    /// installed (and any policy the target applied itself); it is never written back or formatted.
    pub async fn load(space: ConfigSpace<B>, application_target: &str) -> Self {
        let base = iobewi_log::default_policy(application_target)
            .unwrap_or_else(|_| LogPolicy::new(log::LevelFilter::Info));
        Self::load_with(space, base).await
    }

    /// As `load`, with `applied` as the policy the target already applied (a qualification image adds its
    /// own targets): it is the base that applies outside the detailed diagnostic when nothing is stored.
    pub async fn load_with(space: ConfigSpace<B>, applied: LogPolicy) -> Self {
        let mut state = Box::new(PolicyState {
            policy: applied,
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
            debug: BlockingMutex::new(RefCell::new(DebugState::default())),
            apply: iobewi_log::apply_policy,
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
        // `LogConfig::replace` applied the new base as it is: a detailed diagnostic still on goes back over it.
        self.debug_reapply();
        Ok(generation)
    }

    /// Where the detailed diagnostic stands at `now`.
    pub fn debug_status(&self, now: Instant) -> DebugStatus {
        self.debug.lock(|d| {
            let d = d.borrow();
            match d.until {
                Some(until) if until > now => DebugStatus {
                    enabled: true,
                    remaining_seconds: (until - now).as_millis().div_ceil(1000),
                    targets: d.targets.clone(),
                    partial: d.targets.len() < DEBUG_TARGETS.len(),
                },
                _ => DebugStatus {
                    enabled: false,
                    remaining_seconds: 0,
                    targets: Vec::new(),
                    partial: false,
                },
            }
        })
    }

    /// Turns the detailed diagnostic on for `DEBUG_SECONDS` from `now`. While one is on, this changes
    /// nothing: the end is not moved. Nothing is written anywhere.
    pub fn debug_enable(&self, now: Instant) -> DebugStatus {
        let started = self.debug.lock(|d| {
            let mut d = d.borrow_mut();
            if d.until.is_some_and(|until| until > now) {
                return false;
            }
            let (policy, raised) = self.state.lock(|s| debug_policy(&s.borrow().policy));
            (self.apply)(*policy);
            d.targets = raised;
            d.until = Some(now + Duration::from_secs(DEBUG_SECONDS));
            true
        });
        if started {
            log::info!("diag: detailed diagnostic on for {} s", DEBUG_SECONDS);
        }
        self.debug_status(now)
    }

    /// Turns it off at once and puts the base policy back. Nothing happens when it is off already.
    pub fn debug_disable(&self) {
        self.debug_end("disabled");
    }

    /// Ends the detailed diagnostic when its time is up. Called by the drain loop.
    pub fn debug_tick(&self, now: Instant) {
        let over = self
            .debug
            .lock(|d| d.borrow().until.is_some_and(|until| until <= now));
        if over {
            self.debug_end("expired");
        }
    }

    fn debug_end(&self, why: &str) {
        let was_on = self.debug.lock(|d| {
            let mut d = d.borrow_mut();
            let was_on = d.until.take().is_some();
            if was_on {
                d.targets.clear();
                let base = self.state.lock(|s| s.borrow().policy.clone());
                (self.apply)(base);
            }
            was_on
        });
        if was_on {
            log::info!("diag: detailed diagnostic off ({})", why);
        }
    }

    /// The base policy changed under a detailed diagnostic that is on: raise the targets over the new one.
    fn debug_reapply(&self) {
        self.debug.lock(|d| {
            let mut d = d.borrow_mut();
            if d.until.is_some() {
                let (policy, raised) = self.state.lock(|s| debug_policy(&s.borrow().policy));
                (self.apply)(*policy);
                d.targets = raised;
            }
        });
    }

    /// Tests only: where the policies are applied.
    #[cfg(test)]
    pub(crate) fn set_applier(&mut self, apply: fn(LogPolicy)) {
        self.apply = apply;
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

    /// Bounded copy of the newest lines plus the counters (seen, overwritten, withheld).
    pub fn recent(&self, n: usize) -> (Vec<(char, String)>, (u32, u32, u32)) {
        self.history
            .lock(|h| (h.borrow().recent(n), h.borrow().counters()))
    }
}

/// Never returns. Cheap: one short critical section per pass, no network, no flash. It also ends the
/// detailed diagnostic when its time is up, so that nothing else needs a timer.
pub async fn drain_loop<B: ConfigBackend>(diag: &Diag<B>) -> ! {
    loop {
        diag.drain();
        diag.debug_tick(Instant::now());
        Timer::after(DRAIN_PERIOD).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embassy_time::Instant;

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

    struct Pages64;
    impl ekv::flash::Flash for Pages64 {
        type Error = core::convert::Infallible;
        fn page_count(&self) -> usize {
            64
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
        // Admission arithmetic of the shipped 64-page config partition, with the `log` space.
        let store =
            iobewi_config_space::storage::Store::new(Pages64, 1, crate::boot_policy::SPACES);
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

    // --- The temporary detailed diagnostic -------------------------------------------------------

    std::thread_local! {
        static APPLIED: RefCell<Vec<LogPolicy>> = const { RefCell::new(Vec::new()) };
    }
    fn record(policy: LogPolicy) {
        APPLIED.with(|a| a.borrow_mut().push(policy));
    }
    fn last_applied() -> LogPolicy {
        APPLIED.with(|a| a.borrow().last().cloned().expect("a policy was applied"))
    }
    fn applied_count() -> usize {
        APPLIED.with(|a| a.borrow().len())
    }
    /// A `Diag` that records the policies it applies (each test runs on its own thread).
    async fn recording(backend: &Backend, base: LogPolicy) -> Diag<Backend> {
        let space = ConfigManager::new(backend.clone())
            .claim(SPACE, CONFIG_BUDGET)
            .unwrap();
        let mut diag = Diag::load_with(space, base).await;
        diag.set_applier(record);
        APPLIED.with(|a| a.borrow_mut().clear());
        diag
    }
    fn default_base() -> LogPolicy {
        iobewi_log::default_policy("streambewi").unwrap()
    }
    fn at(seconds: u64) -> Instant {
        Instant::from_secs(seconds)
    }
    fn level(policy: &LogPolicy, target: &str) -> log::LevelFilter {
        policy.level_for(target)
    }

    #[test]
    fn the_profile_raises_the_four_product_targets_and_nothing_else() {
        let (policy, raised) = debug_policy(&default_base());
        assert_eq!(raised, DEBUG_TARGETS);
        for target in DEBUG_TARGETS {
            assert_eq!(level(&policy, target), log::LevelFilter::Debug, "{target}");
        }
        // The rest of the product and the third parties keep their level, and nothing is `trace`.
        assert_eq!(level(&policy, "streambewi::setup"), log::LevelFilter::Info);
        assert_eq!(level(&policy, "iobewi_esp_wifi"), log::LevelFilter::Warn);
        assert_eq!(
            level(&policy, "iobewi_wifi_manager"),
            log::LevelFilter::Warn
        );
        assert_eq!(level(&policy, "embassy_usb"), log::LevelFilter::Warn);
        assert_eq!(level(&policy, "iobewi_usb_msc"), log::LevelFilter::Warn);
        assert_eq!(policy.max_level(), log::LevelFilter::Debug);
        assert!(
            policy
                .targets()
                .iter()
                .all(|r| r.level <= log::LevelFilter::Debug)
        );
    }

    #[test]
    fn the_profile_never_names_a_wifi_owner_or_a_withheld_target() {
        for target in DEBUG_TARGETS {
            assert!(target.starts_with("streambewi::"), "{target}");
            assert!(
                !WITHHELD_TARGETS.iter().any(|t| target.starts_with(t)),
                "{target}"
            );
            assert!(
                !WITHHELD_WORDS
                    .iter()
                    .any(|w| contains_ignore_case(target, w)),
                "{target}"
            );
        }
        // And what they log goes through the same sanitizer as everything else.
        assert_eq!(
            sanitize("streambewi::stream", "retry with password=1"),
            Sanitized::Withhold
        );
        assert_eq!(
            kept(
                "streambewi::stream",
                "connecting to http://u:p@host:8000/live?x=abc"
            ),
            "streambewi::stream: connecting to http://host:8000/live?…"
        );
    }

    #[test]
    fn a_full_policy_is_kept_and_the_profile_is_partial_and_says_so() {
        // Eight rules of the user's own: no room for the four targets.
        let mut base = LogPolicy::new(log::LevelFilter::Warn);
        for name in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            base.add_target(name, log::LevelFilter::Error).unwrap();
        }
        let (policy, raised) = debug_policy(&base);
        assert!(raised.is_empty());
        assert_eq!(
            policy.targets(),
            base.targets(),
            "the policy is kept as it was"
        );

        // Six rules of the user's own: two of the four fit.
        let mut base = LogPolicy::new(log::LevelFilter::Warn);
        for name in ["a", "b", "c", "d", "e", "f"] {
            base.add_target(name, log::LevelFilter::Error).unwrap();
        }
        let (policy, raised) = debug_policy(&base);
        assert_eq!(raised, DEBUG_TARGETS[..2]);
        assert_eq!(policy.targets().len(), 8);
        // A rule of the user's for a debug target is raised, not duplicated, and takes no room.
        let mut base = LogPolicy::new(log::LevelFilter::Warn);
        for name in ["a", "b", "c", "d", "e", "f", "g"] {
            base.add_target(name, log::LevelFilter::Error).unwrap();
        }
        let mut with_usb = base.clone();
        with_usb
            .add_target("streambewi::usb", log::LevelFilter::Off)
            .unwrap();
        let (policy, raised) = debug_policy(&with_usb);
        assert_eq!(raised, ["streambewi::usb"]);
        assert_eq!(level(&policy, "streambewi::usb"), log::LevelFilter::Debug);
        assert_eq!(policy.targets().len(), 8);
    }

    #[test]
    fn on_then_off_restores_exactly_the_base_and_writes_nothing() {
        block(async {
            let backend = Backend::default();
            // A base with a target of its own, as a qualification image has.
            let mut base = default_base();
            base.add_target("embassy_usb", log::LevelFilter::Trace)
                .unwrap();
            let diag = recording(&backend, base.clone()).await;
            let yaml_before = diag.policy_yaml();
            assert!(!diag.debug_status(at(0)).enabled);

            let on = diag.debug_enable(at(10));
            assert!(on.enabled && !on.partial);
            assert_eq!(on.remaining_seconds, DEBUG_SECONDS);
            assert_eq!(on.targets, DEBUG_TARGETS);
            let applied = last_applied();
            assert_eq!(
                level(&applied, "streambewi::stream"),
                log::LevelFilter::Debug
            );
            assert_eq!(
                level(&applied, "embassy_usb"),
                log::LevelFilter::Trace,
                "the base's own targets stay"
            );

            diag.debug_disable();
            assert!(!diag.debug_status(at(11)).enabled);
            assert_eq!(last_applied(), base, "exactly the base again");
            // The persisted policy and the stored bytes were never touched.
            assert_eq!(diag.policy_yaml(), yaml_before);
            assert_eq!(backend.commits.get(), 0);
            assert!(backend.bytes.borrow().is_none());
        });
    }

    #[test]
    fn the_stored_policy_is_the_base_and_stays_stored() {
        block(async {
            let backend = Backend::default();
            *backend.bytes.borrow_mut() = Some(WARN_DEBUG.as_bytes().to_vec());
            let diag = recording(&backend, default_base()).await;
            let (yaml, source, _) = diag.policy_yaml();
            assert_eq!(source, Source::Persisted);

            diag.debug_enable(at(0));
            // While on, the policy that is asked for and stored is still the persisted one.
            assert_eq!(diag.policy_yaml().0, yaml);
            assert_eq!(diag.policy_yaml().1, Source::Persisted);
            diag.debug_tick(at(DEBUG_SECONDS));
            assert_eq!(diag.policy_yaml().0, yaml);
            assert_eq!(
                backend.bytes.borrow().as_deref(),
                Some(WARN_DEBUG.as_bytes())
            );
            assert_eq!(
                backend.commits.get(),
                0,
                "no write for a temporary diagnostic"
            );
        });
    }

    #[test]
    fn it_lasts_fifteen_minutes_and_a_second_request_does_not_extend_it() {
        block(async {
            let diag = recording(&Backend::default(), default_base()).await;
            let first = diag.debug_enable(at(100));
            assert_eq!(first.remaining_seconds, 900);
            // Asked again (and again) during the session: the end does not move.
            assert_eq!(diag.debug_enable(at(400)).remaining_seconds, 600);
            assert_eq!(diag.debug_enable(at(990)).remaining_seconds, 10);
            assert_eq!(applied_count(), 1, "the policy was applied once");

            // Not over yet.
            diag.debug_tick(at(999));
            assert!(diag.debug_status(at(999)).enabled);
            assert_eq!(diag.debug_status(at(999)).remaining_seconds, 1);
            // Over: at the deadline, not a second before.
            diag.debug_tick(at(1000));
            assert!(!diag.debug_status(at(1000)).enabled);
            assert_eq!(last_applied(), default_base());
        });
    }

    #[test]
    fn after_the_end_a_new_explicit_request_opens_a_new_session() {
        block(async {
            let diag = recording(&Backend::default(), default_base()).await;
            diag.debug_enable(at(0));
            diag.debug_tick(at(900));
            assert!(!diag.debug_status(at(900)).enabled);
            let again = diag.debug_enable(at(2_000));
            assert!(again.enabled);
            assert_eq!(again.remaining_seconds, 900, "a new session of its own");
            // A request that comes after the deadline but before the drain loop noticed also opens a new one.
            let late = diag.debug_enable(at(2_900));
            assert_eq!(late.remaining_seconds, 900);
        });
    }

    #[test]
    fn turning_it_off_is_immediate_and_harmless_when_it_is_off() {
        block(async {
            let diag = recording(&Backend::default(), default_base()).await;
            diag.debug_disable();
            diag.debug_disable();
            assert_eq!(applied_count(), 0, "nothing to restore");
            diag.debug_enable(at(0));
            diag.debug_disable();
            diag.debug_disable();
            assert_eq!(applied_count(), 2, "on, then off once");
            // The tick of a drain loop that has nothing to end does nothing.
            diag.debug_tick(at(10_000));
            assert_eq!(applied_count(), 2);
        });
    }

    #[test]
    fn a_policy_replaced_in_setup_during_the_session_becomes_the_base_under_the_profile() {
        block(async {
            let backend = Backend::default();
            let diag = recording(&backend, default_base()).await;
            diag.debug_enable(at(0));
            let before = applied_count();

            // A policy of the user's, with a target of its own.
            let new =
                "log:\n  default_level: warn\n  targets:\n    streambewi: debug\n    mine: info\n";
            assert!(block(diag.replace_yaml(new, true)).is_ok());
            assert_eq!(
                backend.commits.get(),
                1,
                "the replacement is the only write"
            );

            // The profile went back over the new policy: its targets are Debug, the user's rules are kept.
            assert!(applied_count() > before);
            let applied = last_applied();
            assert_eq!(
                level(&applied, "streambewi::stream"),
                log::LevelFilter::Debug
            );
            assert_eq!(
                level(&applied, "streambewi::setup"),
                log::LevelFilter::Debug
            );
            assert_eq!(level(&applied, "mine"), log::LevelFilter::Info);
            assert_eq!(applied.default_level, log::LevelFilter::Warn);
            assert!(diag.debug_status(at(1)).enabled, "the session goes on");

            // At the end, the new policy applies, not the old one.
            diag.debug_tick(at(DEBUG_SECONDS));
            let restored = last_applied();
            // The user's rule for the whole product (debug) applies again, not the default one (info).
            assert_eq!(
                level(&restored, "streambewi::stream"),
                log::LevelFilter::Debug
            );
            assert_eq!(restored.targets().len(), 2, "no profile target left");
            assert_eq!(level(&restored, "mine"), log::LevelFilter::Info);
            assert_eq!(diag.policy_yaml().0.trim(), new.trim());
        });
    }

    #[test]
    fn a_refused_replacement_during_the_session_changes_nothing() {
        block(async {
            let backend = Backend::default();
            let diag = recording(&backend, default_base()).await;
            diag.debug_enable(at(0));
            let before = applied_count();
            assert_eq!(
                block(diag.replace_yaml("nonsense", true)),
                Err(PutError::Invalid)
            );
            assert_eq!(
                block(diag.replace_yaml(WARN_DEBUG, false)),
                Err(PutError::NotAllowed)
            );
            assert_eq!(applied_count(), before);
            assert!(diag.debug_status(at(1)).enabled);
            assert_eq!(backend.commits.get(), 0);
        });
    }

    #[test]
    fn the_detailed_diagnostic_allocates_little() {
        let diag = block(recording(&Backend::default(), default_base()));
        let (on, peak, total, calls) = probe::measure(|| diag.debug_enable(at(0)));
        std::println!("POST /api/debug (on): peak {peak} B, total {total} B, {calls} calls");
        // Two policies of about 600 bytes, built on the heap and freed at once.
        assert!(on.enabled && peak < 4096, "{peak}");
        let (_, peak, total, calls) = probe::measure(|| {
            let s = diag.debug_status(at(1));
            serde_json::json!({"enabled": s.enabled, "remaining_seconds": s.remaining_seconds, "duration_seconds": DEBUG_SECONDS, "persistent": false, "targets": s.targets, "partial": s.partial}).to_string()
        });
        std::println!("GET /api/debug: peak {peak} B, total {total} B, {calls} calls");
        assert!(peak < 2048, "{peak}");
        let (_, peak, _, _) = probe::measure(|| diag.debug_disable());
        assert!(peak < 4096, "{peak}");
    }
}
