//! Restart policy: exponential backoff with a cap.

use std::time::Duration;

const BASE_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Time to wait before respawning a worker that just died. Doubles
/// per consecutive failure (1 s, 2 s, 4 s, … capped at 60 s). The
/// counter resets when the child reports `Ready` again.
pub fn backoff_for_attempt(attempt: u32) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    let raw = BASE_BACKOFF.saturating_mul(1u32 << attempt.min(6));
    raw.min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_cap() {
        assert_eq!(backoff_for_attempt(0), Duration::ZERO);
        assert_eq!(backoff_for_attempt(1), Duration::from_secs(2));
        assert_eq!(backoff_for_attempt(2), Duration::from_secs(4));
        assert_eq!(backoff_for_attempt(3), Duration::from_secs(8));
        assert_eq!(backoff_for_attempt(4), Duration::from_secs(16));
        assert_eq!(backoff_for_attempt(5), Duration::from_secs(32));
        assert_eq!(backoff_for_attempt(6), Duration::from_secs(60));
        assert_eq!(backoff_for_attempt(7), Duration::from_secs(60));
        assert_eq!(backoff_for_attempt(99), Duration::from_secs(60));
    }
}
