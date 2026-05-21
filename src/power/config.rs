//! `power.toml` schema + loader.
//!
//! Default values track SPEC §6 Open Questions (`bandit.alpha = 0.05`,
//! `onboarding.days = 14`). `SY_POWER_ONBOARDING_DAYS` overrides the
//! TOML default so the operator can shorten the rules-only window
//! during dev / bench without rewriting the config.
//!
//! Step 1 only needs the schema to deserialize without errors when
//! every stanza is empty — later steps (R2 rules baseline, R3 bandit,
//! R6 drift) fill the cells.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::bandit::Arm;

/// Env override for `[onboarding] days`. Documented in SPEC §6
/// "Open Questions: Onboarding window length".
const ENV_ONBOARDING_DAYS: &str = "SY_POWER_ONBOARDING_DAYS";

/// SPEC §6 Open Question default — Apple Optimized Battery Charging
/// mirrors a 14-day rehearsal before any ML decision lands.
pub const DEFAULT_ONBOARDING_DAYS: u32 = 14;

/// SPEC §6 Open Question default — Conservative LinUCB α. Lower α =
/// tighter regret bound vs the baseline, slower convergence.
pub const DEFAULT_BANDIT_ALPHA: f64 = 0.05;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PowerConfig {
    /// Bandit arm table. Eight pre-validated profiles per SPEC §4
    /// "Bandit Arms"; the names are stable identifiers used by
    /// `sy power profile <name>` and the audit log.
    #[serde(rename = "arms", default)]
    pub arms: Vec<Arm>,
    pub shield: ShieldConfig,
    pub bandit: BanditConfig,
    pub reward: RewardConfig,
    pub onboarding: OnboardingConfig,
    pub rules_baseline: RulesBaselineConfig,
}

/// SPEC §4 "Concrete Shield Constraint Set (HX 370)" defaults. Each
/// field tracks one row of the constraint table; the `[shield]` stanza
/// of `configs/sy/power.toml` ships overrides on a HX 370 dev machine.
/// Step 17's DFA (`src/power/shield/dfa.rs`) reads these — they are
/// **not** hard-coded inside the DFA.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ShieldConfig {
    /// Tctl absolute ceiling (°C). AMD Tjmax ≈ 95.
    pub tctl_peak_c: f32,
    /// Tctl "act now" threshold (°C). Above this ⇒ shield state HOT.
    pub tctl_act_c: f32,
    /// Tctl sustained 60 s average (°C). Above this ⇒ WARM_AC.
    pub tctl_sustained_60s_avg_c: f32,
    /// Package-power excursion ceiling (W per 2 s window). VRM spike
    /// guard; consumed by Step 18's projection, recorded here so the
    /// constraint set lives in one struct.
    pub package_power_excursion_w_per_2s: f32,
    /// Fan-RPM ceiling. Framework HX 370 reference max.
    pub fan_rpm_max: u32,
    /// Battery SOC (%) below which DC operation forces ≤ `balanced`.
    pub battery_low_dc_pct: u8,
    /// Battery SOC (%) below which DC operation forces `quiet`.
    pub battery_emergency_dc_pct: u8,
    /// Minimum profile-change interval (s). Anti-thrash.
    pub profile_thrash_min_interval_s: u32,
    /// MEETING lock duration after VAD release (s).
    pub meeting_lock_after_vad_s: u32,
    /// NPU `xrt-smi --pmode` minimum interval (s).
    pub npu_pmode_min_interval_s: u32,
    /// EPP delta cap per 1 Hz tick.
    pub epp_delta_max_per_tick: u32,
}

