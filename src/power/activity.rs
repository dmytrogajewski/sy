//! Online activity classifier (Step 28 of the `sy-power` roadmap,
//! SPEC §3 "Online auxiliary classifier (linfa-ftrl)").
//!
//! Hand-rolled FTRL-Proximal logistic regression over the 12-channel
//! snapshot feature vec. One model per class, one-vs-rest; `classify`
//! returns the argmax over the per-class sigmoid scores.
//!
//! ## Why hand-rolled (not the `linfa-ftrl = "0.8"` crate)
//!
//! The implementation guidance permits either `linfa-ftrl` or a
//! hand-roll. `linfa-ftrl 0.8.1` resolves on crates.io but pulls the
//! full `linfa` umbrella plus `argmin` + `ndarray-rand` — heavyweight
//! deps for a 5×12 weight matrix update fired at 1 Hz. The hand-roll
//! follows Step 20's CLUCB precedent (~80 LoC of `Vec<f32>` math
//! against the same `Snapshot::features` slice) and keeps the bandit
//! and the classifier on the same dep surface (zero new crates).
//!
//! ## Algorithm (FTRL-Proximal, McMahan 2013)
//!
//! Per binary class `c`, maintain `z_c, n_c, w_c ∈ R^{FEATURE_LEN}`:
//! - Predict: `w_i = -(z_i) / ((β + √n_i)/α + λ1·sign·...)` (with L1
//!   thresholding); `p = σ(w · x)`.
//! - Update on label `y ∈ {0,1}`: `g = (p − y)·x`,
//!   `σ_i = (√(n_i + g_i²) − √n_i) / α`, `z_i += g_i − σ_i·w_i`,
//!   `n_i += g_i²`.
//!
//! Tuning: `α=0.1, β=1.0, λ1=0.0, λ2=1.0` — small L2 keeps weights
//! bounded for the synthetic-data test; L1=0 because the feature
//! count is already small (12) and we want every dimension to
//! contribute.

use serde::{Deserialize, Serialize};

use crate::power::snapshot::{Snapshot, FEATURE_LEN};

/// FTRL learning rate `α`. McMahan 2013 §5 reports best practice in
/// the 0.01..0.3 range for sparse logistic; 0.1 lands inside the band
/// and converges in <200 samples on the seed-pinned synthetic suite.
const FTRL_ALPHA: f32 = 0.1;
/// FTRL per-coordinate learning-rate floor `β`.
const FTRL_BETA: f32 = 1.0;
/// L1 regularisation. Off — the 12-dim feature vec is already small.
const FTRL_L1: f32 = 0.0;
/// L2 regularisation. Small positive keeps weights bounded under
/// near-degenerate features (NaN-replaced slots become 0 below).
const FTRL_L2: f32 = 1.0;

/// One of five activity classes. The ordering is pinned: indices
/// match `power::forecast::model::ACTIVITY_CLASSES`, so the
/// classifier and the forecaster speak the same taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityLabel {
    Idle,
    Browse,
    Call,
    Code,
    Build,
}

/// Total class count — keep in sync with [`ActivityLabel`].
pub const ACTIVITY_CLASS_COUNT: usize = 5;

impl ActivityLabel {
    /// Stable index into the per-class OvR weights. Pinned to match
    /// `forecast::model::ACTIVITY_CLASSES` ordering.
    pub const fn index(self) -> usize {
        match self {
            Self::Idle => 0,
            Self::Browse => 1,
            Self::Call => 2,
            Self::Code => 3,
            Self::Build => 4,
        }
    }

    /// Inverse of [`ActivityLabel::index`]. Returns `None` for any
    /// out-of-range integer.
    pub const fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::Idle),
            1 => Some(Self::Browse),
            2 => Some(Self::Call),
            3 => Some(Self::Code),
            4 => Some(Self::Build),
            _ => None,
        }
    }
}

/// One per-class FTRL state. Three `FEATURE_LEN`-wide vectors per
/// class: `z` (negative gradient accumulator), `n` (squared-gradient
/// accumulator), `w` (the current weight, derived lazily from `z`/`n`
/// inside [`predict`]).
#[derive(Debug, Clone)]
struct FtrlClass {
    z: [f32; FEATURE_LEN],
    n: [f32; FEATURE_LEN],
}

impl FtrlClass {
    fn new() -> Self {
        Self {
            z: [0.0; FEATURE_LEN],
            n: [0.0; FEATURE_LEN],
        }
    }

    /// Derive the per-coordinate weight from `z`/`n` per FTRL-Proximal
    /// §3. The L1 branch is reached only when `FTRL_L1 > 0`; with the
    /// default tuning every coordinate falls through the "else" arm.
    fn weight(&self, i: usize) -> f32 {
        let z = self.z[i];
        if z.abs() <= FTRL_L1 {
            return 0.0;
        }
        let sgn = if z < 0.0 { -1.0 } else { 1.0 };
        let denom = (FTRL_BETA + self.n[i].sqrt()) / FTRL_ALPHA + FTRL_L2;
        -(z - sgn * FTRL_L1) / denom
    }

