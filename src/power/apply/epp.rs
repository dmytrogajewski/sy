//! `energy_performance_preference` actuator — sy-power Step 15 lever B.
//!
//! Writes the requested EPP value to every
//! `cpufreq/policy*/energy_performance_preference` node under
//! `sysfs_root`. AMD shares the EPP knob across policies but the
//! kernel exposes one file per policy and refuses an aggregate write,
//! so the actuator iterates explicitly.
//!
//! Degrades cleanly when `amd_pstate.dynamic_epp=enable`: that
//! kernel parameter makes EPP writes a silent no-op (kernel hands the
//! lever back to the platform firmware), and the matching
//! `PstateSensor` already returns `Blocked` in that case. We re-use
//! the sensor — one source of truth, one place to update if the
//! kernel renames the parameter — and refuse to apply, surfacing a
//! `EppError::EppBlocked` carrying the operator-facing remediation
//! hint (Step 27's grub drop-in disables it).

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::super::bandit::Epp;
use super::super::sensors::pstate::PstateSensor;
use super::super::sensors::{Sensor, SensorReading};
use super::{write_if_changed, Actuator, Applied};

/// Subdir under `sysfs_root` housing the cpufreq policy directories
/// (one per `policy<N>`). The leaf file we touch is
/// `energy_performance_preference`.
const CPUFREQ_DIR: &str = "devices/system/cpu/cpufreq";
const EPP_LEAF: &str = "energy_performance_preference";

/// Operator-facing remediation when `amd_dynamic_epp=enable` is set.
/// Step 27 lands the drop-in that flips this automatically; the hint
/// is a backstop for hosts that haven't run `sy power apply` yet.
const EPP_BLOCKED_HINT: &str = "Add `amd_dynamic_epp=disable` to GRUB_CMDLINE_LINUX, then reboot.";

/// Structured actuator errors. `EppBlocked` is distinct from a sysfs
/// I/O failure so the daemon (Step 19) can degrade gracefully — log
/// once, drop the EPP arm from the bandit, leave platform_profile
/// untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EppError {
    /// `amd_dynamic_epp=enable` is active; EPP writes would be
    /// silently ignored. `hint` is the operator-facing fix.
    EppBlocked { hint: &'static str },
}

impl fmt::Display for EppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EppError::EppBlocked { hint } => {
                write!(f, "EPP writes blocked by amd_dynamic_epp=enable. {hint}")
            }
        }
    }
}

impl std::error::Error for EppError {}

/// Zero-sized actuator implementing [`Actuator`] for [`Epp`].
#[derive(Debug, Default)]
pub struct EppActuator;

impl EppActuator {
    pub fn new() -> Self {
        Self
    }
}

impl Actuator for EppActuator {
    type Target = Epp;
    fn apply(&self, target: Self::Target, sysfs_root: &Path) -> Result<Applied> {
        set_epp(target, sysfs_root)
    }
}

/// Functional entrypoint. Returns aggregated [`Applied`]: if any
/// policy was written, returns `Wrote { path: <first-written>, value }`;
/// if every policy already matched, returns `NoChange`. This is the
/// most useful summary for the daemon — it cares whether the system
/// state changed, not how many of the sixteen policies got touched.
pub fn set_epp(value: Epp, sysfs_root: &Path) -> Result<Applied> {
    if matches!(
        PstateSensor::new().read(sysfs_root)?,
        SensorReading::Blocked
    ) {
        return Err(EppError::EppBlocked {
            hint: EPP_BLOCKED_HINT,
        }
        .into());
    }
    let rendered = render(value);
    let policies = enumerate_policies(sysfs_root)?;
    let mut first_wrote: Option<PathBuf> = None;
    for policy in &policies {
        let leaf = policy.join(EPP_LEAF);
        if let Applied::Wrote { path, .. } = write_if_changed(&leaf, &rendered)? {
            if first_wrote.is_none() {
                first_wrote = Some(path);
            }
        }
    }
    Ok(match first_wrote {
        Some(path) => Applied::Wrote {
            path,
            value: rendered,
        },
        None => Applied::NoChange,
    })
}

/// Canonical string for each `Epp` variant. Matches the kernel's
/// accepted set verbatim (`balance_performance`, etc.) so the kernel
/// parser doesn't reject our write.
fn render(value: Epp) -> String {
    match value {
        Epp::Performance => "performance".to_string(),
        Epp::BalancePerformance => "balance_performance".to_string(),
        Epp::Default => "default".to_string(),
        Epp::BalancePower => "balance_power".to_string(),
        Epp::Power => "power".to_string(),
    }
}

