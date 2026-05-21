//! `amd-pstate` reader.
//!
//! Reads `scaling_governor` + `energy_performance_preference` from
//! `/sys/devices/system/cpu/cpufreq/policy0/` (policy0 stands in for
//! the package — all policies on AMD share the EPP knob).
//!
//! Surfaces a `Blocked` reading when `amd_dynamic_epp=enable` so the
//! daemon does not waste a bandit pull on a knob the kernel ignores
//! (SPEC §2 "writable knobs: EPP", §4.4 amd_dynamic_epp drop-in).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Sensor, SensorReading};

/// Kernel module parameter that, when `enable`, makes EPP writes a
/// silent no-op. Step 27's grub drop-in flips this to `disable`.
const DYNAMIC_EPP_PARAM: &str = "module/amd_pstate/parameters/dynamic_epp";

/// Canonical "primary" cpufreq policy. AMD shares EPP/governor across
/// policies so policy0 is representative.
const POLICY_DIR: &str = "devices/system/cpu/cpufreq/policy0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Governor {
    Schedutil,
    Powersave,
    Performance,
    Ondemand,
    Conservative,
    Userspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Epp {
    Default,
    Performance,
    BalancePerformance,
    BalancePower,
    Power,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PstateReading {
    pub governor: Governor,
    pub epp: Epp,
}

#[derive(Debug, Default)]
pub struct PstateSensor;

impl PstateSensor {
    pub fn new() -> Self {
        Self
    }
}

impl Sensor for PstateSensor {
    fn read(&self, sysfs_root: &Path) -> Result<SensorReading> {
        if dynamic_epp_enabled(sysfs_root)? {
            // Per roadmap §2: when `amd_dynamic_epp=enable` the EPP
            // lever is silently no-op — surface that as a top-level
            // `Blocked` reading, not a stale `Set(_)` value.
            return Ok(SensorReading::Blocked);
        }
        let policy = sysfs_root.join(POLICY_DIR);
        let governor = parse_governor(&read_trim(&policy.join("scaling_governor"))?)?;
        let epp = parse_epp(&read_trim(&policy.join("energy_performance_preference"))?)?;
        Ok(SensorReading::Pstate(PstateReading { governor, epp }))
    }
}

fn read_trim(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(s.trim().to_string())
}

fn dynamic_epp_enabled(sysfs_root: &Path) -> Result<bool> {
    let path: PathBuf = sysfs_root.join(DYNAMIC_EPP_PARAM);
    if !path.exists() {
        return Ok(false);
    }
    Ok(read_trim(&path)? == "enable")
}

fn parse_governor(raw: &str) -> Result<Governor> {
    Ok(match raw {
        "schedutil" => Governor::Schedutil,
        "powersave" => Governor::Powersave,
        "performance" => Governor::Performance,
        "ondemand" => Governor::Ondemand,
        "conservative" => Governor::Conservative,
        "userspace" => Governor::Userspace,
        other => anyhow::bail!("unknown scaling_governor: {other:?}"),
    })
}

fn parse_epp(raw: &str) -> Result<Epp> {
    Ok(match raw {
        "default" => Epp::Default,
        "performance" => Epp::Performance,
        "balance_performance" => Epp::BalancePerformance,
        "balance_power" => Epp::BalancePower,
        "power" => Epp::Power,
        other => anyhow::bail!("unknown energy_performance_preference: {other:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/power/fixtures/sys")
            .join(name)
    }

    #[test]
    fn parses_governor_powersave() {
        // Override the hx370 fixture's schedutil to powersave for this
        // round-trip — write a tmpdir mirror so we don't mutate the
        // canonical fixture committed in-tree.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let policy = tmp.path().join(POLICY_DIR);
        std::fs::create_dir_all(&policy).expect("mkdir policy");
        std::fs::write(policy.join("scaling_governor"), "powersave\n").expect("write gov");
        std::fs::write(
            policy.join("energy_performance_preference"),
            "balance_performance\n",
        )
        .expect("write epp");

        let r = PstateSensor::new().read(tmp.path()).expect("pstate read");
        match r {
            SensorReading::Pstate(p) => {
                assert_eq!(p.governor, Governor::Powersave);
                assert_eq!(p.epp, Epp::BalancePerformance);
            }
            other => panic!("expected Pstate reading, got {other:?}"),
        }
    }

    #[test]
    fn epp_blocked_when_dynamic_enabled() {
        let r = PstateSensor::new()
            .read(&fixture("hx370-dynamic-epp"))
            .expect("pstate read");
        assert_eq!(
            r,
            SensorReading::Blocked,
            "amd_dynamic_epp=enable must surface as Blocked, got {r:?}",
        );
    }

    #[test]
    fn hx370_fixture_parses_balance_performance() {
        let r = PstateSensor::new()
            .read(&fixture("hx370"))
            .expect("pstate read");
        match r {
            SensorReading::Pstate(p) => {
                assert_eq!(p.governor, Governor::Schedutil);
                assert_eq!(p.epp, Epp::BalancePerformance);
            }
            other => panic!("expected Pstate reading, got {other:?}"),
        }
    }
}
