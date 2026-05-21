//! Bandit-arm primitives. Step 14 ships the schema only: the eight
//! pre-validated profiles from SPEC §4 "Bandit Arms" (`whisper, idle,
//! browse, call, code, build, npu-burst, flat-out`), each a tuple of
//! `(platform_profile, epp, igpu_mode, npu_pmode, cgroup_overrides)`.
//!
//! The actuator wiring (Steps 15–16), Conservative LinUCB bandit
//! (Steps 17–18), and audit-log integration (Step 23) consume this
//! same `Arm` struct so the arm name is the stable identifier across
//! the rollout.

use serde::{Deserialize, Serialize};

use super::sensors::igpu::IgpuProfileMode;
use super::sensors::platform::PlatformProfile;

pub mod arms;
pub mod clucb;
pub mod reward;

pub use arms::load_arms;
#[cfg(test)]
pub use clucb::FEATURE_LEN_WITH_ACTIVITY;
pub use clucb::{for_snapshot_features_with_activity, Clucb};
pub use reward::compute_reward;

/// One bandit arm = one fully-specified power profile. The five
/// dimensions match SPEC §4: an ACPI platform profile, an
/// `energy_performance_preference` value, an iGPU `pp_power_profile_mode`,
/// an `xrt-smi` NPU pmode, and a cgroup `cpu.uclamp.*` / `cpu.weight`
/// override block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Arm {
    /// Stable identifier — used by `sy power profile <name>` (Step 22)
    /// and the audit log. The eight names shipped in
    /// `configs/sy/power.toml` are `whisper, idle, browse, call, code,
    /// build, npu-burst, flat-out`.
    pub name: String,
    pub platform_profile: PlatformProfile,
    pub epp: Epp,
    pub igpu_mode: IgpuProfileMode,
    pub npu_pmode: NpuPmode,
    #[serde(default)]
    pub cgroup: CgroupOverrides,
}

/// `energy_performance_preference` value written under each
/// `policy*/energy_performance_preference` sysfs node. Five-step
/// enum per SPEC §2; serialises in snake_case so `power.toml` reads as
/// `epp = "balance_performance"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Epp {
    Performance,
    BalancePerformance,
    Default,
    BalancePower,
    Power,
}

/// `xrt-smi configure --pmode` value. Five rungs documented by AMD;
/// SPEC §4 leaves `default` distinct from `balanced` so the actuator
/// can request the vendor's own auto-mode explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NpuPmode {
    Default,
    Powersaver,
    Balanced,
    Performance,
    Turbo,
}

/// Optional cgroup overrides applied to the daemon's own systemd
/// `--user` slice. All three fields are optional — an absent value
/// means "leave the existing cgroup setting untouched". `cpu_uclamp_*`
/// is a percentage in `[0, 100]`; `cpu_weight` matches systemd's
/// `CPUWeight=` range (`[1, 10_000]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CgroupOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_uclamp_min: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_uclamp_max: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_weight: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SPEC §4 cgroup hints all sit in `[0, 100]`; the deserializer
    /// must accept the `cpu_uclamp_max = 40` form used in `power.toml`
    /// and round-trip back to a sparse JSON object.
    #[test]
    fn cgroup_overrides_round_trip_partial() {
        let toml_src = "cpu_uclamp_max = 40\n";
        let parsed: CgroupOverrides = toml::from_str(toml_src).expect("toml parse");
        assert_eq!(parsed.cpu_uclamp_max, Some(40));
        assert_eq!(parsed.cpu_uclamp_min, None);
        let json = serde_json::to_string(&parsed).expect("json serialize");
        // Skip-if-none keeps the JSON tight for `list-profiles --json`.
        assert_eq!(json, "{\"cpu_uclamp_max\":40}");
    }

    #[test]
    fn epp_serializes_snake_case() {
        let s = serde_json::to_string(&Epp::BalancePerformance).expect("ser");
        assert_eq!(s, "\"balance_performance\"");
        let v: Epp = serde_json::from_str("\"power\"").expect("de");
        assert_eq!(v, Epp::Power);
    }

    #[test]
    fn npu_pmode_serializes_snake_case() {
        let s = serde_json::to_string(&NpuPmode::Powersaver).expect("ser");
        assert_eq!(s, "\"powersaver\"");
        let v: NpuPmode = serde_json::from_str("\"turbo\"").expect("de");
        assert_eq!(v, NpuPmode::Turbo);
    }
}
