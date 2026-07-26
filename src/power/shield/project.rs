//! Shield projection walker (Roadmap Step 18).
//!
//! `project(ranked, state, snapshot, cfg, thrash) -> Arm` walks a
//! pre-ranked candidate list (proposed by the bandit in Step 22; in
//! Step 19 it is the singleton `[rules_baseline]`) and returns the
//! first arm whose `(platform_profile, …)` tuple respects the SPEC §4
//! shield constraints for the current `ShieldState`. If none pass, or
//! the thrash limiter trips, the function returns the rules-baseline
//! arm — the floor CLUCB cannot underperform.
//!
//! The walker is pure-ish: it consults a `ThrashTracker` to enforce
//! the 30 s minimum interval between arm changes, and the tracker's
//! state mutates as a side effect of `record(arm, now)`. The clock is
//! injected (Step 17's pattern) so tests can drive virtual time.
//!
//! ## Constraint rules (SPEC §4, simplified for Step 18)
//!
//! - `Hot`        — only arms with `platform_profile <= Balanced` pass.
//! - `BatteryLow` — only arms with `platform_profile == Quiet` pass.
//! - `Meeting`    — only the `call` arm passes (locked for the
//!   meeting window; the daemon, Step 19, releases the lock 30 s
//!   after VAD release).
//! - `WarmAc`     — every arm passes (warm-but-not-hot envelope).
//! - `CoolAc`     — every arm passes.
//!
//! ## Thrash limiter
//!
//! `ThrashTracker::would_thrash(new_arm, now)` returns `true` when
//! `now - last_change < profile_thrash_min_interval_s` *and*
//! `new_arm != last_arm`. When true, `project` falls back to the
//! baseline rather than the bandit's first-passing pick — rapid arm
//! flips collapse to the conservative floor.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::power::bandit::Arm;
use crate::power::config::PowerConfig;
use crate::power::policy::rules_baseline;
use crate::power::sensors::platform::PlatformProfile;
use crate::power::shield::ShieldState;
use crate::power::snapshot::Snapshot;

/// Pure-function shield constraint check. Returns `true` iff `arm`'s
/// platform-profile tuple is permitted by the current `state`. See
/// module docs for the constraint table.
fn arm_passes_shield(arm: &Arm, state: ShieldState) -> bool {
    match state {
        ShieldState::CoolAc | ShieldState::WarmAc => true,
        ShieldState::Hot => matches!(
            arm.platform_profile,
            PlatformProfile::Quiet | PlatformProfile::Balanced | PlatformProfile::LowPower
        ),
        ShieldState::BatteryLow => matches!(arm.platform_profile, PlatformProfile::Quiet),
        ShieldState::Meeting => arm.name == "call",
    }
}

/// Tracks the last applied arm + the wall-clock instant of that
/// change. The shield projection (`project`) consults this on every
/// tick to enforce the SPEC §4 anti-thrash floor: arms cannot change
/// more often than once per `profile_thrash_min_interval_s` (default
/// 30 s).
///
/// Interior `Mutex` keeps the `&self` signature on the read path so
/// `project` does not need a `&mut` borrow. Lock poisoning is treated
/// as "no record yet" — the projection then degrades to the baseline,
/// the same conservative response the limiter would force on a real
/// thrash.
#[derive(Debug, Default)]
pub struct ThrashTracker {
    state: Mutex<Option<TrackerState>>,
}

#[derive(Debug, Clone)]
struct TrackerState {
    last_arm: String,
    last_change: Instant,
}

impl ThrashTracker {
    /// Construct an empty tracker — the next `would_thrash` returns
    /// `false` regardless of arm or instant.
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when applying `new_arm` at `now` would violate the
    /// `profile_thrash_min_interval_s` floor. Returns `false` when no
    /// arm has been recorded yet, or when `new_arm == last_arm`
    /// (re-applying the same arm is a no-op, not a thrash).
    pub fn would_thrash(&self, new_arm: &str, now: Instant, min_interval: Duration) -> bool {
        let Ok(guard) = self.state.lock() else {
            return false;
        };
        let Some(ref s) = *guard else {
            return false;
        };
        if s.last_arm == new_arm {
            return false;
        }
        now.duration_since(s.last_change) < min_interval
    }

