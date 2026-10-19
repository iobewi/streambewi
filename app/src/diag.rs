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
    state: BlockingMutex<NoopRawMutex, RefCell<PolicyState>>,
    history: BlockingMutex<NoopRawMutex, RefCell<History>>,
}

impl<B: ConfigBackend> Diag<B> {
    /// Reads the `log` space once. An absent, unreadable or invalid policy keeps the fallback already
    /// installed (and any policy the target applied itself); it is never written back or formatted.
    pub async fn load(space: ConfigSpace<B>, application_target: &str) -> Self {
        let mut state = PolicyState {
            policy: iobewi_log::default_policy(application_target)
                .unwrap_or_else(|_| LogPolicy::new(log::LevelFilter::Info)),
            source: Source::Default,
            load_error: false,
        };
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
            *s.borrow_mut() = PolicyState {
                policy,
                source: Source::Persisted,
                load_error: false,
            }
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
            let (lines, (seen, _, withheld)) = diag.recent(8);
            assert_eq!(seen, 2);
            assert_eq!(withheld, 1);
            assert_eq!(lines.len(), 1);
            assert_eq!(
                lines[0].1,
                "streambewi::stream: stream: connecting url=http://h/live.mp3?…"
            );
            assert_eq!(diag.drain(), 0);
        });
    }
}
