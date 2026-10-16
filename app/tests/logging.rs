//! The product's `log::*` calls reach the `iobewi-log` ring. One test only: the
//! logger and the ring are process-wide statics.

use std::sync::atomic::{AtomicUsize, Ordering};

static CONSOLE_LINES: AtomicUsize = AtomicUsize::new(0);

fn console(_: &log::Record<'_>) {
    CONSOLE_LINES.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn product_logs_are_captured_and_reach_the_console() {
    streambewi::logging::install(console);
    iobewi_log::discard();

    // Module path of the product crate, as emitted by `log::info!` inside `app/src`.
    log::info!(target: "streambewi::provisioning", "wifi: ready");
    log::error!(target: "streambewi::boot_policy", "boom");
    // Foreign crates are only captured from Warn up.
    log::info!(target: "embassy_net", "dropped");
    log::warn!(target: "embassy_net", "kept");
    log::debug!(target: "streambewi::stream", "dropped: Info is the global maximum");

    let lines: Vec<String> = std::iter::from_fn(iobewi_log::pop_line)
        .map(|l| l.as_str().to_owned())
        .collect();
    assert_eq!(lines, ["wifi: ready", "boom", "kept"]);
    assert_eq!(CONSOLE_LINES.load(Ordering::Relaxed), 3);
}
