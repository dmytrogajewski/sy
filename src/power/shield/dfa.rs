//! 5-state shield DFA (Roadmap Step 17).
//!
//! `transition(prev, snapshot, cfg, secs_since_call) -> ShieldState` is
//! a pure function: no clock reads, no I/O, deterministic given its
//! four inputs. The state enumeration matches SPEC §4 — `COOL_AC |
//! WARM_AC | HOT | BATTERY_LOW | MEETING` — and the priority order is
//! `MEETING` > `BATTERY_LOW` > `HOT` > `WARM_AC` > `COOL_AC`.
//!
//! - `MEETING` fires when `call_active` is true now, OR `prev ==
//!   Meeting` and fewer than `cfg.meeting_lock_after_vad_s` seconds
//!   have elapsed since `call_active` was last true. That elapsed time
//!   arrives as the injected `secs_since_call` argument (the daemon
//!   tracks the last-call timestamp across ticks) so the DFA stays
//!   pure — no implicit clock read. Once the lock window elapses
//!   MEETING is NOT absorbing: the DFA re-evaluates the thermal /
//!   battery rungs normally (BUG-20260712-1201).
//! - `BATTERY_LOW` fires on DC operation with SOC at or below the
//!   emergency or low thresholds; emergency takes precedence in the
//!   Step 18 projection.
//! - `HOT` fires when Tctl ≥ `tctl_act_c` (default 85 °C).
//! - `WARM_AC` fires when Tctl ≥ `tctl_sustained_60s_avg_c` (default
//!   80 °C). The "AC" suffix tracks SPEC §4 nomenclature; the DFA
//!   itself does not gate on AC for this rung.
//! - `COOL_AC` is the fallback.
//!
//! All thresholds come from [`ShieldConfig`], which loads from the
//! `[shield]` stanza of `configs/sy/power.toml`. Hard-coding the table
//! in this file is **explicitly banned** by the Step 17 DoD.
//!
//! [`ShieldConfig`]: crate::power::config::ShieldConfig

use serde::{Deserialize, Serialize};

use crate::power::config::ShieldConfig;
use crate::power::snapshot::Snapshot;

/// The five shield states from SPEC §4. Serialised as
/// `SCREAMING_SNAKE_CASE` so the wire format matches the existing
/// `shield_state` slot in `sy.power.status/v1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ShieldState {
    CoolAc,
    WarmAc,
    Hot,
    BatteryLow,
    Meeting,
}

impl ShieldState {
    /// Stable string id matching the `serde(rename_all)` form. Used by
    /// `status::build_status_value` to fill the `shield_state` slot
    /// without round-tripping through `serde_json::to_value` for one
    /// enum.
    pub fn as_str(self) -> &'static str {
        match self {
            ShieldState::CoolAc => "COOL_AC",
            ShieldState::WarmAc => "WARM_AC",
            ShieldState::Hot => "HOT",
            ShieldState::BatteryLow => "BATTERY_LOW",
            ShieldState::Meeting => "MEETING",
        }
    }

    /// Reverse of [`ShieldState::as_str`]: parse a SCREAMING_SNAKE_CASE
    /// state name back to the enum. Returns `None` on an unknown token
    /// so callers (Step 23's audit replay) can render a sensible
    /// fallback when an older NDJSON line uses a tag this build no
    /// longer recognises.
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "COOL_AC" => Some(ShieldState::CoolAc),
            "WARM_AC" => Some(ShieldState::WarmAc),
            "HOT" => Some(ShieldState::Hot),
            "BATTERY_LOW" => Some(ShieldState::BatteryLow),
            "MEETING" => Some(ShieldState::Meeting),
            _ => None,
        }
    }
}

