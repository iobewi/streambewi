//! Maintenance start of the system A OTA (#3): what is portable and testable on the host.
//!
//! The OTA itself is the reference composition of `rust-mcu-libs` (`examples/firmware/ota-system`):
//! the transfer, the flash writer, the image validation, OTM1, the activation and the probation are
//! not rewritten here. This module only holds the product's side of it.
//!
//! **Maintenance** is a start of the product that does one thing: the OTA server. No stream, no USB
//! disk, no local web page. It is asked for by `POST /api/maintenance` in the normal start; the target
//! writes a one-shot request in a RAM that survives a reset and not a power cut, restarts, and reads
//! it back once. A power cut, or ten minutes without a request, returns to the normal start.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// A maintenance start was asked for and is waiting for the restart.
static PENDING: AtomicBool = AtomicBool::new(false);

/// This image can serve an OTA (token and TLS identity compiled in). Set by the target at startup.
static AVAILABLE: AtomicBool = AtomicBool::new(false);

pub fn set_available(available: bool) {
    AVAILABLE.store(available, Ordering::Relaxed);
}

pub fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

/// Asks for a restart into maintenance. The restart is the existing one (`setup::RESTART`): the target's
/// reset reads [`maintenance_pending`] first and keeps the request across the reset.
pub fn request_maintenance() {
    PENDING.store(true, Ordering::Relaxed);
    crate::setup::RESTART.signal(());
}

/// True when the restart that is about to happen must be a maintenance start.
pub fn maintenance_pending() -> bool {
    PENDING.load(Ordering::Relaxed)
}

/// The one-shot request kept across a reset: a word and its complement, so that random contents after a
/// power-on (or a reset that interrupted the write) are not taken for a request.
const MAGIC: u32 = 0x4d41_494e; // "MAIN"

pub const fn encode_request() -> [u32; 2] {
    [MAGIC, !MAGIC]
}

pub const CLEARED: [u32; 2] = [0, 0];

pub const fn is_request(words: [u32; 2]) -> bool {
    words[0] == MAGIC && words[1] == !MAGIC
}

/// A maintenance start with no request for this long restarts into the normal start.
pub const IDLE_LIMIT_SECS: u32 = 600;

static LAST_REQUEST_SECS: AtomicU32 = AtomicU32::new(0);

/// Records that the OTA server just served a request.
pub fn touch(now_secs: u32) {
    LAST_REQUEST_SECS.store(now_secs, Ordering::Relaxed);
}

pub fn idle_expired(now_secs: u32) -> bool {
    now_secs.wrapping_sub(LAST_REQUEST_SECS.load(Ordering::Relaxed)) >= IDLE_LIMIT_SECS
}

/// A deployment identifier: 1 to 64 ASCII letters, digits, `-`, `_` or `.` (a header value, never trusted).
pub fn valid_deployment_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A request refused before anything is superseded or erased: bad identifier, empty or oversized body,
/// or a digest that is not `sha256:<64 hex>`.
pub fn valid_upload(id: &str, digest: &str, length: usize, capacity: usize) -> bool {
    valid_deployment_id(id)
        && length > 0
        && length <= capacity
        && length <= u32::MAX as usize
        && iobewi_firmware_image::parse_digest(digest).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn an_upload_is_refused_before_anything_is_superseded_or_erased() {
        assert!(valid_upload("deploy-1", DIGEST, 1024, 1024));
        assert!(!valid_upload("deploy-1", DIGEST, 1025, 1024), "too big");
        assert!(!valid_upload("deploy-1", DIGEST, 0, 1024), "empty");
        assert!(!valid_upload("deploy-1", "sha256:zz", 1, 1024), "digest");
        assert!(!valid_upload(
            "deploy-1",
            &DIGEST.replace("sha256:", ""),
            1,
            1024
        ));
        assert!(!valid_upload("", DIGEST, 1, 1024), "no identifier");
        assert!(!valid_upload("foreign\r\nheader", DIGEST, 1, 1024));
        assert!(!valid_deployment_id("é"));
        assert!(!valid_deployment_id(&"a".repeat(65)));
        assert!(valid_deployment_id(&"a".repeat(64)));
    }

    #[test]
    fn only_the_word_and_its_complement_are_a_request() {
        assert!(is_request(encode_request()));
        assert!(!is_request(CLEARED));
        assert!(!is_request([MAGIC, MAGIC]));
        assert!(!is_request([!MAGIC, MAGIC]));
        assert!(!is_request([0xffff_ffff, 0xffff_ffff]));
        assert!(!is_request([MAGIC ^ 1, !MAGIC]));
    }

    #[test]
    fn maintenance_returns_to_the_normal_start_after_ten_quiet_minutes() {
        touch(100);
        assert!(!idle_expired(100));
        assert!(!idle_expired(100 + IDLE_LIMIT_SECS - 1));
        assert!(idle_expired(100 + IDLE_LIMIT_SECS));
        touch(100 + IDLE_LIMIT_SECS - 1);
        assert!(!idle_expired(100 + IDLE_LIMIT_SECS));
        // The clock may wrap: the difference is what counts.
        touch(u32::MAX - 5);
        assert!(!idle_expired(10));
    }

    #[test]
    fn asking_for_maintenance_asks_for_the_existing_restart() {
        // The flags are process-wide and shared with the route test: only what this call sets is asserted.
        request_maintenance();
        assert!(maintenance_pending());
        assert!(crate::setup::RESTART.signaled());
    }
}
