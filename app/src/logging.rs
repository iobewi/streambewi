//! Operating log of the product.
//!
//! StreamBeWI logs through the `log` facade; `iobewi-log` supplies the global
//! logger and the bounded ring. Operating events (network connection, USB bus and
//! stream state, data loss, provisioning) are logged at Info and Warn; per-sector
//! and driver traces are debug and stay off. The target supplies the console
//! callback; Improv ignores text that is not one of its frames, so the log shares
//! UART0 with it.

/// Log target prefix of this crate (`streambewi::...`): captured at Info.
/// Other targets are captured at Warn and above.
pub const LOG_TARGET: &str = "streambewi";

/// Installs the global logger with the target's console writer. The target calls this once before
/// configuration preparation and before [`run`](crate::run).
pub fn install(console: fn(&log::Record<'_>)) {
    iobewi_log::install(console, LOG_TARGET);
}

use core::cell::RefCell;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use iobewi_log::LogPolicy;

/// The policy the target applied itself at start, if it did (a qualification image adds targets).
static APPLIED: Mutex<CriticalSectionRawMutex, RefCell<Option<LogPolicy>>> =
    Mutex::new(RefCell::new(None));

/// The target tells which policy it applied. Called once, right after `apply_policy`.
pub fn remember_applied(policy: LogPolicy) {
    APPLIED.lock(|slot| *slot.borrow_mut() = Some(policy));
}

/// The policy that applies when no policy is stored and no detailed diagnostic is on: what the target
/// applied, or the default of `iobewi-log`.
pub fn baseline() -> LogPolicy {
    APPLIED
        .lock(|slot| slot.borrow().clone())
        .unwrap_or_else(|| {
            iobewi_log::default_policy(LOG_TARGET)
                .unwrap_or_else(|_| LogPolicy::new(log::LevelFilter::Info))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_baseline_is_the_default_policy_until_the_target_says_otherwise() {
        // Process-wide state: this is the only test that touches it.
        let default = iobewi_log::default_policy(LOG_TARGET).unwrap();
        assert_eq!(baseline(), default);

        let mut applied = default.clone();
        applied
            .add_target("embassy_usb", log::LevelFilter::Trace)
            .unwrap();
        remember_applied(applied.clone());
        assert_eq!(baseline(), applied, "what the target applied is the base");
        assert_ne!(baseline(), default);
    }
}