/// List every `cpufreq/policy*` directory under `sysfs_root`, sorted
/// lexicographically so the "first written" path in the aggregated
/// `Applied` is deterministic (matters for the audit log).
fn enumerate_policies(sysfs_root: &Path) -> Result<Vec<PathBuf>> {
    let root = sysfs_root.join(CPUFREQ_DIR);
    let mut policies: Vec<PathBuf> = Vec::new();
    let iter = match std::fs::read_dir(&root) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(policies),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("read_dir {}", root.display())));
        }
    };
    for entry in iter {
        let entry = entry.with_context(|| format!("iterate {}", root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.starts_with("policy") {
            policies.push(path);
        }
    }
    policies.sort();
    Ok(policies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a `devices/system/cpu/cpufreq/policy<N>/` tree with N
    /// policy directories under `td`. Each policy starts with the
    /// `seed_current` value so we can exercise both diff branches.
    fn fixture_with_policies(
        td: &tempfile::TempDir,
        n_policies: usize,
        seed_current: &str,
    ) -> PathBuf {
        let root = td.path().to_path_buf();
        let cpufreq = root.join(CPUFREQ_DIR);
        fs::create_dir_all(&cpufreq).expect("mkdir cpufreq");
        for i in 0..n_policies {
            let p = cpufreq.join(format!("policy{i}"));
            fs::create_dir_all(&p).expect("mkdir policy");
            fs::write(p.join("scaling_governor"), "schedutil\n").expect("seed governor");
            fs::write(p.join(EPP_LEAF), format!("{seed_current}\n")).expect("seed epp");
        }
        root
    }

    /// Step 15 required: a write covers EVERY `policy*` directory.
    /// Fixture has 4 of them; assert all 4 end up with the new value.
    #[test]
    fn writes_to_every_policy() {
        const N_POLICIES: usize = 4;
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture_with_policies(&td, N_POLICIES, "balance_performance");
        let out = set_epp(Epp::Performance, &root).expect("apply");
        match out {
            Applied::Wrote { value, .. } => assert_eq!(value, "performance"),
            other => panic!("expected Wrote, got {other:?}"),
        }
        for i in 0..N_POLICIES {
            let leaf = root
                .join(CPUFREQ_DIR)
                .join(format!("policy{i}"))
                .join(EPP_LEAF);
            let got = fs::read_to_string(&leaf).expect("read after");
            assert_eq!(
                got.trim(),
                "performance",
                "policy{i} unchanged: {got:?} (path={leaf:?})",
            );
        }
    }

    /// Idempotency aggregator: every policy already matches the
    /// requested value ⇒ `Applied::NoChange`. Mirrors the
    /// platform_profile contract.
    #[test]
    fn no_change_when_every_policy_matches() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture_with_policies(&td, 2, "balance_performance");
        let out = set_epp(Epp::BalancePerformance, &root).expect("apply");
        assert_eq!(out, Applied::NoChange);
    }

    /// Step 15 required: when the host has `amd_dynamic_epp=enable`,
    /// the actuator refuses the write with `EppError::EppBlocked` and
    /// the error message mentions the kernel cmdline remediation —
    /// the operator must learn how to unstick this without reading
    /// the source.
    #[test]
    fn degrades_when_amd_dynamic_epp_enabled() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture_with_policies(&td, 2, "balance_performance");
        // Stamp `module/amd_pstate/parameters/dynamic_epp=enable` to
        // mirror the hx370-dynamic-epp fixture used by Step 2's
        // sensor tests.
        let dyn_dir = root.join("module/amd_pstate/parameters");
        fs::create_dir_all(&dyn_dir).expect("mkdir dyn_epp parent");
        fs::write(dyn_dir.join("dynamic_epp"), "enable\n").expect("seed dyn_epp");

        let err = set_epp(Epp::Performance, &root).expect_err("must error");
        let ee = err
            .downcast_ref::<EppError>()
            .expect("error must be EppError");
        assert_eq!(
            ee,
            &EppError::EppBlocked {
                hint: EPP_BLOCKED_HINT,
            },
        );
        let rendered = format!("{ee}");
        assert!(
            rendered.contains("amd_dynamic_epp=disable"),
            "EppBlocked must surface the kernel-cmdline fix: {rendered}",
        );
    }
}
