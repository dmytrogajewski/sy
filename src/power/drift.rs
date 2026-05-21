//! Drift detection — sy-power Step 30.
//!
//! Two in-house concept-drift detectors operating on residual streams
//! produced by Phase R5's forecaster (GRU) and Phase R4's bandit
//! (CLUCB):
//!
//! * `Adwin` — Bifet & Gavalda 2007 adaptive windowing. Splits the
//!   live window into every two contiguous sub-windows and tests the
//!   mean gap against a Hoeffding bound; the older half is shrunk
//!   whenever any cut fails the test, which is also when the alarm
//!   fires. We feed it the forecast residual.
//! * `Ddm` — Gama et al. 2004 drift detection method. Tracks the
//!   running mean (`p`) and stddev (`s`) of a binary error stream
//!   and compares the current `p + s` against the best-so-far
//!   `p_min + s_min`. Two thresholds → `Warning` (`> p_min +
//!   2 s_min`) and `Alarm` (`> p_min + 3 s_min`). We feed it the
//!   bandit reward residual binarised against the rules baseline.
//!
//! Both detectors are pure (their only state lives inside `&mut
//! self`) so the daemon (Step 31) can own one per stream without
//! reaching into globals.
//!
//! See `specs/research/sy-power/SPEC.md` §3 ("Drift response — drop
//! to rules-only") and §4 (`drift.adwin_alarm`/`drift.ddm_warning`
//! status fields).

use std::collections::VecDeque;

/// Signal emitted by each detector after observing one sample. The
/// daemon (Step 31) interprets `Warning` as advisory (e.g. status JSON
/// surface only) and `Alarm` as the trigger to drop to rules-only and
/// schedule a retrain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftSignal {
    /// Stream looks stationary; no action required.
    Stable,
    /// Stream is degrading but not yet past the alarm threshold.
    Warning,
    /// Stream has changed; the daemon should drop to baseline.
    Alarm,
}

/// Bifet & Gavalda 2007 adaptive-windowing concept-drift detector.
///
/// The window holds the most recent observations; on each `observe`
/// we evaluate every two-way split `W = W0 ++ W1` and shrink `W0`
/// (drop the oldest sample) whenever `|mean(W0) - mean(W1)|` exceeds
/// the Hoeffding bound derived from the configured `delta`. The
/// alarm fires on the same tick the cut happens.
pub struct Adwin {
    window: VecDeque<f32>,
    /// Hoeffding confidence; clamped on construction. `pub(crate)`
    /// so [`DriftDetector::reset`] can rebuild the detector with the
    /// same sensitivity without re-threading the original argument.
    pub(crate) delta: f32,
    /// Hard cap on window length; ADWIN's textbook formulation is
    /// unbounded but the daemon's residual stream is high-rate so we
    /// cap at `MAX_WINDOW` to keep the per-tick cost O(MAX_WINDOW).
    max_window: usize,
}

/// ADWIN sensitivity. The Bifet paper recommends `delta = 0.002` as
/// the "high confidence, low false-positive" default; the daemon
/// uses the same value.
pub const ADWIN_DEFAULT_DELTA: f32 = 0.002;

/// Window cap. 2048 covers ~17 minutes of forecast residuals at the
/// daemon's 2 Hz tick rate, which is more than enough lookback to
/// detect daily drift without paying for an unbounded VecDeque.
pub const ADWIN_MAX_WINDOW: usize = 2048;

impl Adwin {
    /// Construct an ADWIN with the given confidence. `delta` must be
    /// in `(0, 1)`; values outside that range are clamped to the
    /// default so the detector never panics on a misconfigured
    /// daemon.
    pub fn new(delta: f32) -> Self {
        let delta = if delta > 0.0 && delta < 1.0 {
            delta
        } else {
            ADWIN_DEFAULT_DELTA
        };
        Self {
            window: VecDeque::new(),
            delta,
            max_window: ADWIN_MAX_WINDOW,
        }
    }

