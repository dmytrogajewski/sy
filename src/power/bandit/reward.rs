//! Bandit reward function (Roadmap Step 21, SPEC §6 Open Question 5).
//!
//! `compute_reward(before, after, applied_arm, prev_arm, cfg) -> f32`
//! is pure: no I/O, no clock reads, no global state. The daemon's
//! Step 22 hot loop feeds the result into `Clucb::update` so the
//! conservative bandit's posterior reflects the actually observed
//! (work, power, thermal, thrash) outcome of the previous tick's
//! arm choice.
//!
//! Canonical form (SPEC §6 Q5):
//! ```text
//! work_proxy(s)   = s.igpu_busy_pct + s.npu_workloads * 10
//! perf_per_watt   = (work_proxy(after) - work_proxy(before))
//!                   / (after.package_power_w + EPS)
//! thermal_penalty = max(0, after.tctl_c - 80) / 10
//! thrash_penalty  = 1 if applied_arm != prev_arm else 0
//!
//! reward = w_perf * perf_per_watt
//!        - w_thermal * thermal_penalty
//!        - w_thrash  * thrash_penalty
//! ```
//!
//! The result is finally clamped to `[REWARD_FLOOR, REWARD_CEIL]`
//! (`±10`) so a degenerate snapshot (e.g. zero package power with a
//! large work-proxy delta) cannot pump arbitrary mass into one arm's
//! posterior. SPEC §6 Q5 calls out the clamp explicitly: "the reward
//! must be bounded so the Cholesky update stays well-conditioned."

use crate::power::config::RewardConfig;
use crate::power::snapshot::Snapshot;

/// Clamp bounds for the final scalar reward. Symmetric and small
/// enough that even after the conservative wrapper's `α` margin the
/// Sherman-Morrison update on `A_a` stays well-conditioned (12×12
/// Gram matrix, `λ = 1.0` prior — see `bandit::clucb`).
pub const REWARD_FLOOR: f32 = -10.0;
pub const REWARD_CEIL: f32 = 10.0;

/// Tctl knee above which the thermal penalty starts pushing back.
/// Matches the SPEC §4 "WARM_AC" shield threshold so the reward
/// signal and the shield DFA agree on what "warm" means.
pub const THERMAL_KNEE_C: f32 = 80.0;

/// Divisor on the thermal excess (°C above the knee). Scaling by 10
/// means a Tjmax-adjacent 90°C reading contributes a penalty of 1.0
/// before the `thermal_weight` multiplier — a "big but not infinite"
/// nudge.
pub const THERMAL_SCALE_C: f32 = 10.0;

/// Floor on the package-power divisor so a zero-power reading cannot
/// blow `perf_per_watt` up to `f32::INFINITY`. Mirrors the dose used
/// by `Snapshot::package_power_w_5tap` to mask sensor jitter.
pub const POWER_EPSILON_W: f32 = 0.01;

/// NPU work weight inside the "useful work happened" proxy. Each
/// in-flight aiplane workload counts for `10` units of busy-pct, so
/// one queued embedder is roughly equivalent to a 10 % iGPU load —
/// the heuristic the Step 21 roadmap text pins.
pub const NPU_WORK_WEIGHT: f32 = 10.0;

/// Pure scalar reward used by Step 22's daemon update path.
///
/// Inputs are immutable references; the function performs no I/O.
/// `prev_arm = None` is treated as "no thrash" (first-ever decision).
pub fn compute_reward(
    before: &Snapshot,
    after: &Snapshot,
    applied_arm: &str,
    prev_arm: Option<&str>,
    cfg: &RewardConfig,
) -> f32 {
    let perf_per_watt = perf_per_watt_term(before, after);
    let thermal = thermal_penalty(after);
    let thrash = thrash_penalty(applied_arm, prev_arm);
    let raw = cfg.perf_per_watt_weight * perf_per_watt
        - cfg.thermal_weight * thermal
        - cfg.thrash_weight * thrash;
    // Bounded reward — see REWARD_FLOOR/REWARD_CEIL comment above.
    // NaN guard: a snapshot with NaN fields would otherwise poison
    // the bandit's Gram matrix; clamp() short-circuits NaN to NaN, so
    // we route through a manual finite-check first.
    if !raw.is_finite() {
        return 0.0;
    }
    raw.clamp(REWARD_FLOOR, REWARD_CEIL)
}