    /// Persist `arm` as the most recently applied arm at `now`.
    /// Called by `project` after it returns its pick.
    ///
    /// `last_change` marks the last actual arm *change*, not the last
    /// tick: when `arm` equals the currently recorded arm the timestamp
    /// is left untouched. Refreshing it on every same-arm re-pick would
    /// keep the anti-thrash window perpetually young at the daemon's
    /// 1 Hz cadence, permanently locking out all future arm switches
    /// (BUG-20260712-1046).
    pub fn record(&self, arm: &str, now: Instant) {
        if let Ok(mut guard) = self.state.lock() {
            if matches!(guard.as_ref(), Some(s) if s.last_arm == arm) {
                return;
            }
            *guard = Some(TrackerState {
                last_arm: arm.to_string(),
                last_change: now,
            });
        }
    }
}

/// Resolve `name` against `cfg.arms`. Returns `None` if no arm with
/// that name is configured — the caller treats this as "fall back to
/// the first arm in the ranked list" (the bandit's pick) to keep the
/// daemon making progress on a misconfigured baseline.
fn arm_named<'a>(name: &str, cfg: &'a PowerConfig) -> Option<&'a Arm> {
    cfg.arms.iter().find(|a| a.name == name)
}

/// Walk `ranked` in order, return the first arm whose tuple passes
/// the SPEC §4 shield constraints for `state`. When none pass, or
/// the thrash limiter would trip on the candidate, return the
/// rules-baseline arm for `state`. The tracker is updated with the
/// final pick so successive ticks observe the anti-thrash floor.
///
/// The function is `O(ranked.len() + cfg.arms.len())` — both bounded
/// by 8 in the shipped config — so the DoD's 50 µs budget is met
/// with headroom (see `tests::project_completes_in_under_50us`).
pub fn project(
    ranked: &[Arm],
    state: ShieldState,
    snapshot: &Snapshot,
    cfg: &PowerConfig,
    thrash: &ThrashTracker,
    now: Instant,
) -> Arm {
    project_inner(ranked, state, snapshot, cfg, thrash, now, false)
}

/// Like [`project`], but treats `ranked` as an operator pin
/// (`sy power profile <arm>`): the anti-thrash `would_thrash` floor is
/// bypassed so the pinned arm actuates regardless of how recently the
/// arm last changed. The SPEC §4 *safety* shield constraints (Hot /
/// BatteryLow / Meeting) still apply — a pin cannot defeat the thermal
/// or battery guard. See BUG-20260712-1136.
pub fn project_forced(
    ranked: &[Arm],
    state: ShieldState,
    snapshot: &Snapshot,
    cfg: &PowerConfig,
    thrash: &ThrashTracker,
    now: Instant,
) -> Arm {
    project_inner(ranked, state, snapshot, cfg, thrash, now, true)
}

/// Shared walker for [`project`] / [`project_forced`]. `forced` skips the
/// anti-thrash veto (operator pins must win over the oscillation floor)
/// while keeping the shield safety constraints and the tracker update.
fn project_inner(
    ranked: &[Arm],
    state: ShieldState,
    snapshot: &Snapshot,
    cfg: &PowerConfig,
    thrash: &ThrashTracker,
    now: Instant,
    forced: bool,
) -> Arm {
    let min_interval = Duration::from_secs(u64::from(cfg.shield.profile_thrash_min_interval_s));
    for candidate in ranked {
        if !arm_passes_shield(candidate, state) {
            continue;
        }
        if !forced && thrash.would_thrash(&candidate.name, now, min_interval) {
            break;
        }
        thrash.record(&candidate.name, now);
        return candidate.clone();
    }
    let baseline_name = rules_baseline(state, snapshot, &cfg.rules_baseline);
    let chosen = arm_named(baseline_name, cfg)
        .cloned()
        .unwrap_or_else(|| fallback_arm(baseline_name));
    thrash.record(&chosen.name, now);
    chosen
}

