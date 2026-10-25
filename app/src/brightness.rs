//! Brightness of the status LED: the user's percentage, the level the indicator gets, and the three
//! values behind the setting.
//!
//! - **saved**: the percentage kept in the configuration (`settings` space, see [`crate::settings`]);
//!   unknown until the configuration is read at boot.
//! - **previewed**: a percentage asked for by the settings page, held in RAM and dropped after
//!   [`PREVIEW_TTL`] without a new request, or by a cancel. It is never written anywhere.
//! - **applied**: what the LED shows: the preview while there is one, else the saved value, else
//!   nothing (dark) while the saved value is not known yet.
//!
//! A save is asked here ([`Brightness::request_save`]) and done by `stream::save_settings`, which owns
//! the protection of the USB disk. The page reads the outcome in `/api/state`.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant};

/// The level (out of 255) that 100 % stands for.
pub const MAX_LEVEL: u8 = 64;
/// The percentage when none is saved: level 23, what the LED showed before the setting existed.
pub const DEFAULT_PERCENT: u8 = 36;
/// How long a preview lasts without a new request.
pub const PREVIEW_TTL: Duration = Duration::from_secs(60);

/// Whether `percent` is a brightness the user can ask for.
pub const fn valid(percent: u8) -> bool {
    percent <= 100
}

/// The level for the indicator: `percent * MAX_LEVEL / 100`, rounded down. 0 is dark, 100 is
/// [`MAX_LEVEL`]. Linear: no gamma. A percentage over 100 counts as 100.
pub const fn level_of(percent: u8) -> u8 {
    let percent = if percent > 100 { 100 } else { percent };
    (percent as u16 * MAX_LEVEL as u16 / 100) as u8
}

/// Why a save did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveError {
    /// The configuration could not be written; the previous value is kept.
    Storage,
}

impl SaveError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Storage => "Écriture impossible : l’ancienne luminosité est conservée.",
        }
    }
}

/// A request that is not valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid;

#[derive(Default)]
struct Inner {
    saved: Option<u8>,
    preview: Option<(u8, Instant)>,
    /// The latest save asked for and not started.
    pending: Option<u8>,
    /// The save being written.
    saving: Option<u8>,
    failed: Option<SaveError>,
    available: bool,
}

/// What `/api/state` says of the setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// The firmware has a status LED to set.
    pub available: bool,
    /// The percentage kept in the configuration.
    pub saved: u8,
    /// The percentage being previewed, if any.
    pub preview: Option<u8>,
    /// The percentage the LED shows.
    pub applied: u8,
    /// A save is asked for or being written.
    pub busy: bool,
    /// The last save failed (until the next request).
    pub error: Option<SaveError>,
}

pub struct Brightness {
    ttl: Duration,
    inner: Mutex<CriticalSectionRawMutex, RefCell<Inner>>,
    /// The applied level may have changed: wakes the LED task.
    changed: Signal<CriticalSectionRawMutex, ()>,
    /// A save was asked for: wakes `stream::save_settings`.
    save: Signal<CriticalSectionRawMutex, ()>,
}

impl Brightness {
    pub const fn new() -> Self {
        Self::with_preview_ttl(PREVIEW_TTL)
    }

    /// Only the tests shorten the preview time (a test cannot wait a minute).
    #[cfg(test)]
    pub(crate) const fn short_preview(ttl: Duration) -> Self {
        Self::with_preview_ttl(ttl)
    }

