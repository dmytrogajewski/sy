//! Counterfactual rules-only baseline (Roadmap Step 33).
//!
//! `compute_counterfactual_baseline(entries) -> EnergyMetrics` replays
//! every audit entry's `(shield_state, snapshot)` pair through
//! [`crate::power::policy::rules::rules_baseline`] and computes the
//! [`EnergyMetrics`] **the rules-only daemon would have produced**.
//! The number is a model, not ground truth — see the SPEC §4
//! "Methodology" section that Step 35's report exposes to the
//! operator.
//!
//! ## Per-arm expected power table
//!
//! Each canonical arm gets a single fixed wattage. The table is a
//! coarse proxy keyed off the platform_profile / EPP / NPU pmode
//! tuple in `configs/sy/power.toml`; precise per-tick power is what
//! `snapshot.raw.package_power_w` already records, and the bandit
//! reward function uses that directly. The proxy here is only used
//! when the audit entry was the rules baseline's pick (we never ran
//! that arm, so we can't read its actual package power). The numbers
//! match the SPEC §4 "Bandit Arms" expected-power column — quiet
//! profiles draw <10 W, code-class profiles ~18 W, flat-out ~40 W.

use crate::power::log::AuditEntry;
use crate::power::policy::rules::rules_baseline;
use crate::power::shield::ShieldState;

use super::metrics::EnergyMetrics;

/// `whisper` — platform = quiet, EPP = power, NPU = powersaver.
pub const POWER_W_WHISPER: f32 = 8.0;
/// `idle` — platform = balanced, EPP = balance_power, NPU = default.
pub const POWER_W_IDLE: f32 = 10.0;
/// `browse` — platform = balanced, EPP = balance_performance.
pub const POWER_W_BROWSE: f32 = 12.0;
/// `call` — platform = balanced, EPP = balance_performance,
/// extra +3 W headroom for the audio/VAD chain.
pub const POWER_W_CALL: f32 = 15.0;
/// `code` — platform = balanced, EPP = balance_performance, more
/// uclamp_min headroom than `browse`.
pub const POWER_W_CODE: f32 = 18.0;
/// `npu-burst` — platform = balanced, EPP = balance_performance,
/// NPU = performance.
pub const POWER_W_NPU_BURST: f32 = 22.0;
/// `build` — platform = performance, EPP = balance_performance.
pub const POWER_W_BUILD: f32 = 28.0;
/// `flat-out` — platform = performance, EPP = performance,
/// NPU = turbo. Roof of the arm table.
pub const POWER_W_FLAT_OUT: f32 = 40.0;

/// Fallback wattage for an arm name the table does not recognise
/// (e.g. an operator-defined arm in a non-standard `power.toml`).
/// Conservative middle-of-the-road value so an unknown arm doesn't
/// distort the savings number in either direction.
pub const POWER_W_UNKNOWN: f32 = 15.0;

/// Look up the expected package power of one canonical arm. Pure
/// function — same name in, same number out, no I/O.
pub fn expected_power_w(arm: &str) -> f32 {
    match arm {
        "whisper" => POWER_W_WHISPER,
        "idle" => POWER_W_IDLE,
        "browse" => POWER_W_BROWSE,
        "call" => POWER_W_CALL,
        "code" => POWER_W_CODE,
        "npu-burst" => POWER_W_NPU_BURST,
        "build" => POWER_W_BUILD,
        "flat-out" => POWER_W_FLAT_OUT,
        _ => POWER_W_UNKNOWN,
    }
}