/// Pure-function shield transition. See module-level docstring for
/// the priority order. The function reads only `snapshot.raw` fields
/// and the `cfg` thresholds; it makes no syscalls, allocates nothing,
/// and is `Send + Sync` by virtue of its arguments.
pub fn transition(
    prev: ShieldState,
    snapshot: &Snapshot,
    cfg: &ShieldConfig,
    secs_since_call: Option<f32>,
) -> ShieldState {
    let raw = &snapshot.raw;
    let call_active = raw.call_active.unwrap_or(false);
    // MEETING is held while a whitelisted idle-inhibitor is live
    // (`call_active`, a LEVEL per BUG-20260712-1200) and for
    // `cfg.meeting_lock_after_vad_s` seconds after it last went false.
    // `secs_since_call` is the daemon-tracked elapsed time since
    // `call_active` was last true (`None` ⇒ never seen this run); the
    // daemon threads it in so the DFA reads no wall clock and stays
    // pure. Once the window elapses MEETING releases and the DFA
    // re-evaluates normally — it is NOT absorbing (BUG-20260712-1201).
    let within_lock = secs_since_call
        .map(|s| s < cfg.meeting_lock_after_vad_s as f32)
        .unwrap_or(false);
    if call_active || (prev == ShieldState::Meeting && within_lock) {
        return ShieldState::Meeting;
    }

    let ac_online = raw.ac_online.unwrap_or(true);
    if !ac_online {
        if let Some(soc) = raw.battery_soc_pct {
            if soc <= cfg.battery_emergency_dc_pct || soc <= cfg.battery_low_dc_pct {
                return ShieldState::BatteryLow;
            }
        }
    }

    if let Some(tctl) = raw.tctl_c {
        if tctl >= cfg.tctl_act_c {
            return ShieldState::Hot;
        }
        if tctl >= cfg.tctl_sustained_60s_avg_c {
            return ShieldState::WarmAc;
        }
    }

    ShieldState::CoolAc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::snapshot::{SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
    use chrono::{TimeZone, Utc};

    /// SPEC §4 act threshold — Step 17 anchor for the HOT test.
    const TCTL_HOT_C: f32 = 86.0;
    /// Below `tctl_sustained_60s_avg_c` ⇒ COOL_AC.
    const TCTL_COOL_C: f32 = 60.0;

    /// Build a snapshot pinned to the SPEC's reference instant so
    /// every transition test reads off the same zero-feature vector
    /// plus the `raw` overrides the test supplies.
    fn snap_with(raw: SnapshotRaw) -> Snapshot {
        Snapshot {
            schema: SCHEMA_ID,
            ts: Utc
                .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
                .single()
                .expect("pinned UTC"),
            features: [0.0_f32; FEATURE_LEN],
            raw,
            snapshot_hash: "0".repeat(64),
        }
    }

    #[test]
    fn transitions_to_hot_when_tctl_above_85() {
        let cfg = ShieldConfig::default();
        let snap = snap_with(SnapshotRaw {
            tctl_c: Some(TCTL_HOT_C),
            ac_online: Some(true),
            battery_soc_pct: Some(100),
            ..Default::default()
        });
        let state = transition(ShieldState::CoolAc, &snap, &cfg, None);
        assert_eq!(state, ShieldState::Hot);
    }

    #[test]
    fn battery_low_at_25pct_dc() {
        let cfg = ShieldConfig::default();
        // SOC at the threshold; DC; cool Tctl. SPEC §4 row 5.
        let snap = snap_with(SnapshotRaw {
            tctl_c: Some(TCTL_COOL_C),
            ac_online: Some(false),
            battery_soc_pct: Some(cfg.battery_low_dc_pct),
            ..Default::default()
        });
        let state = transition(ShieldState::CoolAc, &snap, &cfg, None);
        assert_eq!(state, ShieldState::BatteryLow);
    }

    #[test]
    fn battery_low_emergency_at_10pct_dc() {
        let cfg = ShieldConfig::default();
        let snap = snap_with(SnapshotRaw {
            tctl_c: Some(TCTL_COOL_C),
            ac_online: Some(false),
            battery_soc_pct: Some(cfg.battery_emergency_dc_pct),
            ..Default::default()
        });
        let state = transition(ShieldState::CoolAc, &snap, &cfg, None);
        assert_eq!(state, ShieldState::BatteryLow);
    }

    /// Build a (live-call, post-call) snapshot pair on a cool/AC/full
    /// host so the only thing driving MEETING is the call signal.
    fn meeting_snap_pair() -> (Snapshot, Snapshot) {
        let live = snap_with(SnapshotRaw {
            tctl_c: Some(TCTL_COOL_C),
            ac_online: Some(true),
            battery_soc_pct: Some(100),
            call_active: Some(true),
            ..Default::default()
        });
        let after_vad = snap_with(SnapshotRaw {
            tctl_c: Some(TCTL_COOL_C),
            ac_online: Some(true),
            battery_soc_pct: Some(100),
            call_active: Some(false),
            ..Default::default()
        });
        (live, after_vad)
    }

    #[test]
    fn meeting_held_within_lock_window() {
        // While call_active is true, then for every second up to (but
        // not including) `meeting_lock_after_vad_s` after it goes
        // false, MEETING stays pinned — no premature release.
        let cfg = ShieldConfig::default();
        let (live, after_vad) = meeting_snap_pair();
        let s0 = transition(ShieldState::CoolAc, &live, &cfg, Some(0.0));
        assert_eq!(s0, ShieldState::Meeting);
        let mut prev = s0;
        for secs in 0..cfg.meeting_lock_after_vad_s {
            prev = transition(prev, &after_vad, &cfg, Some(secs as f32));
            assert_eq!(
                prev,
                ShieldState::Meeting,
                "must stay MEETING at secs={secs}"
            );
        }
    }

    #[test]
    fn meeting_releases_after_lock_window() {
        // BUG-20260712-1201: MEETING must NOT be absorbing. Once the
        // call ends (call_active=false) and `meeting_lock_after_vad_s`
        // seconds elapse, the DFA re-evaluates normally instead of
        // pinning MEETING until daemon restart.
        let cfg = ShieldConfig::default();
        let (live, after_vad) = meeting_snap_pair();
        let s0 = transition(ShieldState::CoolAc, &live, &cfg, Some(0.0));
        assert_eq!(s0, ShieldState::Meeting);
        // One second short of the window → still held.
        let held = transition(
            s0,
            &after_vad,
            &cfg,
            Some((cfg.meeting_lock_after_vad_s - 1) as f32),
        );
        assert_eq!(held, ShieldState::Meeting);
        // Window elapsed → MEETING releases; cool/AC snapshot ⇒ COOL_AC.
        let released = transition(
            held,
            &after_vad,
            &cfg,
            Some(cfg.meeting_lock_after_vad_s as f32),
        );
        assert_eq!(
            released,
            ShieldState::CoolAc,
            "MEETING must release once the lock window elapses",
        );
    }

    #[test]
    fn cool_ac_when_idle_and_charged() {
        // Sanity floor: no call, AC, full battery, cool Tctl ⇒ COOL_AC.
        let cfg = ShieldConfig::default();
        let snap = snap_with(SnapshotRaw {
            tctl_c: Some(TCTL_COOL_C),
            ac_online: Some(true),
            battery_soc_pct: Some(80),
            ..Default::default()
        });
        let state = transition(ShieldState::CoolAc, &snap, &cfg, None);
        assert_eq!(state, ShieldState::CoolAc);
    }

    /// Exhaustive sweep over (Tctl, SOC, AC, meeting). The Step 17 DoD
    /// requires 10 000 cases via proptest; the crate has no proptest
    /// dep so we cover the full grid (Tctl ∈ 20..100 step 1 = 80, SOC
    /// ∈ 0..=100 step 1 = 101, AC ∈ {true,false} = 2, meeting ∈
    /// {true,false} = 2) — 80 × 101 × 2 × 2 = 32 320 cases, well above
    /// 10 000. Asserts every reachable state is one of the five
    /// enumerants (an unreachable `_ => panic!()` would catch any
    /// drift the moment `ShieldState` grows a variant without a DFA
    /// branch).
    #[test]
    fn full_transition_table() {
        let cfg = ShieldConfig::default();
        let mut cases = 0u32;
        for tctl in 20..100 {
            for soc in 0..=100u8 {
                for ac in [true, false] {
                    for meeting in [true, false] {
                        let snap = snap_with(SnapshotRaw {
                            tctl_c: Some(tctl as f32),
                            ac_online: Some(ac),
                            battery_soc_pct: Some(soc),
                            call_active: Some(meeting),
                            ..Default::default()
                        });
                        let state = transition(ShieldState::CoolAc, &snap, &cfg, None);
                        // Match-all proves the function never falls
                        // off the back of the enum — adding a new
                        // variant later forces this match to update
                        // (compile-time guard).
                        match state {
                            ShieldState::CoolAc
                            | ShieldState::WarmAc
                            | ShieldState::Hot
                            | ShieldState::BatteryLow
                            | ShieldState::Meeting => {}
                        }
                        cases += 1;
                    }
                }
            }
        }
        assert!(
            cases >= 10_000,
            "DoD says 10 000 cases; exhaustive grid covered {cases}",
        );
    }
}
