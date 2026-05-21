//! `load_arms(&PowerConfig) -> Result<Vec<Arm>>` — read the arm table
//! out of an already-parsed `PowerConfig` and surface clean validation
//! errors. Step 14 keeps validation enum-bounded; the sysfs-choices
//! cross-check happens at actuation time in Step 15.

use anyhow::Result;

use super::Arm;
use crate::power::config::PowerConfig;

/// Pull the arm table out of `cfg`. Each `ArmConfig` already passed
/// serde's enum-bounded validation when the TOML was parsed; this
/// helper just rebuilds the runtime `Arm` view and is the canonical
/// entry point for `sy power list-profiles`, the bandit policy
/// (Step 17), and the shield projection (Step 12).
pub fn load_arms(cfg: &PowerConfig) -> Result<Vec<Arm>> {
    Ok(cfg.arms.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `configs/sy/power.toml` MUST ship the eight canonical arms in
    /// the SPEC §4 order; the audit log + `sy power profile <name>`
    /// rely on those names being stable identifiers.
    const CANONICAL_NAMES: [&str; 8] = [
        "whisper",
        "idle",
        "browse",
        "call",
        "code",
        "build",
        "npu-burst",
        "flat-out",
    ];

    fn shipped_config_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml")
    }

    #[test]
    fn loads_eight_canonical_arms() {
        let cfg = PowerConfig::load(&shipped_config_path()).expect("shipped power.toml parses");
        let arms = load_arms(&cfg).expect("load_arms");
        let names: Vec<&str> = arms.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names, CANONICAL_NAMES,
            "shipped power.toml must enumerate the SPEC §4 arms in order",
        );
    }

    /// A TOML override with `platform_profile = "ludicrous"` must fail
    /// to load — the deserializer rejects unknown enum strings so a
    /// typo surfaces at config-load, not at actuation time.
    #[test]
    fn rejects_unknown_platform_profile() {
        let bad = r#"
[[arms]]
name = "ludicrous-speed"
platform_profile = "ludicrous"
epp = "performance"
igpu_mode = "POWER_SAVING"
npu_pmode = "turbo"
"#;
        let err = toml::from_str::<PowerConfig>(bad).expect_err("ludicrous must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("ludicrous"),
            "error must name the bad value: {msg}",
        );
    }
}