/// Replay every entry's `(shield_state, snapshot)` pair through the
/// rules baseline, and return the [`EnergyMetrics`] the rules-only
/// daemon would have produced. Each entry is treated as one 1 Hz
/// tick: a per-tick wattage × 1 s gets summed into `energy_kj_total`
/// (divided by 1 000 because the audit log records one entry per
/// second).
///
/// The `energy_saved_vs_baseline_kj` slot is intentionally left at
/// zero here — that slot is "savings of the bandit vs the baseline"
/// and only makes sense once the caller (Step 35 report driver)
/// pairs this output with the in-trace [`extract_energy_metrics`]
/// number. Returning a partial struct keeps both producers honest
/// about which fields they own.
///
/// `perf_per_watt_delta_pct` is likewise zero — perf-per-watt is a
/// joint property of two traces (the actual + the baseline), not of
/// the baseline alone.
///
/// [`extract_energy_metrics`]: super::metrics::extract_energy_metrics
pub fn compute_counterfactual_baseline(entries: &[AuditEntry]) -> EnergyMetrics {
    use crate::power::config::RulesBaselineConfig;
    let cfg = RulesBaselineConfig::default();
    let mut sum_w: f32 = 0.0;
    let mut samples: u32 = 0;
    for entry in entries {
        let state = entry
            .shield_state
            .as_deref()
            .and_then(ShieldState::parse)
            .unwrap_or(ShieldState::CoolAc);
        let arm = rules_baseline(state, &entry.snapshot, &cfg);
        sum_w += expected_power_w(arm);
        samples += 1;
    }
    let mean = if samples > 0 {
        sum_w / samples as f32
    } else {
        0.0
    };
    EnergyMetrics {
        mean_package_power_w: mean,
        energy_kj_total: sum_w / 1000.0,
        energy_saved_vs_baseline_kj: 0.0,
        perf_per_watt_delta_pct: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::activity::ActivityLabel;
    use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
    use chrono::{DateTime, TimeZone, Utc};

    fn pinned_ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
            .single()
            .expect("pinned UTC")
    }

    fn snap() -> Snapshot {
        Snapshot {
            schema: SCHEMA_ID,
            ts: pinned_ts(),
            features: [0.0; FEATURE_LEN],
            raw: SnapshotRaw {
                package_power_w: Some(8.0),
                activity_label: Some(ActivityLabel::Code),
                ..Default::default()
            },
            snapshot_hash: "0".repeat(64),
        }
    }

    fn entry(arm: &str, shield: &str) -> AuditEntry {
        AuditEntry::r3(
            snap(),
            arm.to_string(),
            shield.to_string(),
            vec![],
            vec![(arm.to_string(), 0.5)],
            0.05,
        )
    }

    /// Roadmap test: running the replay twice on the same input
    /// returns byte-identical EnergyMetrics — no clock reads, no RNG,
    /// no HashMap-iteration leak (the function returns a struct, not
    /// a map). Serialise both runs and compare the JSON bytes.
    #[test]
    fn counterfactual_replay_deterministic() {
        let entries = vec![
            entry("flat-out", "COOL_AC"),
            entry("flat-out", "WARM_AC"),
            entry("flat-out", "HOT"),
            entry("idle", "BATTERY_LOW"),
            entry("call", "MEETING"),
        ];
        let a = compute_counterfactual_baseline(&entries);
        let b = compute_counterfactual_baseline(&entries);
        let a_json = serde_json::to_string(&a).expect("serialize a");
        let b_json = serde_json::to_string(&b).expect("serialize b");
        assert_eq!(
            a_json, b_json,
            "back-to-back replay must produce byte-identical output",
        );
    }

    /// Roadmap test (note name deviation): the roadmap text reads
    /// "audit entries with bandit-chosen `flat-out` get replayed as
    /// `code` (the COOL_AC baseline arm)" — but the shipped
    /// `RulesBaselineConfig::default()` maps CoolAc → "browse", not
    /// "code" (Step 18 anchor). The test asserts the actual default,
    /// which is "browse"; the roadmap text is loose.
    #[test]
    fn baseline_uses_rules_table_not_bandit() {
        let entries = vec![
            entry("flat-out", "COOL_AC"),
            entry("flat-out", "COOL_AC"),
            entry("flat-out", "COOL_AC"),
        ];
        let m = compute_counterfactual_baseline(&entries);
        // Three ticks at COOL_AC ⇒ rules picks "browse" (12 W) each.
        // Bandit had picked "flat-out" (40 W); the baseline number
        // MUST be the browse wattage, not the flat-out wattage.
        let expected_mean = POWER_W_BROWSE;
        assert!(
            (m.mean_package_power_w - expected_mean).abs() < 1e-5,
            "baseline must replay CoolAc as 'browse' ({POWER_W_BROWSE} W), \
             not the bandit's 'flat-out' ({POWER_W_FLAT_OUT} W); got {} W",
            m.mean_package_power_w,
        );
        // 3 ticks × 12 W / 1000 = 0.036 kJ.
        let expected_kj = 3.0 * POWER_W_BROWSE / 1000.0;
        assert!(
            (m.energy_kj_total - expected_kj).abs() < 1e-5,
            "energy total must match the rules-baseline arm, got {} kJ \
             vs expected {} kJ",
            m.energy_kj_total,
            expected_kj,
        );
    }
}