/// `(work_proxy_after − work_proxy_before) / (power_after + ε)`.
/// Returns 0.0 when either side has no usable readings — the caller
/// then sees a zero contribution from this term, which is the
/// documented "we observed nothing" semantic.
fn perf_per_watt_term(before: &Snapshot, after: &Snapshot) -> f32 {
    let work_before = work_proxy(before);
    let work_after = work_proxy(after);
    let delta = work_after - work_before;
    let power = after
        .raw
        .package_power_w
        .filter(|w| w.is_finite())
        .unwrap_or(0.0);
    delta / (power + POWER_EPSILON_W)
}

/// Heuristic "useful work happened" proxy from the Step 21 roadmap:
/// `igpu_busy_pct + npu_workloads * NPU_WORK_WEIGHT`. Missing readings
/// degrade to zero rather than NaN so the subtraction stays finite.
fn work_proxy(snap: &Snapshot) -> f32 {
    let igpu = snap.raw.igpu_busy_pct.unwrap_or(0) as f32;
    let npu = snap.raw.npu_workloads.unwrap_or(0) as f32;
    igpu + npu * NPU_WORK_WEIGHT
}

/// `max(0, tctl − THERMAL_KNEE_C) / THERMAL_SCALE_C`. Zero below the
/// knee, scales linearly above it.
fn thermal_penalty(after: &Snapshot) -> f32 {
    let tctl = after.raw.tctl_c.filter(|t| t.is_finite()).unwrap_or(0.0);
    ((tctl - THERMAL_KNEE_C).max(0.0)) / THERMAL_SCALE_C
}

