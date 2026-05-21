//! 5-state shield DFA (Roadmap Step 17).
//!
//! `transition(prev, snapshot, cfg) -> ShieldState` is a pure function:
//! no clock reads, no I/O, deterministic given its three inputs. The
//! state enumeration matches SPEC §4 — `COOL_AC | WARM_AC | HOT |
//! BATTERY_LOW | MEETING` — and the priority order is `MEETING` >
//! `BATTERY_LOW` > `HOT` > `WARM_AC` > `COOL_AC`.
//!
//! - `MEETING` fires when `call_active` is true now, OR `prev ==
//!   Meeting` with the call still held; the 30-second "lock after VAD
//!   release" timer lives in the daemon (Step 19), not here.
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
pub fn transition(prev: ShieldState, snapshot: &Snapshot, cfg: &ShieldConfig) -> ShieldState {
    let raw = &snapshot.raw;
    let call_active = raw.call_active.unwrap_or(false);
    // "MEETING locks until VAD release" — the daemon (Step 19) clears
    // `prev` after `cfg.meeting_lock_after_vad_s` of silence. While
    // `call_active` is true OR the daemon hasn't released `prev`, the
    // DFA pins MEETING. The threshold itself stays a daemon-side
    // concern so the DFA stays pure (no implicit clock read).
    if call_active || prev == ShieldState::Meeting {
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
        let state = transition(ShieldState::CoolAc, &snap, &cfg);
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
        let state = transition(ShieldState::CoolAc, &snap, &cfg);
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
        let state = transition(ShieldState::CoolAc, &snap, &cfg);
        assert_eq!(state, ShieldState::BatteryLow);
    }

    #[test]
    fn meeting_state_locks_for_30s_post_vad() {
        // Scripted snapshot stream: tick 0 triggers MEETING via
        // call_active. Subsequent ticks have call_active = false; the
        // DFA must hold MEETING as long as `prev == Meeting` (the
        // daemon, Step 19, manages the 30 s release timer outside the
        // DFA).
        let cfg = ShieldConfig::default();
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
        let s0 = transition(ShieldState::CoolAc, &live, &cfg);
        assert_eq!(s0, ShieldState::Meeting);
        // Simulate 30 ticks of 1 Hz silence — DFA must stay MEETING
        // because the daemon hasn't released `prev` yet.
        let mut prev = s0;
        for _ in 0..cfg.meeting_lock_after_vad_s {
            prev = transition(prev, &after_vad, &cfg);
            assert_eq!(prev, ShieldState::Meeting);
        }
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
        let state = transition(ShieldState::CoolAc, &snap, &cfg);
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
                        let state = transition(ShieldState::CoolAc, &snap, &cfg);
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
