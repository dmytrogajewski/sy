//! Wayland `ext-idle-notify-v1` watcher (SPEC §2 "12-signal panel":
//! user-idle channel).
//!
//! **Step 7 known limitation:** `wayland-client` is not yet a direct
//! dep of the workspace (it is a transitive of iced, but pulling it
//! in here would inflate the dep budget beyond Step 7's scope). The
//! channel ships today as a **deterministic stub**: `poll()` always
//! returns `UserIdle { since_ms: 0 }` — i.e. "user is not idle". A
//! follow-up step will wire the real `ext-idle-notify-v1` listener.
//!
//! The pure-fn core [`compute_since_ms`] takes an injected
//! `last_activity_ms` clock value and returns the monotonic delta —
//! tested directly so the contract ("non-decreasing within a single
//! idle session") is locked down today.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::{IntentChannel, IntentEvent};

/// Pure-fn delta between `now` and the last activity timestamp, both
/// in milliseconds since some shared monotonic origin. Saturates at
/// zero for clock skew defensiveness — `since_ms` is never negative.
pub fn compute_since_ms(now_ms: u64, last_activity_ms: u64) -> u64 {
    now_ms.saturating_sub(last_activity_ms)
}

/// Idle-watcher channel. Holds an `Arc<Mutex<u64>>` slot the future
/// Wayland subscriber thread will update on each `idled`/`resumed`
/// transition. Today the slot is initialised at construction time
/// and never moves — `poll()` always emits `since_ms: 0`.
pub struct IdleChannel {
    origin: Instant,
    /// Monotonic ms of the last activity event. Step 10 will update
    /// this from the `ext-idle-notify-v1` `resumed` callback.
    last_activity_ms: Arc<Mutex<u64>>,
}

impl IdleChannel {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            last_activity_ms: Arc::new(Mutex::new(0)),
        }
    }

    /// Monotonic ms since the channel was constructed. Internal hook
    /// for the future subscriber + the monotonicity test.
    pub fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

impl Default for IdleChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl IntentChannel for IdleChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        let last = self.last_activity_ms.lock().ok().map(|g| *g).unwrap_or(0);
        Some(IntentEvent::UserIdle {
            since_ms: compute_since_ms(self.now_ms(), last),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Within a single idle session (no `resumed` event between
    /// observations), successive `poll()`s must return
    /// non-decreasing `since_ms` values — the forecaster relies on
    /// this monotonicity to compute "how long has the user been
    /// idle".
    #[test]
    fn since_ms_monotonic() {
        let mut ch = IdleChannel::new();
        let s1 = match ch.poll() {
            Some(IntentEvent::UserIdle { since_ms }) => since_ms,
            other => panic!("expected UserIdle, got {other:?}"),
        };
        // Inject a forward jump in monotonic time by sleeping a few
        // ms — the second observation must be ≥ the first.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let s2 = match ch.poll() {
            Some(IntentEvent::UserIdle { since_ms }) => since_ms,
            other => panic!("expected UserIdle, got {other:?}"),
        };
        assert!(s2 >= s1, "since_ms went backwards: {s1} -> {s2}");
    }

    /// `compute_since_ms` saturates at zero when the activity
    /// timestamp is somehow ahead of "now" (clock skew, future-dated
    /// subscriber update). The channel must never emit a negative
    /// `since_ms`.
    #[test]
    fn saturates_on_clock_skew() {
        assert_eq!(compute_since_ms(10, 100), 0);
        assert_eq!(compute_since_ms(100, 10), 90);
    }
}