/// `1.0` when the daemon switched arms vs the previous tick, else
/// `0.0`. `prev_arm = None` (first decision) counts as no thrash.
fn thrash_penalty(applied_arm: &str, prev_arm: Option<&str>) -> f32 {
    match prev_arm {
        Some(p) if p != applied_arm => 1.0,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
    use chrono::{TimeZone, Utc};

    /// Pinned timestamp for every test snapshot — the reward fn is
    /// `ts`-independent today, but pinning here keeps future debug
    /// `eprintln!`s reproducible across runs.
    fn pinned_ts() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
            .single()
            .expect("pinned UTC instant")
    }

    /// Build a minimal `Snapshot` populated with the four `raw` fields
    /// the reward fn reads (`tctl_c`, `package_power_w`,
    /// `igpu_busy_pct`, `npu_workloads`). Everything else stays at
    /// `Default::default()` — the fn never touches it.
    fn snap(tctl: f32, power: f32, igpu: u8, npu: u32) -> Snapshot {
        Snapshot {
            schema: SCHEMA_ID,
            ts: pinned_ts(),
            features: [0.0; FEATURE_LEN],
            raw: SnapshotRaw {
                tctl_c: Some(tctl),
                package_power_w: Some(power),
                igpu_busy_pct: Some(igpu),
                npu_workloads: Some(npu),
                ..Default::default()
            },
            snapshot_hash: "0".repeat(64),
        }
    }

    /// SPEC §6 Q5 thrash term: keeping the same arm tick-over-tick is
    /// rewarded over switching. We freeze the (before, after) pair so
    /// the `perf_per_watt` + `thermal` terms cancel out — only the
    /// thrash penalty moves.
    #[test]
    fn thrash_penalty_increases_with_recent_changes() {
        let before = snap(60.0, 8.0, 30, 0);
        let after = snap(60.0, 8.0, 30, 0);
        let cfg = RewardConfig::default();
        let same_arm = compute_reward(&before, &after, "build", Some("build"), &cfg);
        let switched = compute_reward(&before, &after, "idle", Some("build"), &cfg);
        assert!(
            switched < same_arm,
            "switching arms must be penalised vs. keeping the same arm \
             (same={same_arm}, switched={switched})",
        );
        // And the gap should be exactly `thrash_weight` since the
        // other terms cancel.
        let gap = same_arm - switched;
        assert!(
            (gap - cfg.thrash_weight).abs() < 1e-5,
            "thrash gap {gap} must equal cfg.thrash_weight {}",
            cfg.thrash_weight
        );
    }

    /// SPEC §6 Q5 thermal term: below `THERMAL_KNEE_C` (80°C) the
    /// thermal penalty is zero; above it the reward drops. Same
    /// applied/prev arm so the thrash term cancels; same work proxy
    /// so the perf/W term cancels.
    #[test]
    fn thermal_penalty_kicks_in_above_80c() {
        let cfg = RewardConfig::default();
        let before = snap(60.0, 8.0, 30, 0);
        let after_cool = snap(75.0, 8.0, 30, 0);
        let after_hot = snap(85.0, 8.0, 30, 0);
        let cool = compute_reward(&before, &after_cool, "code", Some("code"), &cfg);
        let hot = compute_reward(&before, &after_hot, "code", Some("code"), &cfg);
        assert!(
            hot < cool,
            "above 80°C the reward must drop (cool@75°C={cool}, hot@85°C={hot})",
        );
        // Below the knee, the thermal penalty must contribute zero;
        // double-check by comparing against a 60°C reading (same as
        // `before`'s tctl, so trivially below the knee).
        let after_chilly = snap(60.0, 8.0, 30, 0);
        let chilly = compute_reward(&before, &after_chilly, "code", Some("code"), &cfg);
        assert!(
            (chilly - cool).abs() < 1e-5,
            "below 80°C the thermal term must be zero \
             (chilly@60°C={chilly}, cool@75°C={cool})",
        );
    }

    /// SPEC §6 Q5 bounded-reward invariant. Sweep a Cartesian product
    /// of (tctl, power, igpu busy, npu workloads, arm-changes) over
    /// the documented operating range. Every result must land inside
    /// `[REWARD_FLOOR, REWARD_CEIL]` — Step 21 Hard-blocker note
    /// expects the explicit `clamp()` to enforce this even when the
    /// raw formula overshoots (e.g. zero package power with a large
    /// work-proxy delta).
    #[test]
    fn reward_is_bounded() {
        let cfg = RewardConfig::default();
        let powers = [0.1_f32, 1.0, 5.0, 20.0, 50.0];
        let tctls = [20.0_f32, 40.0, 60.0, 80.0, 90.0, 100.0];
        let busies = [0_u8, 25, 50, 75, 100];
        let npus = [0_u32, 1, 3, 5];
        for &p_after in &powers {
            for &t_after in &tctls {
                for &b_before in &busies {
                    for &b_after in &busies {
                        for &n_before in &npus {
                            for &n_after in &npus {
                                let before = snap(60.0, 8.0, b_before, n_before);
                                let after = snap(t_after, p_after, b_after, n_after);
                                for (applied, prev) in [
                                    ("build", Some("build")),
                                    ("build", Some("idle")),
                                    ("idle", None),
                                ] {
                                    let r = compute_reward(&before, &after, applied, prev, &cfg);
                                    assert!(
                                        r.is_finite(),
                                        "reward must be finite (before=({b_before},{n_before}) \
                                         after=({t_after}°C,{p_after}W,{b_after},{n_after}) \
                                         applied={applied} prev={prev:?}): got {r}",
                                    );
                                    assert!(
                                        (REWARD_FLOOR..=REWARD_CEIL).contains(&r),
                                        "reward {r} out of [{REWARD_FLOOR}, {REWARD_CEIL}] \
                                         for (before=({b_before},{n_before}) \
                                         after=({t_after}°C,{p_after}W,{b_after},{n_after}) \
                                         applied={applied} prev={prev:?})",
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Defensive: a snapshot with NaN tctl / power must not poison the
    /// bandit. The fn returns zero (neutral signal) so the bandit's
    /// posterior stays untouched on the bad tick.
    #[test]
    fn nan_inputs_degrade_to_zero() {
        let cfg = RewardConfig::default();
        let before = snap(60.0, 8.0, 30, 0);
        let mut after = snap(60.0, 8.0, 30, 0);
        after.raw.package_power_w = Some(f32::NAN);
        let r = compute_reward(&before, &after, "code", Some("code"), &cfg);
        assert!(r.is_finite(), "NaN power must not produce NaN reward");
        assert!(
            (r - 0.0).abs() < 1e-5,
            "NaN inputs must degrade to 0.0, got {r}"
        );
    }
}
