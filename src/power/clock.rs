//! Clock injection for the snapshot assembler (Step 8).
//!
//! `snapshot::collect_tick` must not pull `Utc::now()` directly — the
//! tests need a frozen clock so the resulting feature vector is
//! byte-stable. The trait is deliberately tiny: one method,
//! `Send + Sync`, no lifetime parameters, so the daemon (Step 10) can
//! pass a `&dyn Clock` straight through.

#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::time::Duration;

use chrono::{DateTime, Utc};

/// Wall-clock source. Production wires `SystemClock` (`Utc::now`);
/// tests wire `MockClock` with a pinned instant.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Production clock — thin wrapper over `chrono::Utc::now()`.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Test clock — `now()` returns a pinned instant the test author
/// advances explicitly via `tick(d)`. Interior `Mutex` keeps the
/// trait's `&self` signature compatible while still allowing
/// in-place advancement from a single thread.
///
/// Test-only: production wires `SystemClock`. `MockClock` is gated
/// under `#[cfg(test)]` so the non-test build stays free of
/// dead-code warnings — the project bans dead-code suppression
/// attributes outside test scope, so test-scoping is the canonical
/// way to land a helper used only from tests.
#[cfg(test)]
#[derive(Debug)]
pub struct MockClock {
    now: Mutex<DateTime<Utc>>,
}

#[cfg(test)]
impl MockClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    /// Advance the clock by `d`. Tests use this to simulate time
    /// passage between successive `collect_tick` calls.
    pub fn tick(&self, d: Duration) {
        if let Ok(mut g) = self.now.lock() {
            let chrono_d =
                chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::zero());
            *g += chrono_d;
        }
    }
}

#[cfg(test)]
impl Clock for MockClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|p| *p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A pinned `MockClock` returns the exact same instant on every
    /// `now()` call — the assertion the snapshot test relies on for
    /// byte-stable output.
    #[test]
    fn mock_clock_pinned_returns_same_instant() {
        let t = Utc
            .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
            .single()
            .unwrap();
        let c = MockClock::new(t);
        assert_eq!(c.now(), t);
        assert_eq!(c.now(), t);
    }

    /// `tick(d)` advances the pinned instant by the requested duration.
    #[test]
    fn mock_clock_ticks_forward() {
        let t = Utc
            .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
            .single()
            .unwrap();
        let c = MockClock::new(t);
        c.tick(Duration::from_secs(60));
        assert_eq!(
            c.now(),
            Utc.with_ymd_and_hms(2026, 5, 19, 12, 1, 0)
                .single()
                .unwrap()
        );
    }
}
