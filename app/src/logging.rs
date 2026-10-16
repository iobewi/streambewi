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