    /// Push one observation and return the drift verdict for this
    /// tick. The detector is purely additive: only the internal
    /// window changes.
    pub fn observe(&mut self, x: f32) -> DriftSignal {
        if !x.is_finite() {
            // NaN/Inf would poison the running sums; drop silently.
            // The daemon already gates on `Snapshot::is_well_formed`,
            // so in practice this only triggers on malformed test
            // fixtures.
            return DriftSignal::Stable;
        }
        self.window.push_back(x);
        if self.window.len() > self.max_window {
            self.window.pop_front();
        }
        let n = self.window.len();
        if n < ADWIN_MIN_SPLIT * 2 {
            return DriftSignal::Stable;
        }
        // Walk every split `i in [MIN_SPLIT, n - MIN_SPLIT]`. Shrink
        // from the head whenever a cut exceeds the Hoeffding bound;
        // alarm if any cut shrinks.
        let mut alarm = false;
        loop {
            let n_now = self.window.len();
            if n_now < ADWIN_MIN_SPLIT * 2 {
                break;
            }
            let var_w = window_variance(&self.window);
            // Only inspect splits at exponentially-spaced offsets
            // from both ends. This is Bifet & Gavalda's bucket trick
            // expressed without explicit bucket bookkeeping: the
            // mean gap is monotone in `i` for piecewise stationary
            // streams, so a coarse stride loses no detection power
            // but keeps each tick O(log² n) instead of O(n²).
            let mut cut = false;
            for i in adwin_split_offsets(n_now) {
                let (mean0, mean1) = split_means(&self.window, i);
                let eps = adwin_bound(i, n_now - i, var_w, self.delta);
                if (mean0 - mean1).abs() > eps {
                    cut = true;
                    break;
                }
            }
            if cut {
                self.window.pop_front();
                alarm = true;
            } else {
                break;
            }
        }
        if alarm {
            DriftSignal::Alarm
        } else {
            DriftSignal::Stable
        }
    }

    /// Read-only window length. Exposed so the daemon's status JSON
    /// (Step 31) can surface the live lookback to the operator.
    pub fn window_len(&self) -> usize {
        self.window.len()
    }
}

/// Smallest sub-window the ADWIN cut search considers. Below 30 the
/// Hoeffding bound is so wide that any reasonable mean gap fits
/// inside it, which produces noise rather than signal.
const ADWIN_MIN_SPLIT: usize = 30;

