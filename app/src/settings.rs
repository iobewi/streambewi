//! Product settings kept in the `settings` space of `iobewi-config-space`.
//!
//! The space holds one small record of simple parameters, so that a new option does not need a space
//! of its own (a space costs at least 8 pages of the 48 the `config` partition can admit). The record
//! is a magic, then entries `id, length, value`:
//!
//! ```text
//! "SET1" | id u8, len u8, value[len] | id u8, len u8, value[len] | ...
//! ```
//!
//! | Id | Setting | Value |
//! | --- | --- | --- |
//! | 1 | LED brightness | one byte, 0 to 100 (percent) |
//!
//! An entry this firmware does not know is kept as it is when the record is rewritten, so a setting
//! added later is not lost by an older firmware that saves another one. A known entry with a bad
//! length or value counts as absent (the default applies) and is replaced by the next save of it. A
//! record that is not a `SET1` record counts as empty and is replaced by the next save: nothing is
//! ever erased by loading.
//!
//! This is the only thing the `settings` space holds. It does not touch the Wi-Fi, USB boot, stations
//! or log spaces.

use alloc::vec::Vec;

use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use iobewi_config_space::{Budget, ConfigBackend, ConfigSpace};

use crate::brightness::{self, SaveError};

/// Name of the space in the store's manifest.
pub const SPACE: &str = "settings";
/// Room for a handful of simple settings. The space reserves 8 pages whatever the budget up to ~2 KB.
pub const CONFIG_BUDGET: Budget = Budget::new(64);

const MAGIC: &[u8; 4] = b"SET1";
const LED_BRIGHTNESS: u8 = 1;

/// The settings of a record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Values {
    /// Percent, 0 to 100. `None`: not saved, the default applies.
    pub led_percent: Option<u8>,
    /// Entries of settings this firmware does not know, in the record's own encoding.
    other: Vec<u8>,
}

impl Values {
    /// Reads a record. `None` if it is not a `SET1` record or an entry is cut short.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut rest = bytes.strip_prefix(MAGIC.as_slice())?;
        let mut values = Self::default();
        while !rest.is_empty() {
            let (&id, tail) = rest.split_first()?;
            let (&len, tail) = tail.split_first()?;
            let len = usize::from(len);
            let value = tail.get(..len)?;
            match id {
                LED_BRIGHTNESS => {
                    values.led_percent = match value {
                        [percent] if brightness::valid(*percent) => Some(*percent),
                        // A value this firmware cannot use counts as absent.
                        _ => None,
                    };
                }
                _ => values.other.extend_from_slice(&rest[..2 + len]),
            }
            rest = &tail[len..];
        }
        Some(values)
    }

    /// The record for these values.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(MAGIC.len() + 3 + self.other.len());
        out.extend_from_slice(MAGIC);
        if let Some(percent) = self.led_percent {
            out.extend_from_slice(&[LED_BRIGHTNESS, 1, percent]);
        }
        out.extend_from_slice(&self.other);
        out
    }
}

/// How the record was found at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loaded {
    /// A valid record.
    Record,
    /// No record yet (first boot of this firmware).
    Absent,
    /// A record that is not a `SET1` record: counts as empty.
    Unreadable,
    /// The storage could not be read: counts as empty.
    StorageError,
}

pub struct SettingsStore<B: ConfigBackend> {
    space: ConfigSpace<B>,
    current: Mutex<NoopRawMutex, Values>,
}

impl<B: ConfigBackend> SettingsStore<B> {
    /// Reads the record. It never fails and never writes: whatever is wrong, the defaults apply.
    pub async fn load(space: ConfigSpace<B>) -> (Self, Loaded) {
        let (values, how) = match space.load().await {
            Ok(None) => (Values::default(), Loaded::Absent),
            Ok(Some(snapshot)) => match Values::decode(&snapshot.data) {
                Some(values) => (values, Loaded::Record),
                None => (Values::default(), Loaded::Unreadable),
            },
            Err(_) => (Values::default(), Loaded::StorageError),
        };
        (
            Self {
                space,
                current: Mutex::new(values),
            },
            how,
        )
    }

    /// The saved LED brightness, or the default.
    pub async fn led_percent(&self) -> u8 {
        self.current
            .lock()
            .await
            .led_percent
            .unwrap_or(brightness::DEFAULT_PERCENT)
    }

    /// Keeps `percent` as the LED brightness: one commit of the whole record through Config-Space.
    /// The value in RAM changes only after the commit succeeded.
    pub async fn save_led_percent(&self, percent: u8) -> Result<(), SaveError> {
        if !brightness::valid(percent) {
            return Err(SaveError::Storage);
        }
        let mut current = self.current.lock().await;
        let mut next = current.clone();
        next.led_percent = Some(percent);
        let bytes = next.encode();
        if bytes.len() > CONFIG_BUDGET.max_bytes() {
            return Err(SaveError::Storage);
        }
        self.space
            .commit(&bytes)
            .await
            .map_err(|_| SaveError::Storage)?;
        *current = next;
        Ok(())
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
