//! Rules-baseline arm picker (Roadmap Step 18).
//!
//! `rules_baseline(state, snapshot, cfg) -> &str` is a pure function:
//! deterministic given its three inputs, no clock reads, no I/O. It
//! returns the canonical arm name configured for `state` in
//! `[rules_baseline]`. The caller (`shield::project`) resolves the
//! name to an `Arm` via `bandit::load_arms`.
//!
//! The `snapshot` argument is unused today — every state currently
//! maps to a single arm regardless of feature values. It is part of
//! the signature because SPEC §4 leaves room for future refinements
//! (e.g. distinguishing emergency vs low DC SOC, or splitting
//! WARM_AC by Tctl band). Including the parameter now keeps the
//! call sites stable when the table grows in a later step.
//!
//! The roadmap text reads "BATTERY_LOW → quiet" but `quiet` is a
//! `platform_profile` value, not a canonical arm name. The lowest-
//! power canonical arm shipped in `configs/sy/power.toml` is
//! `whisper` (platform = quiet, epp = power), and that is what
//! `RulesBaselineConfig::default()` returns.

use crate::power::config::RulesBaselineConfig;
use crate::power::shield::ShieldState;
use crate::power::snapshot::Snapshot;

/// Pure-function rules baseline. Returns the canonical arm name
/// configured for `state`. `_snapshot` is reserved for future
/// state-splitting refinements (see module docs).
pub fn rules_baseline<'a>(
    state: ShieldState,
    _snapshot: &Snapshot,
    cfg: &'a RulesBaselineConfig,
) -> &'a str {
    match state {
        ShieldState::CoolAc => cfg.cool_ac.as_str(),
        ShieldState::WarmAc => cfg.warm_ac.as_str(),
        ShieldState::Hot => cfg.hot.as_str(),
        ShieldState::BatteryLow => cfg.battery_low.as_str(),
        ShieldState::Meeting => cfg.meeting.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
    use chrono::{TimeZone, Utc};

    fn pinned_snapshot() -> Snapshot {
        Snapshot {
            schema: SCHEMA_ID,
            ts: Utc
                .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
                .single()
                .expect("pinned UTC"),
            features: [0.0_f32; FEATURE_LEN],
            raw: SnapshotRaw::default(),
            snapshot_hash: "0".repeat(64),
        }
    }

    /// SPEC §4 floor: every `ShieldState` variant maps to exactly one
    /// non-empty arm name. The match is exhaustive — adding a new
    /// state without extending the table is a compile-time failure
    /// inside `rules_baseline`.
    #[test]
    fn baseline_table_total() {
        let cfg = RulesBaselineConfig::default();
        let snap = pinned_snapshot();
        for state in [
            ShieldState::CoolAc,
            ShieldState::WarmAc,
            ShieldState::Hot,
            ShieldState::BatteryLow,
            ShieldState::Meeting,
        ] {
            let arm = rules_baseline(state, &snap, &cfg);
            assert!(
                !arm.is_empty(),
                "baseline must map every state to a non-empty arm; missing {state:?}",
            );
        }
    }

    /// The shipped `configs/sy/power.toml` MUST include a
    /// `[rules_baseline]` stanza whose entries match every canonical
    /// arm name; a typo here would silently degrade the daemon to the
    /// `fallback_arm` path on every tick.
    #[test]
    fn shipped_config_baseline_resolves_to_canonical_arms() {
        use crate::power::config::PowerConfig;
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml");
        let cfg = PowerConfig::load(&path).expect("shipped power.toml parses");
        let names: Vec<&str> = cfg.arms.iter().map(|a| a.name.as_str()).collect();
        for s in [
            cfg.rules_baseline.cool_ac.as_str(),
            cfg.rules_baseline.warm_ac.as_str(),
            cfg.rules_baseline.hot.as_str(),
            cfg.rules_baseline.battery_low.as_str(),
            cfg.rules_baseline.meeting.as_str(),
        ] {
            assert!(
                names.contains(&s),
                "rules_baseline arm {s:?} must exist in shipped arms list",
            );
        }
    }

    /// Deterministic given (state, snapshot, cfg) — back-to-back calls
    /// return the same arm name without side effects.
    #[test]
    fn baseline_is_deterministic() {
        let cfg = RulesBaselineConfig::default();
        let snap = pinned_snapshot();
        let a = rules_baseline(ShieldState::Hot, &snap, &cfg);
        let b = rules_baseline(ShieldState::Hot, &snap, &cfg);
        assert_eq!(a, b);
        assert_eq!(a, "idle");
    }
}
