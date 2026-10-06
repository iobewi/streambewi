//! Local log capture for the product.
//!
//! StreamBeWI logs through the `log` facade; `iobewi-log` supplies the global
//! logger and the bounded ring. The product owns no physical console: the console
//! callback is a no-op, so nothing is written to UART0 or USB-Serial-JTAG (which
//! carry Improv) and lines are only captured in the ring.

/// Log target prefix of this crate (`streambewi::...`): captured at Info.
/// Other targets are captured at Warn and above.
pub const LOG_TARGET: &str = "streambewi";

fn no_console(_: &log::Record<'_>) {}

/// Installs the global logger. Call once, first thing in [`run`](crate::run),
/// before the product logs.
pub fn install() {
    iobewi_log::install(no_console, LOG_TARGET);
}