    const fn with_preview_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(RefCell::new(Inner {
                saved: None,
                preview: None,
                pending: None,
                saving: None,
                failed: None,
                available: false,
            })),
            changed: Signal::new(),
            save: Signal::new(),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        self.inner.lock(|cell| f(&mut cell.borrow_mut()))
    }

    /// The LED task is running (`true`) or ended (`false`).
    pub fn set_available(&self, available: bool) {
        self.with(|inner| inner.available = available);
    }

    /// The saved percentage, read from the configuration at boot.
    pub fn loaded(&self, percent: u8) {
        self.with(|inner| {
            inner.saved = Some(if valid(percent) {
                percent
            } else {
                DEFAULT_PERCENT
            })
        });
        self.changed.signal(());
    }

    fn applied_percent(inner: &Inner, now: Instant) -> u8 {
        match inner.preview {
            Some((percent, deadline)) if deadline > now => percent,
            _ => inner.saved.unwrap_or(0),
        }
    }

    /// The level the LED shows now: dark until the saved value is known.
    pub fn applied_level(&self, now: Instant) -> u8 {
        level_of(self.with(|inner| Self::applied_percent(inner, now)))
    }

    /// Shows `percent` for [`PREVIEW_TTL`], renewed by every call. Nothing is written.
    pub fn preview(&self, percent: u8, now: Instant) -> Result<(), Invalid> {
        if !valid(percent) {
            return Err(Invalid);
        }
        let deadline = now + self.ttl;
        self.with(|inner| inner.preview = Some((percent, deadline)));
        self.changed.signal(());
        Ok(())
    }

    /// Drops the preview: the LED goes back to the saved value.
    pub fn cancel(&self) {
        self.with(|inner| inner.preview = None);
        self.changed.signal(());
    }

    /// When the preview ends, if there is one.
    pub fn preview_deadline(&self) -> Option<Instant> {
        self.with(|inner| inner.preview.map(|(_, deadline)| deadline))
    }

    /// Drops the preview if its time is up. Returns whether it did.
    pub fn expire(&self, now: Instant) -> bool {
        self.with(|inner| match inner.preview {
            Some((_, deadline)) if deadline <= now => {
                inner.preview = None;
                true
            }
            _ => false,
        })
    }

    /// Resolves when the applied level may have changed.
    pub async fn changed(&self) {
        self.changed.wait().await;
    }

    /// Asks to keep `percent`. The write is done later, protected: see `stream::save_settings`.
    pub fn request_save(&self, percent: u8) -> Result<(), Invalid> {
        if !valid(percent) {
            return Err(Invalid);
        }
        self.with(|inner| {
            inner.pending = Some(percent);
            inner.failed = None;
        });
        self.save.signal(());
        Ok(())
    }

    /// Waits for a save to do, and marks it as being written.
    pub async fn next_save(&self) -> u8 {
        loop {
            if let Some(percent) = self.with(|inner| {
                let next = inner.pending.take();
                if next.is_some() {
                    inner.saving = next;
                }
                next
            }) {
                return percent;
            }
            self.save.wait().await;
        }
    }

    /// The outcome of the write of `percent`. A success makes it the saved value and drops the
    /// preview. A failure keeps the saved value and the preview, and records the error.
    pub fn finish_save(&self, percent: u8, outcome: Result<(), SaveError>) {
        self.with(|inner| {
            inner.saving = None;
            match outcome {
                Ok(()) => {
                    inner.saved = Some(percent);
                    inner.preview = None;
                    inner.failed = None;
                }
                Err(error) => inner.failed = Some(error),
            }
        });
        self.changed.signal(());
    }

    pub fn status(&self, now: Instant) -> Status {
        self.with(|inner| Status {
            available: inner.available,
            saved: inner.saved.unwrap_or(DEFAULT_PERCENT),
            preview: match inner.preview {
                Some((percent, deadline)) if deadline > now => Some(percent),
                _ => None,
            },
            applied: Self::applied_percent(inner, now),
            busy: inner.pending.is_some() || inner.saving.is_some(),
            error: inner.failed,
        })
    }
}

impl Default for Brightness {
    fn default() -> Self {
        Self::new()
    }
}

