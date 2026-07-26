//! Read-only adapter over the `crate::power` daemon's in-memory
//! bandit/shield state. Per the sy-mon SPEC §4 `SystemSnapshot` JSON
//! example, the `power` panel surfaces three values:
//!
//! * `current_arm` — the bandit arm currently applied (string).
//! * `dwell_pct` — per-arm fraction of recent ticks (sums to ~1.0).
//! * `regret_cum` — cumulative counterfactual regret vs the rules
//!   baseline.
//!
//! The actual state lives in the binary crate (`src/power/`); sy-core
//! is below it in the dependency graph and cannot import it. This
//! module therefore exposes a tiny [`PowerSource`] trait that any
//! caller in the binary can implement on top of its in-memory state
//! (e.g. the power daemon's bandit registry + the `BanditMetrics`
//! shape in `src/power/report/metrics.rs`), and a pure projection
//! [`PowerSample::from_source`] that bundles the three reads into one
//! `*Sample` for [`crate::mon::snapshot::SystemSnapshot`] (Step 6).
//!
//! Nothing in this module reads sysfs, opens a file, or spawns a
//! subprocess — the read path that produces those three values is
//! owned by the power daemon. We're a thin projection over an already
//! emitted shape, not a second source of truth.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Read-only view onto the power daemon's bandit/shield state. The
/// binary's call-site (Step 11) implements this on its in-memory
/// daemon handle; sy-core unit tests fake it with a hand-rolled
/// struct (see `tests::reads_current_arm`).
///
/// `BTreeMap` keeps the resulting JSON deterministic — the
/// `dwell_pct` keys appear in alphabetical order across runs, which
/// is what the Step 6 golden snapshot for `SystemSnapshot` expects.
pub trait PowerSource {
    /// Arm token applied by the most recent bandit decision
    /// (`"perf"`, `"balanced"`, `"save"`, etc.). The SPEC §4 example
    /// shows this as a free-form string so the popup can render any
    /// arm name the power daemon ships.
    fn current_arm(&self) -> String;
    /// Per-arm dwell fraction over the daemon's rolling window. Sums
    /// to ~1.0 (within f32 rounding) when at least one tick has been
    /// observed; an empty map signals "no decisions yet".
    fn dwell_pct(&self) -> BTreeMap<String, f32>;
    /// Cumulative regret vs the rules baseline since the daemon last
    /// reset its counter. Negative values mean the bandit consumed
    /// less power than the rules baseline (the sign convention from
    /// `crate::power::report::metrics::BanditMetrics::cumulative_regret_vs_baseline`
    /// in `src/power/report/metrics.rs`).
    fn regret_cum(&self) -> f32;
}

/// One sensor tick of power-daemon state. The struct mirrors the
/// SPEC §4 `SystemSnapshot` JSON shape:
///
/// ```json
/// "power": {"current_arm": "balanced", "dwell_pct": {"perf": 0.18,
///   "balanced": 0.71, "save": 0.11}, "regret_cum": 0.034}
/// ```
///
/// Derives `Serialize` + `Deserialize` so Step 6's `SystemSnapshot`
/// can embed it without an intermediate copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerSample {
    pub current_arm: String,
    pub dwell_pct: BTreeMap<String, f32>,
    pub regret_cum: f32,
}

impl PowerSample {
    /// Project a [`PowerSource`] into the JSON-shaped struct. Pure
    /// function over the trait's three accessors — the source decides
    /// what "current" means (last decision, last 1 Hz tick, last
    /// applied arm); the sample just records what it was told.
    pub fn from_source(src: &impl PowerSource) -> Self {
        Self {
            current_arm: src.current_arm(),
            dwell_pct: src.dwell_pct(),
            regret_cum: src.regret_cum(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake `PowerSource` that returns canned values. Stands in for
    /// the binary's real bandit/shield registry — the adapter is a
    /// pure projection so a hand-rolled fake is the right shape.
    struct FakePower {
        arm: &'static str,
        dwell: &'static [(&'static str, f32)],
        regret: f32,
    }

    impl PowerSource for FakePower {
        fn current_arm(&self) -> String {
            self.arm.to_string()
        }
        fn dwell_pct(&self) -> BTreeMap<String, f32> {
            self.dwell
                .iter()
                .map(|(k, v)| ((*k).to_string(), *v))
                .collect()
        }
        fn regret_cum(&self) -> f32 {
            self.regret
        }
    }

    /// Roadmap Step 4 DoD test: the adapter reads the current arm
    /// from the source and folds it (alongside dwell + regret) into
    /// a [`PowerSample`] that matches the SPEC §4 JSON example.
    #[test]
    fn reads_current_arm() {
        const ARM: &str = "balanced";
        const REGRET: f32 = 0.034;
        let src = FakePower {
            arm: ARM,
            dwell: &[("perf", 0.18), ("balanced", 0.71), ("save", 0.11)],
            regret: REGRET,
        };
        let sample = PowerSample::from_source(&src);
        assert_eq!(sample.current_arm, ARM);
        assert_eq!(sample.regret_cum, REGRET);
        // BTreeMap keeps the keys alphabetised — Step 6's golden
        // SystemSnapshot relies on that determinism.
        let keys: Vec<&String> = sample.dwell_pct.keys().collect();
        assert_eq!(keys, vec!["balanced", "perf", "save"]);
        assert!(
            (sample.dwell_pct["balanced"] - 0.71).abs() < 1e-5,
            "dwell['balanced'] should be 0.71, got {}",
            sample.dwell_pct["balanced"],
        );
    }
}
