//! Cyclical time-of-day + day-of-week encoding (SPEC §2 "12-signal
//! panel": the time channel). Plain pure-fn — no bus, no fd, no
//! socket. The forecaster sees `(sin θ, cos θ)` for hour-of-day and
//! `(sin φ, cos φ)` for day-of-week so the 23:59 → 00:00 wrap is
//! continuous in feature space (avoiding the discontinuity a raw
//! `hour` integer would impose on a regressor).
//!
//! Step 8's snapshot assembler will inject a `Clock` here. Today the
//! channel reads `chrono::Utc::now()` on every `poll()` so the
//! anti-dead-code probe in `cli::probe_intent` works without a clock
//! wired through yet.

use std::f32::consts::TAU;

use chrono::{DateTime, Datelike, Timelike, Utc};

use super::{IntentChannel, IntentEvent};

/// 24-hour cycle length.
const HOURS_PER_DAY: f32 = 24.0;
/// 7-day cycle length (Monday=0..Sunday=6 in `chrono::Weekday::num_days_from_monday`).
const DAYS_PER_WEEK: f32 = 7.0;

/// Pure cyclical encoding. `t` is any `DateTime<Utc>`; output is the
/// `(sin, cos)` pair for hour-of-day plus the same pair for
/// day-of-week. Returned as a four-tuple to keep the signature flat
/// (no helper struct) — Step 8 packs these into the feature vec.
pub fn encode(t: DateTime<Utc>) -> (f32, f32, f32, f32) {
    let hour_frac =
        (t.hour() as f32 + t.minute() as f32 / 60.0 + t.second() as f32 / 3600.0) / HOURS_PER_DAY;
    let dow_frac = (t.weekday().num_days_from_monday() as f32
        + (t.hour() as f32 + t.minute() as f32 / 60.0) / HOURS_PER_DAY)
        / DAYS_PER_WEEK;
    let theta = TAU * hour_frac;
    let phi = TAU * dow_frac;
    (theta.sin(), theta.cos(), phi.sin(), phi.cos())
}

/// Stateless clock-driven channel. Each `poll()` returns a fresh
/// `IntentEvent::TimeOfDay` — the daemon snapshot assembler (Step 8)
/// reads it every 1 Hz tick. There is no dedup because the encoding
/// changes every second.
pub struct TimeChannel;

impl TimeChannel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TimeChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl IntentChannel for TimeChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        let (sin, cos, dow_sin, dow_cos) = encode(Utc::now());
        Some(IntentEvent::TimeOfDay {
            sin,
            cos,
            dow_sin,
            dow_cos,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Encoding tolerance: the `23:59:59 Sunday → 00:00:00 Monday`
    /// wrap should land within ~3e-3 (the second-resolution gap is
    /// `1/86400` of a full cycle, ≈ 7.3e-5 radians on hour-of-day
    /// plus the week wrap; floats compound a bit).
    const WRAP_TOL: f32 = 4e-3;

    /// The cyclical encoding must wrap continuously across the
    /// `23:59:59 Sunday → 00:00:00 Monday` boundary — that is the
    /// whole point of the (sin, cos) mapping vs a raw `hour` int.
    /// Sunday is `weekday().num_days_from_monday() == 6`, Monday is 0;
    /// a naive integer encoding would jump 6 → 0 here.
    #[test]
    fn cyclical_encoding_continuous() {
        // 2024-12-29 was a Sunday; 2024-12-30 was the following Monday.
        let sunday_end = Utc
            .with_ymd_and_hms(2024, 12, 29, 23, 59, 59)
            .single()
            .expect("valid Sunday timestamp");
        let monday_start = Utc
            .with_ymd_and_hms(2024, 12, 30, 0, 0, 0)
            .single()
            .expect("valid Monday timestamp");
        let (s1, c1, ds1, dc1) = encode(sunday_end);
        let (s2, c2, ds2, dc2) = encode(monday_start);
        assert!(
            (s1 - s2).abs() < WRAP_TOL,
            "hour sin discontinuous: {s1} vs {s2}"
        );
        assert!(
            (c1 - c2).abs() < WRAP_TOL,
            "hour cos discontinuous: {c1} vs {c2}"
        );
        assert!(
            (ds1 - ds2).abs() < WRAP_TOL,
            "dow sin discontinuous: {ds1} vs {ds2}"
        );
        assert!(
            (dc1 - dc2).abs() < WRAP_TOL,
            "dow cos discontinuous: {dc1} vs {dc2}"
        );
    }
}