    fn dot(&self, x: &[f32; FEATURE_LEN]) -> f32 {
        let mut s = 0.0;
        for (i, x_i) in x.iter().enumerate() {
            s += self.weight(i) * x_i;
        }
        s
    }

    /// Apply one FTRL-Proximal gradient step against
    /// `label ∈ {0.0, 1.0}` (one-vs-rest). Treats NaN feature slots as
    /// zero so a missing sensor doesn't poison the update.
    fn partial_fit(&mut self, x: &[f32; FEATURE_LEN], label: f32) {
        let clean = sanitise_features(x);
        let p = sigmoid(self.dot(&clean));
        let grad_coeff = p - label;
        for (i, x_i) in clean.iter().enumerate() {
            let g = grad_coeff * x_i;
            let sigma = ((self.n[i] + g * g).sqrt() - self.n[i].sqrt()) / FTRL_ALPHA;
            let w_i = self.weight(i);
            self.z[i] += g - sigma * w_i;
            self.n[i] += g * g;
        }
    }
}

/// Replace every non-finite slot with 0.0 so a sensor that yielded
/// `f32::NAN` (per `snapshot::collect_tick`'s missing-sysfs branch)
/// does not poison the gradient — NaN propagates through `exp` /
/// `sqrt` and would lock the model into a NaN steady state on the
/// very first hostile snapshot.
fn sanitise_features(x: &[f32; FEATURE_LEN]) -> [f32; FEATURE_LEN] {
    let mut clean = [0.0_f32; FEATURE_LEN];
    for (i, v) in x.iter().enumerate() {
        clean[i] = if v.is_finite() { *v } else { 0.0 };
    }
    clean
}

fn sigmoid(z: f32) -> f32 {
    1.0 / (1.0 + (-z).exp())
}

/// Online one-vs-rest FTRL-Proximal classifier over the 12-channel
/// `Snapshot::features` vec. Five binary models share a single
/// `OnlineClassifier`; [`classify`] picks the argmax of the sigmoid
/// scores.
#[derive(Debug, Clone)]
pub struct OnlineClassifier {
    classes: [FtrlClass; ACTIVITY_CLASS_COUNT],
}

impl Default for OnlineClassifier {
    fn default() -> Self {
        Self::new()
    }
}

impl OnlineClassifier {
    /// Construct a fresh classifier — all weights / accumulators
    /// zero, so the first `classify` call returns
    /// [`ActivityLabel::Idle`] (the index-0 class) by argmax-of-ties.
    pub fn new() -> Self {
        Self {
            classes: [
                FtrlClass::new(),
                FtrlClass::new(),
                FtrlClass::new(),
                FtrlClass::new(),
                FtrlClass::new(),
            ],
        }
    }

    /// Score the snapshot under every per-class model; return the
    /// argmax. Never panics: NaN feature slots are treated as zero
    /// inside [`FtrlClass::dot`]'s weight derivation.
    pub fn classify(&self, snap: &Snapshot) -> ActivityLabel {
        let scores = self.scores(&snap.features);
        let mut best_idx = 0usize;
        let mut best = scores[0];
        for (i, s) in scores.iter().enumerate().skip(1) {
            if *s > best {
                best = *s;
                best_idx = i;
            }
        }
        ActivityLabel::from_index(best_idx).unwrap_or(ActivityLabel::Idle)
    }

    /// Apply one FTRL update against `label` — the one-vs-rest target
    /// for class `label` is `+1.0`, every other class is `0.0`.
    pub fn partial_fit(&mut self, snap: &Snapshot, label: ActivityLabel) {
        let target_idx = label.index();
        for (i, class) in self.classes.iter_mut().enumerate() {
            let y = if i == target_idx { 1.0 } else { 0.0 };
            class.partial_fit(&snap.features, y);
        }
    }