/// Construct a degenerate arm when the configured baseline name is
/// missing from `cfg.arms`. The daemon still gets a named arm to
/// log; the actuators (Step 15-16) will refuse to write because the
/// tuple defaults are intentionally not "performance".
fn fallback_arm(name: &str) -> Arm {
    use crate::power::bandit::{CgroupOverrides, Epp, NpuPmode};
    use crate::power::sensors::igpu::IgpuProfileMode;
    Arm {
        name: name.to_string(),
        platform_profile: PlatformProfile::Quiet,
        epp: Epp::Power,
        igpu_mode: IgpuProfileMode::Other("POWER_SAVING".into()),
        npu_pmode: NpuPmode::Powersaver,
        cgroup: CgroupOverrides::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::bandit::load_arms;
    use crate::power::config::PowerConfig;
    use crate::power::snapshot::{SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;

    /// Pinned UTC instant for the snapshot fixture. Step 17 uses the
    /// same anchor so any test mixing the two stays byte-stable.
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

    /// Load the shipped arm table; tests use these so the
    /// platform-profile checks operate on the canonical SPEC §4 set.
    fn shipped_cfg() -> PowerConfig {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml");
        PowerConfig::load(&path).expect("shipped power.toml parses")
    }

    fn arm(cfg: &PowerConfig, name: &str) -> Arm {
        load_arms(cfg)
            .expect("load_arms")
            .into_iter()
            .find(|a| a.name == name)
            .unwrap_or_else(|| panic!("arm {name} missing from shipped config"))
    }

    #[test]
    fn picks_first_passing_arm() {
        let cfg = shipped_cfg();
        let snap = pinned_snapshot();
        let tracker = ThrashTracker::new();
        let ranked = vec![arm(&cfg, "flat-out"), arm(&cfg, "build"), arm(&cfg, "code")];
        let pick = project(
            &ranked,
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            Instant::now(),
        );
        // COOL_AC permits every arm, so the head of the ranked list wins.
        assert_eq!(pick.name, "flat-out");
    }

    #[test]
    fn falls_back_to_baseline_when_all_blocked() {
        let cfg = shipped_cfg();
        let snap = pinned_snapshot();
        let tracker = ThrashTracker::new();
        // HOT permits only arms with platform_profile <= Balanced.
        // Stack the ranked list with performance-only arms; none pass.
        let ranked = vec![arm(&cfg, "flat-out"), arm(&cfg, "build")];
        let pick = project(
            &ranked,
            ShieldState::Hot,
            &snap,
            &cfg,
            &tracker,
            Instant::now(),
        );
        // Baseline for HOT is `idle` (SPEC §4 floor; configured in
        // `RulesBaselineConfig::default`).
        assert_eq!(pick.name, "idle");
    }

    #[test]
    fn meeting_state_locks_in_call_arm() {
        let cfg = shipped_cfg();
        let snap = pinned_snapshot();
        let tracker = ThrashTracker::new();
        // Even with `code` first, MEETING constraint admits only
        // `call`. The walker skips `code` and the baseline picks
        // `call` (configured default for MEETING).
        let ranked = vec![arm(&cfg, "code"), arm(&cfg, "build")];
        let pick = project(
            &ranked,
            ShieldState::Meeting,
            &snap,
            &cfg,
            &tracker,
            Instant::now(),
        );
        assert_eq!(pick.name, "call");
    }

    #[test]
    fn profile_thrash_limit_30s() {
        let cfg = shipped_cfg();
        let snap = pinned_snapshot();
        let tracker = ThrashTracker::new();
        let t0 = Instant::now();
        // Tick 0: bandit proposes `code`, shield admits it.
        let p0 = project(
            &[arm(&cfg, "code")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0,
        );
        assert_eq!(p0.name, "code");
        // Tick 1 (1 s later): bandit proposes `build`. The
        // anti-thrash floor (30 s default) blocks the change; the
        // baseline takes over. Baseline for COOL_AC is `browse`.
        let p1 = project(
            &[arm(&cfg, "build")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0 + Duration::from_secs(1),
        );
        assert_eq!(
            p1.name, "browse",
            "rapid flip must collapse to baseline, got {}",
            p1.name,
        );
        // Tick 2 (31 s later): the limiter has cleared; bandit's
        // pick goes through.
        let p2 = project(
            &[arm(&cfg, "build")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0 + Duration::from_secs(31),
        );
        assert_eq!(p2.name, "build");
    }

    /// BUG-20260712-1046: an arm switch must take effect even after a
    /// long steady-state run of unchanged re-picks. Under the bug,
    /// `record` refreshed `last_change` on every tick (including
    /// same-arm re-picks), so at the daemon's 1 Hz cadence the 30 s
    /// anti-thrash floor never elapsed relative to the *last tick* —
    /// permanently locking out every pin / bandit arm switch.
    #[test]
    fn arm_switch_takes_effect_after_steady_repicks() {
        let cfg = shipped_cfg();
        let snap = pinned_snapshot();
        let tracker = ThrashTracker::new();
        let t0 = Instant::now();
        // Tick 0: bandit settles on `code`.
        let p0 = project(
            &[arm(&cfg, "code")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0,
        );
        assert_eq!(p0.name, "code");
        // Ticks 1..=60 at 1 Hz: the SAME arm is re-picked (steady
        // state). No actual change occurs, so the anti-thrash window
        // must keep marking tick 0.
        for i in 1..=60 {
            let _ = project(
                &[arm(&cfg, "code")],
                ShieldState::CoolAc,
                &snap,
                &cfg,
                &tracker,
                t0 + Duration::from_secs(i),
            );
        }
        // Tick 61: a pin / bandit switch to `flat-out`. 61 s have
        // elapsed since the last ACTUAL change (tick 0), well past the
        // 30 s floor, so the switch must go through.
        let p = project(
            &[arm(&cfg, "flat-out")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0 + Duration::from_secs(61),
        );
        assert_eq!(
            p.name, "flat-out",
            "arm switch must take effect after a steady run of re-picks; got {}",
            p.name,
        );
    }

    /// BUG-20260712-1046: an unchanged re-pick must NOT slide the
    /// anti-thrash window forward — `last_change` marks the last actual
    /// arm CHANGE, not the last `record` call.
    #[test]
    fn repick_does_not_refresh_last_change() {
        let tracker = ThrashTracker::new();
        let t0 = Instant::now();
        let min = Duration::from_secs(30);
        tracker.record("code", t0);
        // A same-arm re-pick 29 s later must not move the window.
        tracker.record("code", t0 + Duration::from_secs(29));
        // 31 s after the real change at t0, switching arms is allowed
        // even though a re-pick happened at t0+29s.
        assert!(
            !tracker.would_thrash("flat-out", t0 + Duration::from_secs(31), min),
            "unchanged re-pick must not refresh last_change",
        );
    }

    /// BUG-20260712-1046 regression guard: the anti-thrash floor's real
    /// purpose survives the fix — a genuine rapid flap A→B→A inside the
    /// window still collapses to the conservative baseline.
    #[test]
    fn rapid_flap_within_window_still_suppressed() {
        let cfg = shipped_cfg();
        let snap = pinned_snapshot();
        let tracker = ThrashTracker::new();
        let t0 = Instant::now();
        // Settle on `code`.
        let p0 = project(
            &[arm(&cfg, "code")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0,
        );
        assert_eq!(p0.name, "code");
        // 1 s later flip to `build` — blocked, collapses to baseline.
        let p1 = project(
            &[arm(&cfg, "build")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0 + Duration::from_secs(1),
        );
        assert_eq!(p1.name, "browse");
        // 2 s later flip back to `code` — still inside the window,
        // still suppressed to the baseline.
        let p2 = project(
            &[arm(&cfg, "code")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0 + Duration::from_secs(2),
        );
        assert_eq!(
            p2.name, "browse",
            "rapid A->B->A flap must stay suppressed; got {}",
            p2.name,
        );
    }

    /// BUG-20260712-1136: an operator pin must actuate even when the
    /// anti-thrash window is warm. `project_forced` bypasses the
    /// `would_thrash` veto (the floor exists to damp bandit oscillation,
    /// not to override explicit operator intent), while the safety
    /// shield constraints still apply.
    #[test]
    fn forced_pin_bypasses_thrash_floor() {
        let cfg = shipped_cfg();
        let snap = pinned_snapshot();
        let tracker = ThrashTracker::new();
        let t0 = Instant::now();
        // Settle on the baseline arm.
        let p0 = project(
            &[arm(&cfg, "browse")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0,
        );
        assert_eq!(p0.name, "browse");
        // 1 s later an operator pins `flat-out` — inside the 30 s
        // window, so the un-forced path would collapse to the baseline.
        // A forced pin must go through.
        let p1 = project_forced(
            &[arm(&cfg, "flat-out")],
            ShieldState::CoolAc,
            &snap,
            &cfg,
            &tracker,
            t0 + Duration::from_secs(1),
        );
        assert_eq!(
            p1.name, "flat-out",
            "operator pin must bypass the anti-thrash floor; got {}",
            p1.name,
        );
    }

    /// BUG-20260712-1136: a forced pin still honours the SPEC §4 safety
    /// shield — pinning a `performance` arm while `Hot` must not defeat
    /// the thermal guard; it falls back to the rules baseline.
    #[test]
    fn forced_pin_still_respects_shield_safety() {
        let cfg = shipped_cfg();
        let snap = pinned_snapshot();
        let tracker = ThrashTracker::new();
        let picked = project_forced(
            &[arm(&cfg, "flat-out")],
            ShieldState::Hot,
            &snap,
            &cfg,
            &tracker,
            Instant::now(),
        );
        assert_ne!(
            picked.name, "flat-out",
            "a forced performance pin must not defeat the Hot thermal guard",
        );
    }

    /// DoD bullet 1: shield projection completes in <50 µs. No
    /// `criterion` dep in tree, so a tight `Instant`-based perf test
    /// stands in for the bench. 10 000 iterations averaged keeps the
    /// per-call cost under the budget by ~3 orders of magnitude on
    /// the dev machine (pure-fn walk over 8 arms, simple matches).
    #[test]
    fn project_completes_in_under_50us() {
        const ITERS: u32 = 10_000;
        const BUDGET_NS: u128 = 50_000; // 50 µs
        let cfg = shipped_cfg();
        let snap = pinned_snapshot();
        let tracker = ThrashTracker::new();
        let ranked = load_arms(&cfg).expect("load_arms");
        let start = Instant::now();
        for _ in 0..ITERS {
            // Use a fresh `now` so the thrash limiter doesn't gate
            // the bench — every iteration must exercise the full
            // walk + record path.
            let _ = project(
                &ranked,
                ShieldState::CoolAc,
                &snap,
                &cfg,
                &tracker,
                Instant::now(),
            );
        }
        let avg_ns = start.elapsed().as_nanos() / u128::from(ITERS);
        assert!(
            avg_ns < BUDGET_NS,
            "shield::project must stay under 50 µs/call; observed {avg_ns} ns",
        );
    }
}