impl Default for ShieldConfig {
    fn default() -> Self {
        // SPEC §4 "Concrete Shield Constraint Set (HX 370)" canonical
        // values. Bake them in so tests / start-up before `power.toml`
        // exists still observe the documented constraint table.
        Self {
            tctl_peak_c: 90.0,
            tctl_act_c: 85.0,
            tctl_sustained_60s_avg_c: 80.0,
            package_power_excursion_w_per_2s: 15.0,
            fan_rpm_max: 5500,
            battery_low_dc_pct: 25,
            battery_emergency_dc_pct: 10,
            profile_thrash_min_interval_s: 30,
            meeting_lock_after_vad_s: 30,
            npu_pmode_min_interval_s: 5,
            epp_delta_max_per_tick: 64,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BanditConfig {
    pub alpha: f64,
}

impl Default for BanditConfig {
    fn default() -> Self {
        Self {
            alpha: DEFAULT_BANDIT_ALPHA,
        }
    }
}

/// SPEC §6 Open Question 5 — reward shaping weights. The canonical
/// scalar reward `compute_reward` combines:
///
/// - `perf_per_watt_weight` × Δ(work-proxy) / package_power_w
/// - minus `thermal_weight` × max(0, tctl − 80°C) / 10
/// - minus `thrash_weight` × `1{applied_arm ≠ prev_arm}`
///
/// Defaults match the SPEC §6 Open Question 5 answer (1.0 / 0.5 / 0.3) —
/// "perf/W dominates, thermal floor pushes back hard above 80°C, thrash
/// is a small but ever-present nudge against pointless arm churn."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RewardConfig {
    pub perf_per_watt_weight: f32,
    pub thermal_weight: f32,
    pub thrash_weight: f32,
}

/// SPEC §6 Open Question 5 defaults — pinned here so the bandit
/// reward fn and the `power.toml` head comment cannot drift.
pub const DEFAULT_PERF_PER_WATT_WEIGHT: f32 = 1.0;
pub const DEFAULT_THERMAL_WEIGHT: f32 = 0.5;
pub const DEFAULT_THRASH_WEIGHT: f32 = 0.3;

impl Default for RewardConfig {
    fn default() -> Self {
        Self {
            perf_per_watt_weight: DEFAULT_PERF_PER_WATT_WEIGHT,
            thermal_weight: DEFAULT_THERMAL_WEIGHT,
            thrash_weight: DEFAULT_THRASH_WEIGHT,
        }
    }
}

/// Hand-tuned `ShieldState -> arm-name` lookup table. Step 18's
/// `policy::rules_baseline` consults this struct; the bandit must
/// never be allowed to underperform the arm picked here (CLUCB
/// α-margin floor). Names match the eight canonical arms in
/// `configs/sy/power.toml` — `whisper, idle, browse, call, code,
/// build, npu-burst, flat-out`. The roadmap text reads "BATTERY_LOW
/// → quiet" but `quiet` is a `platform_profile` value, not a
/// canonical arm name; the lowest-power arm is `whisper` (platform
/// = quiet, epp = power, igpu = POWER_SAVING, npu = powersaver),
/// which is what this default ships.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RulesBaselineConfig {
    pub cool_ac: String,
    pub warm_ac: String,
    pub hot: String,
    pub battery_low: String,
    pub meeting: String,
}

impl Default for RulesBaselineConfig {
    fn default() -> Self {
        Self {
            cool_ac: "browse".to_string(),
            warm_ac: "code".to_string(),
            hot: "idle".to_string(),
            battery_low: "whisper".to_string(),
            meeting: "call".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OnboardingConfig {
    pub days: u32,
}

impl Default for OnboardingConfig {
    fn default() -> Self {
        Self {
            days: DEFAULT_ONBOARDING_DAYS,
        }
    }
}

impl PowerConfig {
    /// Load + apply env overrides. Missing file ⇒ defaults.
    pub fn load(path: &Path) -> Result<Self> {
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            toml::from_str::<PowerConfig>(&text)
                .with_context(|| format!("parse {}", path.display()))?
        } else {
            Self::default()
        };
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    /// Parse from a TOML string + apply env overrides. Tests-only
    /// helper today; promoted to `pub` when Step 35's `sy power
    /// show --config-from-stdin` lands.
    #[cfg(test)]
    fn from_str_with_env(text: &str) -> Result<Self> {
        let mut cfg: PowerConfig = toml::from_str(text).context("parse power config")?;
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var(ENV_ONBOARDING_DAYS) {
            if let Ok(n) = v.parse::<u32>() {
                self.onboarding.days = n;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per the AGENTS.md NPU-plane norms: env-var tests are flake-prone
    /// when parallelized — this serial mutex pins every env-touching
    /// test to one thread.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn defaults_match_spec() {
        let _g = ENV_LOCK.lock().unwrap();
        // Make sure the env override doesn't leak in from a sibling test.
        // Safe: held under ENV_LOCK so no other test races us.
        unsafe { std::env::remove_var(ENV_ONBOARDING_DAYS) };

        let cfg = PowerConfig::from_str_with_env("").expect("empty config parses");
        assert_eq!(cfg.bandit.alpha, DEFAULT_BANDIT_ALPHA);
        assert_eq!(cfg.onboarding.days, DEFAULT_ONBOARDING_DAYS);
    }

    #[test]
    fn onboarding_env_override() {
        let _g = ENV_LOCK.lock().unwrap();
        // Safe: held under ENV_LOCK so no other test races us.
        unsafe { std::env::set_var(ENV_ONBOARDING_DAYS, "5") };

        let cfg = PowerConfig::from_str_with_env("").expect("empty config parses");
        assert_eq!(cfg.onboarding.days, 5);

        unsafe { std::env::remove_var(ENV_ONBOARDING_DAYS) };
    }

    #[test]
    fn shield_defaults_match_spec_table() {
        // SPEC §4 "Concrete Shield Constraint Set (HX 370)" canonical
        // values — these are the rows of the SPEC's constraint table.
        // Test pinned so any drift between the SPEC and the struct
        // becomes a failing test the next person to touch the table
        // must reconcile.
        let s = ShieldConfig::default();
        assert!((s.tctl_peak_c - 90.0).abs() < 1e-3);
        assert!((s.tctl_act_c - 85.0).abs() < 1e-3);
        assert!((s.tctl_sustained_60s_avg_c - 80.0).abs() < 1e-3);
        assert!((s.package_power_excursion_w_per_2s - 15.0).abs() < 1e-3);
        assert_eq!(s.fan_rpm_max, 5500);
        assert_eq!(s.battery_low_dc_pct, 25);
        assert_eq!(s.battery_emergency_dc_pct, 10);
        assert_eq!(s.profile_thrash_min_interval_s, 30);
        assert_eq!(s.meeting_lock_after_vad_s, 30);
        assert_eq!(s.npu_pmode_min_interval_s, 5);
        assert_eq!(s.epp_delta_max_per_tick, 64);
    }

    #[test]
    fn shield_stanza_overrides_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var(ENV_ONBOARDING_DAYS) };
        let cfg = PowerConfig::from_str_with_env(
            "[shield]\ntctl_act_c = 80.0\nbattery_emergency_dc_pct = 5\n",
        )
        .expect("toml parses");
        assert!((cfg.shield.tctl_act_c - 80.0).abs() < 1e-3);
        assert_eq!(cfg.shield.battery_emergency_dc_pct, 5);
        // Untouched fields still default.
        assert!((cfg.shield.tctl_peak_c - 90.0).abs() < 1e-3);
    }

    #[test]
    fn rules_baseline_defaults_canonical_arms() {
        // Step 18 anchor: the floor table maps every shield state to
        // one canonical arm name. Drift here means CLUCB's α-margin
        // floor diverges from SPEC §4.
        let r = RulesBaselineConfig::default();
        assert_eq!(r.cool_ac, "browse");
        assert_eq!(r.warm_ac, "code");
        assert_eq!(r.hot, "idle");
        assert_eq!(r.battery_low, "whisper");
        assert_eq!(r.meeting, "call");
    }

    #[test]
    fn rules_baseline_stanza_overrides_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var(ENV_ONBOARDING_DAYS) };
        let cfg = PowerConfig::from_str_with_env("[rules_baseline]\nhot = \"whisper\"\n")
            .expect("toml parses");
        assert_eq!(cfg.rules_baseline.hot, "whisper");
        // Untouched fields still default.
        assert_eq!(cfg.rules_baseline.cool_ac, "browse");
    }

    #[test]
    fn reward_defaults_match_spec_q5() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var(ENV_ONBOARDING_DAYS) };
        let cfg = PowerConfig::from_str_with_env("").expect("empty config parses");
        // SPEC §6 Open Question 5 canonical defaults.
        assert!((cfg.reward.perf_per_watt_weight - DEFAULT_PERF_PER_WATT_WEIGHT).abs() < 1e-6);
        assert!((cfg.reward.thermal_weight - DEFAULT_THERMAL_WEIGHT).abs() < 1e-6);
        assert!((cfg.reward.thrash_weight - DEFAULT_THRASH_WEIGHT).abs() < 1e-6);
    }

    #[test]
    fn reward_stanza_overrides_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var(ENV_ONBOARDING_DAYS) };
        let cfg = PowerConfig::from_str_with_env(
            "[reward]\nperf_per_watt_weight = 2.5\nthermal_weight = 1.5\n",
        )
        .expect("toml parses");
        assert!((cfg.reward.perf_per_watt_weight - 2.5).abs() < 1e-6);
        assert!((cfg.reward.thermal_weight - 1.5).abs() < 1e-6);
        // Untouched fields still default.
        assert!((cfg.reward.thrash_weight - DEFAULT_THRASH_WEIGHT).abs() < 1e-6);
    }

    #[test]
    fn alpha_can_be_overridden_in_toml() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var(ENV_ONBOARDING_DAYS) };

        let cfg = PowerConfig::from_str_with_env("[bandit]\nalpha = 0.1\n").expect("toml parses");
        assert!((cfg.bandit.alpha - 0.1).abs() < 1e-9);
        // Onboarding still default when the stanza is absent.
        assert_eq!(cfg.onboarding.days, DEFAULT_ONBOARDING_DAYS);
    }
}