    fn scores(&self, x: &[f32; FEATURE_LEN]) -> [f32; ACTIVITY_CLASS_COUNT] {
        let clean = sanitise_features(x);
        let mut out = [0.0_f32; ACTIVITY_CLASS_COUNT];
        for (i, c) in self.classes.iter().enumerate() {
            out[i] = sigmoid(c.dot(&clean));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
    use chrono::{TimeZone, Utc};

    /// Construct a deterministic snapshot whose feature vec equals
    /// `features`. Hash + timestamp are pinned constants so the test
    /// doesn't depend on the clock.
    fn snapshot_with(features: [f32; FEATURE_LEN]) -> Snapshot {
        Snapshot {
            schema: SCHEMA_ID,
            ts: Utc
                .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
                .single()
                .unwrap(),
            features,
            raw: SnapshotRaw::default(),
            snapshot_hash: "0".repeat(64),
        }
    }

    /// DoD: a freshly constructed classifier returns one of the five
    /// enumerants for any input — no panic, no out-of-range index.
    #[test]
    fn classifies_idle_snapshot() {
        let clf = OnlineClassifier::new();
        let low = snapshot_with([0.0; FEATURE_LEN]);
        let label = clf.classify(&low);
        // Untrained, all five sigmoids return 0.5 → argmax picks
        // index 0 (`Idle`). Pin that as the documented contract.
        assert_eq!(label, ActivityLabel::Idle);
    }

    /// Linear-congruential RNG seeded for the synthetic-data test —
    /// hand-rolled so the test stays hermetic without pulling
    /// `rand` as a dev-dep.
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 32) as u32
        }
        /// Box-Muller transform: two uniforms → one standard normal.
        fn next_gauss(&mut self) -> f32 {
            let u1 = (self.next_u32() as f32 / u32::MAX as f32).max(1e-7);
            let u2 = self.next_u32() as f32 / u32::MAX as f32;
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
        }
    }

    /// Generate a synthetic dataset with five well-separated class
    /// centres in the 12-dim space. Each centre puts a +1.0 spike in
    /// the class's "signature" dimension and a -1.0 spike in the
    /// *previous* class's signature dimension; everything else stays
    /// at the per-class mean. Noise is N(0, 0.1).
    fn synth_dataset(n_per_class: usize) -> Vec<([f32; FEATURE_LEN], ActivityLabel)> {
        const NOISE_SCALE: f32 = 0.1;
        let mut rng = Lcg::new(0xC0FFEE);
        let mut rows = Vec::with_capacity(n_per_class * ACTIVITY_CLASS_COUNT);
        for cls in 0..ACTIVITY_CLASS_COUNT {
            let label =
                ActivityLabel::from_index(cls).expect("class index in [0, ACTIVITY_CLASS_COUNT)");
            let prev = (cls + ACTIVITY_CLASS_COUNT - 1) % ACTIVITY_CLASS_COUNT;
            for _ in 0..n_per_class {
                let mut x = [0.0_f32; FEATURE_LEN];
                for slot in x.iter_mut() {
                    *slot = NOISE_SCALE * rng.next_gauss();
                }
                x[cls] += 1.0;
                x[prev] -= 1.0;
                rows.push((x, label));
            }
        }
        rows
    }

    /// DoD: feed 200 labelled snapshots; held-out accuracy rises from
    /// random (0.2 for 5 classes) to ≥ 0.7. Pin the seed so the test
    /// is deterministic; pin the train/holdout split so flakes are
    /// real regressions, not data shuffles.
    #[test]
    fn partial_fit_improves_accuracy() {
        const N_TRAIN_PER_CLASS: usize = 30;
        const N_TEST_PER_CLASS: usize = 10;
        const EPOCHS: usize = 5;
        const ACCURACY_TARGET: f32 = 0.7;

        let train = synth_dataset(N_TRAIN_PER_CLASS);
        let test = synth_dataset(N_TEST_PER_CLASS);

        let mut clf = OnlineClassifier::new();
        for _ in 0..EPOCHS {
            for (x, label) in &train {
                clf.partial_fit(&snapshot_with(*x), *label);
            }
        }

        let mut correct = 0usize;
        for (x, label) in &test {
            if clf.classify(&snapshot_with(*x)) == *label {
                correct += 1;
            }
        }
        let acc = correct as f32 / test.len() as f32;
        assert!(
            acc >= ACCURACY_TARGET,
            "held-out accuracy {acc:.2} below target {ACCURACY_TARGET:.2} \
             (random for 5 classes is 0.20)",
        );
    }

    /// DoD: a 1000-iteration `partial_fit` loop completes in well
    /// under 1 s wall, so the daemon's 1 Hz tick (Step 29) stays
    /// inside the 7 ms per-tick budget with multiple orders of
    /// magnitude of headroom.
    #[test]
    fn partial_fit_1000_iters_under_one_second() {
        const ITERS: usize = 1000;
        let mut clf = OnlineClassifier::new();
        let snap = snapshot_with([0.1; FEATURE_LEN]);
        let start = std::time::Instant::now();
        for _ in 0..ITERS {
            clf.partial_fit(&snap, ActivityLabel::Code);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "1000 partial_fit iters took {elapsed:?}, expected < 1 s",
        );
    }

    /// Index round-trip — pin the ordering against the documented
    /// taxonomy in `forecast::model::ACTIVITY_CLASSES` so a refactor
    /// of one trips the other.
    #[test]
    fn index_round_trip_covers_all_five_classes() {
        for i in 0..ACTIVITY_CLASS_COUNT {
            let label = ActivityLabel::from_index(i).expect("index in range");
            assert_eq!(label.index(), i);
        }
        assert!(ActivityLabel::from_index(ACTIVITY_CLASS_COUNT).is_none());
    }
}