/// The brightness of the product's status LED.
pub static BRIGHTNESS: Brightness = Brightness::new();

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Instant {
        Instant::from_millis(ms)
    }

    #[test]
    fn the_percentage_gives_the_level() {
        assert_eq!(level_of(0), 0);
        assert_eq!(
            level_of(36),
            23,
            "the default is what the LED showed before the setting"
        );
        assert_eq!(level_of(100), MAX_LEVEL);
        assert_eq!(level_of(50), 32);
        assert_eq!(level_of(1), 0);
        assert_eq!(level_of(101), MAX_LEVEL, "over 100 counts as 100");
        // Never past the cap, and never going down as the percentage goes up.
        let mut last = 0;
        for percent in 0..=100 {
            let level = level_of(percent);
            assert!(level >= last && level <= MAX_LEVEL);
            last = level;
        }
        assert!(valid(0) && valid(100) && !valid(101));
    }

    #[test]
    fn dark_until_the_saved_value_is_known_then_the_saved_value() {
        let b = Brightness::new();
        assert_eq!(
            b.applied_level(at(0)),
            0,
            "no flash before the configuration is read"
        );
        assert_eq!(b.status(at(0)).saved, DEFAULT_PERCENT);
        b.loaded(0);
        assert_eq!(b.applied_level(at(0)), 0);
        b.loaded(100);
        assert_eq!(b.applied_level(at(0)), MAX_LEVEL);
        b.loaded(250);
        assert_eq!(
            b.status(at(0)).saved,
            DEFAULT_PERCENT,
            "an unusable value counts as the default"
        );
    }

    #[test]
    fn a_preview_is_applied_but_never_saved_and_ends_after_sixty_seconds() {
        let b = Brightness::new();
        b.loaded(36);
        b.preview(80, at(1_000)).unwrap();
        let s = b.status(at(1_000));
        assert_eq!(
            (s.saved, s.preview, s.applied, s.busy),
            (36, Some(80), 80, false)
        );
        assert_eq!(b.applied_level(at(1_000)), level_of(80));
        assert_eq!(b.preview_deadline(), Some(at(61_000)));

        // Renewed by every request.
        b.preview(10, at(30_000)).unwrap();
        assert_eq!(b.preview_deadline(), Some(at(90_000)));
        assert!(!b.expire(at(89_999)));
        assert_eq!(b.status(at(89_999)).applied, 10);

        // Over: back to the saved value.
        assert_eq!(b.status(at(90_000)).applied, 36);
        assert_eq!(b.applied_level(at(90_000)), level_of(36));
        assert!(b.expire(at(90_000)));
        assert_eq!(b.preview_deadline(), None);
        assert_eq!(b.status(at(90_000)).preview, None);
        assert!(!b.expire(at(90_001)), "nothing left to expire");
    }

    #[test]
    fn a_cancel_goes_back_to_the_saved_value() {
        let b = Brightness::new();
        b.loaded(36);
        b.preview(0, at(0)).unwrap();
        assert_eq!(b.applied_level(at(1)), 0);
        b.cancel();
        assert_eq!(b.status(at(1)).preview, None);
        assert_eq!(b.applied_level(at(1)), level_of(36));
        b.cancel(); // nothing to cancel: harmless
    }

    #[test]
    fn a_value_out_of_range_is_refused_and_changes_nothing() {
        let b = Brightness::new();
        b.loaded(36);
        assert_eq!(b.preview(101, at(0)), Err(Invalid));
        assert_eq!(b.request_save(101), Err(Invalid));
        let s = b.status(at(0));
        assert_eq!((s.preview, s.busy, s.saved), (None, false, 36));
    }

    #[test]
    fn a_save_is_pending_then_saving_then_the_saved_value() {
        let b = Brightness::new();
        b.loaded(36);
        b.preview(70, at(0)).unwrap();
        b.request_save(70).unwrap();
        assert!(b.status(at(0)).busy);
        assert_eq!(b.status(at(0)).saved, 36, "not saved yet");

        let percent = embassy_futures::block_on(b.next_save());
        assert_eq!(percent, 70);
        assert!(b.status(at(0)).busy, "being written");
        assert_eq!(b.status(at(0)).saved, 36);

        b.finish_save(70, Ok(()));
        let s = b.status(at(0));
        assert_eq!(
            (s.saved, s.busy, s.error, s.preview, s.applied),
            (70, false, None, None, 70)
        );
    }

    #[test]
    fn a_failed_save_keeps_the_saved_value_and_the_preview_and_says_so() {
        let b = Brightness::new();
        b.loaded(36);
        b.preview(70, at(0)).unwrap();
        b.request_save(70).unwrap();
        let percent = embassy_futures::block_on(b.next_save());
        b.finish_save(percent, Err(SaveError::Storage));
        let s = b.status(at(1));
        assert_eq!(s.saved, 36, "the previous value is kept");
        assert_eq!(
            s.preview,
            Some(70),
            "the LED keeps showing what was asked, until the preview ends"
        );
        assert_eq!(s.error, Some(SaveError::Storage));
        assert!(!s.busy);
        assert!(SaveError::Storage.message().contains("conservée"));

        // A new request clears the error and can succeed.
        b.request_save(70).unwrap();
        assert_eq!(b.status(at(1)).error, None);
        let percent = embassy_futures::block_on(b.next_save());
        b.finish_save(percent, Ok(()));
        assert_eq!(b.status(at(1)).saved, 70);
    }

    #[test]
    fn saves_are_taken_one_at_a_time_and_the_latest_request_wins() {
        let b = Brightness::new();
        b.loaded(36);
        b.request_save(10).unwrap();
        b.request_save(20).unwrap();
        assert_eq!(
            embassy_futures::block_on(b.next_save()),
            20,
            "the latest request replaces the earlier one"
        );
        // Asked for while the 20 is being written: taken after it.
        b.request_save(30).unwrap();
        b.finish_save(20, Ok(()));
        assert!(b.status(at(0)).busy, "the 30 is still to do");
        assert_eq!(embassy_futures::block_on(b.next_save()), 30);
        b.finish_save(30, Ok(()));
        assert_eq!(b.status(at(0)).saved, 30);
        assert!(!b.status(at(0)).busy);
    }

    #[test]
    fn the_led_task_is_reported_as_available() {
        let b = Brightness::new();
        assert!(!b.status(at(0)).available);
        b.set_available(true);
        assert!(b.status(at(0)).available);
        b.set_available(false);
        assert!(!b.status(at(0)).available);
    }
}