/// Exponentially-spaced split offsets in `[ADWIN_MIN_SPLIT, n -
/// ADWIN_MIN_SPLIT]`. Yields offsets at both ends of the window
/// (e.g. for n=1024: 30, 60, 120, …, 480, then 544, 904, 964, 994)
/// so the cut search is O(log n) per tick instead of O(n).
fn adwin_split_offsets(n: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if n < ADWIN_MIN_SPLIT * 2 {
        return out;
    }
    let lo = ADWIN_MIN_SPLIT;
    let hi = n - ADWIN_MIN_SPLIT;
    let mut step = lo;
    while step <= hi - lo {
        out.push(step);
        if n.saturating_sub(step) >= lo && n.saturating_sub(step) <= hi {
            out.push(n - step);
        }
        step = step.saturating_mul(2);
        if step == 0 {
            break;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Compute `(mean(window[..i]), mean(window[i..]))` in one pass.
/// Pure: no allocation, no panics for `i` in `[1, window.len() - 1]`.
fn split_means(window: &VecDeque<f32>, i: usize) -> (f32, f32) {
    let mut sum0 = 0.0f64;
    let mut sum1 = 0.0f64;
    for (idx, v) in window.iter().enumerate() {
        if idx < i {
            sum0 += *v as f64;
        } else {
            sum1 += *v as f64;
        }
    }
    let n0 = i as f64;
    let n1 = (window.len() - i) as f64;
    ((sum0 / n0) as f32, (sum1 / n1) as f32)
}

/// Variance-aware ADWIN2 bound (Bifet & Gavalda 2007 §3.2, the
/// "improved" version used by MOA). Tighter than the plain Hoeffding
/// when the residual stream's standard deviation is small — which is
/// exactly our regime (forecast residuals on a stationary stream
/// run ~0.1 std). `var_w` is the population variance of the full
/// window, `delta` the per-detector confidence, and the per-cut
/// confidence is Bonferroni-corrected by the window length `n`.
fn adwin_bound(n0: usize, n1: usize, var_w: f32, delta: f32) -> f32 {
    let m_inv = 1.0 / (n0 as f32 - 0.5) + 1.0 / (n1 as f32 - 0.5);
    let n = (n0 + n1) as f32;
    let delta_prime = delta / n;
    let log_term = (2.0 / delta_prime).ln();
    let var_term = (2.0 * var_w * m_inv * log_term).sqrt();
    let bias_term = (2.0 / 3.0) * log_term * m_inv;
    var_term + bias_term
}

/// Population variance of the live window. Used by [`adwin_bound`];
/// kept local to avoid pulling in `statrs`.
fn window_variance(window: &VecDeque<f32>) -> f32 {
    let n = window.len();
    if n == 0 {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for v in window.iter() {
        sum += *v as f64;
    }
    let mean = sum / n as f64;
    let mut sq = 0.0f64;
    for v in window.iter() {
        let d = *v as f64 - mean;
        sq += d * d;
    }
    (sq / n as f64) as f32
}

/// Gama et al. 2004 drift detection method on a binary error stream.
///
/// Maintains the running Bernoulli mean (`p`) and standard deviation
/// (`s = sqrt(p (1 - p) / n)`), plus the best-so-far minimum
/// (`p_min + s_min`). The detector emits `Warning` when the current
/// `p + s` exceeds `p_min + 2 s_min` and `Alarm` at `p_min + 3
/// s_min`.
pub struct Ddm {
    n: u32,
    /// Total error count (Welford-style would over-engineer this; the
    /// Bernoulli running mean only needs the sum).
    errors: u32,
    p_min: f32,
    s_min: f32,
}

/// DDM warmup before the running minimum is meaningful. Gama et al.
/// recommend ≥ 30 for sensitivity; on the daemon's high-rate residual
/// stream we want the running minimum to settle near the *true* mean
/// before any threshold check, which empirically takes ~200 samples
/// on a Bernoulli(p≈0.1) stream. Lower values latch onto a chance-low
/// `p_min` and trigger spurious alarms on stationary streams.
pub const DDM_MIN_SAMPLES: u32 = 200;

impl Default for Ddm {
    fn default() -> Self {
        Self::new()
    }
}

impl Ddm {
    /// Construct a DDM with the running minimum primed to "infinity"
    /// so the first comparison can only lower it.
    pub fn new() -> Self {
        Self {
            n: 0,
            errors: 0,
            p_min: f32::INFINITY,
            s_min: f32::INFINITY,
        }
    }

    /// Observe one binary error sample (`true` = error). Returns the
    /// drift verdict for this tick. Pure: only updates `&mut self`.
    pub fn observe(&mut self, error: bool) -> DriftSignal {
        self.n = self.n.saturating_add(1);
        if error {
            self.errors = self.errors.saturating_add(1);
        }
        if self.n < DDM_MIN_SAMPLES {
            return DriftSignal::Stable;
        }
        // Laplace-smoothed Bernoulli estimate (add-one prior). The
        // unsmoothed version latches `p_min = 0` whenever the warmup
        // happens to draw zero errors by chance — `p_min + 3 s_min = 0`
        // then alarms on every subsequent error. Add-one smoothing
        // keeps `p_min` strictly positive without distorting the
        // post-warmup estimate.
        let p = (self.errors as f32 + 1.0) / (self.n as f32 + 2.0);
        let s = (p * (1.0 - p) / self.n as f32).sqrt();
        if p + s < self.p_min + self.s_min {
            self.p_min = p;
            self.s_min = s;
        }
        let warning_thresh = self.p_min + 2.0 * self.s_min;
        let alarm_thresh = self.p_min + 3.0 * self.s_min;
        if p + s > alarm_thresh {
            DriftSignal::Alarm
        } else if p + s > warning_thresh {
            DriftSignal::Warning
        } else {
            DriftSignal::Stable
        }
    }
}

/// SPEC §4 `sy.power.status/v1` `drift` block. Step 31's daemon
/// publishes this each tick into a shared `LatestDriftStatus` slot;
/// the CLI's `sy power status` reads it back through the IPC
/// `StatusResponse` and exits 3 (`EXIT_DRIFT_ACTIVE`) when
/// `adwin_alarm == true`. Serializable so the IPC wire frame can
/// carry it directly; `Default` is "all-clear", matching the
/// daemon's first-tick state.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DriftStatus {
    /// `true` while the ADWIN detector on the forecast-residual
    /// stream is in alarm. Drives the daemon's "drop to rules-only"
    /// gate; cleared after a successful retrain.
    pub adwin_alarm: bool,
    /// `true` while the DDM detector on the reward-residual stream
    /// is past its Warning threshold but below Alarm. Advisory only —
    /// surfaced to the operator via status JSON; does not gate the
    /// bandit.
    pub ddm_warning: bool,
    /// Wall-clock instant of the most recent ADWIN alarm fire.
    /// `None` until the first alarm; persists past the clear so an
    /// operator can see how long ago the last drift event was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_alarm_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Composite detector wrapping one ADWIN (continuous residual) and
/// one DDM (binarised reward residual). Step 31's daemon owns one
/// instance and feeds the forecast residual to ADWIN and the bandit
/// reward residual to DDM each tick.
pub struct DriftDetector {
    pub forecast: Adwin,
    pub reward: Ddm,
}

impl Default for DriftDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl DriftDetector {
    pub fn new() -> Self {
        Self {
            forecast: Adwin::new(ADWIN_DEFAULT_DELTA),
            reward: Ddm::new(),
        }
    }

    /// Drop every observation in both sub-detectors so a successful
    /// retrain returns the daemon to a clean baseline. Step 31's
    /// `drift_clears_after_successful_retrain` test pins this
    /// behaviour: ADWIN's window empties, DDM's running minimum
    /// resets to `+inf`, so the next stationary stream cannot
    /// re-trigger on stale state.
    pub fn reset(&mut self) {
        self.forecast = Adwin::new(self.forecast.delta);
        self.reward = Ddm::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic Box-Muller sampler over a 32-bit xorshift PRNG.
    /// Keeps the drift tests hermetic without dragging `rand` into
    /// `Cargo.toml` (no existing power module needs it).
    struct GaussRng {
        state: u32,
    }
    impl GaussRng {
        fn new(seed: u32) -> Self {
            Self {
                state: if seed == 0 { 0xDEAD_BEEF } else { seed },
            }
        }
        fn next_u32(&mut self) -> u32 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.state = x;
            x
        }
        fn next_uniform(&mut self) -> f32 {
            // Map to (0, 1] — Box-Muller needs strictly positive.
            let v = self.next_u32() as f32 / u32::MAX as f32;
            if v <= 0.0 {
                f32::EPSILON
            } else {
                v
            }
        }
        fn next_normal(&mut self, mean: f32, std: f32) -> f32 {
            let u1 = self.next_uniform();
            let u2 = self.next_uniform();
            let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos();
            mean + std * z
        }
    }

    /// Bifet's textbook ADWIN benchmark: 1000 samples from
    /// `N(0.5, 0.1)` then 1000 from `N(0.8, 0.1)`. The alarm must
    /// fire within ±50 samples of the true change point at 1000.
    const BIFET_CHANGE_POINT: usize = 1000;
    const BIFET_TOLERANCE: usize = 50;

    #[test]
    fn adwin_classic_bifet_dataset() {
        let mut rng = GaussRng::new(0xB1FE_7000);
        let mut adwin = Adwin::new(ADWIN_DEFAULT_DELTA);
        let mut first_alarm: Option<usize> = None;
        for i in 0..(BIFET_CHANGE_POINT * 2) {
            let mean = if i < BIFET_CHANGE_POINT { 0.5 } else { 0.8 };
            let x = rng.next_normal(mean, 0.1);
            if let DriftSignal::Alarm = adwin.observe(x) {
                if first_alarm.is_none() {
                    first_alarm = Some(i);
                    break;
                }
            }
        }
        let fired_at = first_alarm.expect("adwin should fire on the bifet sequence");
        let lo = BIFET_CHANGE_POINT.saturating_sub(BIFET_TOLERANCE);
        let hi = BIFET_CHANGE_POINT + BIFET_TOLERANCE;
        assert!(
            (lo..=hi).contains(&fired_at),
            "adwin alarm at {fired_at}, want in [{lo}, {hi}]"
        );
    }

    /// DDM emits Warning strictly before Alarm on a ramp where the
    /// Bernoulli error rate climbs from 0.1 → 0.5 over 600 samples.
    #[test]
    fn ddm_warning_precedes_alarm() {
        let mut rng = GaussRng::new(0xDD33_0001);
        let mut ddm = Ddm::new();
        let mut warn_at: Option<usize> = None;
        let mut alarm_at: Option<usize> = None;
        // Warmup: 200 samples at the low error rate so `p_min`/`s_min`
        // settle on the "good" regime before the ramp begins.
        for i in 0..200 {
            let err = rng.next_uniform() < 0.1;
            match ddm.observe(err) {
                DriftSignal::Warning if warn_at.is_none() => warn_at = Some(i),
                DriftSignal::Alarm if alarm_at.is_none() => alarm_at = Some(i),
                _ => {}
            }
        }
        // Ramp: 600 samples, error rate climbs 0.1 → 0.5.
        for j in 0..600 {
            let rate = 0.1 + (j as f32 / 600.0) * 0.4;
            let err = rng.next_uniform() < rate;
            let i = 200 + j;
            match ddm.observe(err) {
                DriftSignal::Warning if warn_at.is_none() => warn_at = Some(i),
                DriftSignal::Alarm if alarm_at.is_none() => alarm_at = Some(i),
                _ => {}
            }
            if warn_at.is_some() && alarm_at.is_some() {
                break;
            }
        }
        let w = warn_at.expect("ddm should emit Warning during ramp");
        let a = alarm_at.expect("ddm should emit Alarm during ramp");
        assert!(w <= a, "Warning at {w} must precede Alarm at {a}");
    }

    /// 10k stationary samples produce zero Warnings and zero Alarms
    /// on both detectors.
    #[test]
    fn no_false_alarm_on_stationary_stream() {
        const N: usize = 10_000;
        let mut rng = GaussRng::new(0x5747_1042);
        let mut adwin = Adwin::new(ADWIN_DEFAULT_DELTA);
        let mut ddm = Ddm::new();
        let mut adwin_alarms = 0;
        let mut ddm_alarms = 0;
        let mut ddm_warnings = 0;
        for _ in 0..N {
            let x = rng.next_normal(0.5, 0.1);
            if let DriftSignal::Alarm = adwin.observe(x) {
                adwin_alarms += 1;
            }
            // DDM consumes a binary stream; use a deterministic
            // low-rate Bernoulli error.
            let err = rng.next_uniform() < 0.1;
            match ddm.observe(err) {
                DriftSignal::Warning => ddm_warnings += 1,
                DriftSignal::Alarm => ddm_alarms += 1,
                DriftSignal::Stable => {}
            }
        }
        assert_eq!(adwin_alarms, 0, "stationary stream produced ADWIN alarms");
        assert_eq!(ddm_alarms, 0, "stationary stream produced DDM alarms");
        assert_eq!(ddm_warnings, 0, "stationary stream produced DDM warnings");
    }
}
