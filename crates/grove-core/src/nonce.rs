//! A value that will not repeat, for naming one attempt at something.
//!
//! Two callers, for the same reason. Every control request carries an
//! idempotency key so a reply lost on the way back cannot make the GUI ask
//! twice and get two sessions; and a transient systemd scope carries one so a
//! restarted process cannot collide with the unit of the one it replaces,
//! which systemd refuses while the old unit is still being cleaned up.
//!
//! The clock is enough for both. Neither use is a secret or a hash: the
//! requirement is only that two attempts a moment apart get different names.

/// Nanoseconds since the epoch.
///
/// A clock before the epoch yields 0 rather than failing: a repeated nonce
/// costs an idempotency collision, which is a retry, and is not worth an error
/// path on a machine whose clock is that broken.
pub fn nonce() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_nonces_taken_in_order_do_not_go_backwards() {
        let first = nonce();
        let second = nonce();
        assert!(second >= first, "the clock ran backwards within one test");
        assert!(first > 0, "a working clock is past the epoch");
    }
}
