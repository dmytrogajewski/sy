//! cgroup v2 actuator — sy-power Step 16 lever E.
//!
//! Writes `cpu.weight`, `cpu.uclamp.min`, and `cpu.uclamp.max` under
//! the daemon's own systemd `--user` scope. Production path is
//! `/sys/fs/cgroup/user.slice/user-$UID.slice/user@$UID.service/
//! app.slice/sy-powerd.service/` — the daemon is a `--user` service,
//! so its scope lives under `app.slice`. The actuator takes the
//! cgroup root as a `&Path` so tests use a tempdir and never touch
//! the live cgroup hierarchy.
//!
//! Inputs are the optional fields of [`CgroupOverrides`] (per
//! `bandit/mod.rs`): each `None` field is left untouched, so an arm
//! that only tunes `cpu_uclamp_max` doesn't disturb the others. All
//! three writes go through [`super::write_if_changed`] for
//! idempotency.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::super::bandit::CgroupOverrides;
use super::{write_if_changed, Actuator, Applied};

/// File names on a cgroup v2 mount. `cpu.uclamp.*` are exposed as
/// percentages 0..100 (or `max` for `cpu.uclamp.max=100`); we write
/// the numeric form unconditionally — the kernel accepts both.
const CPU_WEIGHT: &str = "cpu.weight";
const CPU_UCLAMP_MIN: &str = "cpu.uclamp.min";
const CPU_UCLAMP_MAX: &str = "cpu.uclamp.max";

/// SPEC §4 + `CgroupOverrides` doc-comment range checks. We refuse
/// out-of-range writes here (rather than letting the kernel return
/// EINVAL) so the audit log carries a structured reason instead of
/// "write failed".
const CPU_WEIGHT_MIN: u32 = 1;
const CPU_WEIGHT_MAX: u32 = 10_000;
const CPU_UCLAMP_PCT_MAX: u8 = 100;

/// Structured cgroup actuator errors. Distinct from `anyhow::Error`
/// so the daemon (Step 19) can downcast and decide whether to drop
/// the arm (`OutOfRange`) or fall through (`MissingScope`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CgroupError {
    /// The cgroup scope directory doesn't exist under `cgroup_root`.
    /// On a real host this means systemd hasn't created the unit's
    /// scope yet (daemon not started). The actuator returns this so
    /// the caller can warn-and-skip rather than crash.
    MissingScope { path: PathBuf },
    /// One of the override fields is outside the documented range.
    /// `field` is the cgroup leaf name; `value` is the rendered
    /// out-of-range number.
    OutOfRange { field: &'static str, value: String },
}

impl fmt::Display for CgroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CgroupError::MissingScope { path } => {
                write!(f, "cgroup scope {} not present", path.display())
            }
            CgroupError::OutOfRange { field, value } => {
                write!(f, "cgroup {field} value {value} out of range")
            }
        }
    }
}

impl std::error::Error for CgroupError {}

/// Zero-sized actuator. The cgroup-scope path is passed in per-call
/// so the daemon can switch between its own scope and a parent slice
/// without re-instantiating.
#[derive(Debug, Default)]
pub struct CgroupActuator;

impl CgroupActuator {
    pub fn new() -> Self {
        Self
    }
}

impl Actuator for CgroupActuator {
    type Target = CgroupOverrides;
    /// `sysfs_root` here is the **cgroup scope directory** (e.g.
    /// `…/sy-powerd.service/`), not the global `/sys` root. The
    /// `Actuator` trait takes a `&Path` and we re-use the same slot —
    /// the caller knows which root applies for which actuator.
    fn apply(&self, target: Self::Target, sysfs_root: &Path) -> Result<Applied> {
        set_cgroup(target, sysfs_root)
    }
}

/// Apply each present override to its cgroup leaf. Mirrors the
/// `EppActuator` aggregator: if any field caused a write, return the
/// first written path; if every field was a no-op or absent, return
/// `Applied::NoChange`.
pub fn set_cgroup(target: CgroupOverrides, cgroup_root: &Path) -> Result<Applied> {
    if !cgroup_root.is_dir() {
        return Err(CgroupError::MissingScope {
            path: cgroup_root.to_path_buf(),
        }
        .into());
    }
    let mut first_wrote: Option<PathBuf> = None;
    let mut last_value = String::new();
    if let Some(weight) = target.cpu_weight {
        if !(CPU_WEIGHT_MIN..=CPU_WEIGHT_MAX).contains(&weight) {
            return Err(CgroupError::OutOfRange {
                field: CPU_WEIGHT,
                value: weight.to_string(),
            }
            .into());
        }
        let rendered = weight.to_string();
        match write_if_changed(&cgroup_root.join(CPU_WEIGHT), &rendered)? {
            Applied::Wrote { path, value } => {
                if first_wrote.is_none() {
                    first_wrote = Some(path);
                }
                last_value = value;
            }
            Applied::NoChange => {}
        }
    }
    if let Some(min) = target.cpu_uclamp_min {
        if min > CPU_UCLAMP_PCT_MAX {
            return Err(CgroupError::OutOfRange {
                field: CPU_UCLAMP_MIN,
                value: min.to_string(),
            }
            .into());
        }
        let rendered = min.to_string();
        match write_if_changed(&cgroup_root.join(CPU_UCLAMP_MIN), &rendered)? {
            Applied::Wrote { path, value } => {
                if first_wrote.is_none() {
                    first_wrote = Some(path);
                }
                last_value = value;
            }
            Applied::NoChange => {}
        }
    }
    if let Some(max) = target.cpu_uclamp_max {
        if max > CPU_UCLAMP_PCT_MAX {
            return Err(CgroupError::OutOfRange {
                field: CPU_UCLAMP_MAX,
                value: max.to_string(),
            }
            .into());
        }
        let rendered = max.to_string();
        match write_if_changed(&cgroup_root.join(CPU_UCLAMP_MAX), &rendered)? {
            Applied::Wrote { path, value } => {
                if first_wrote.is_none() {
                    first_wrote = Some(path);
                }
                last_value = value;
            }
            Applied::NoChange => {}
        }
    }
    Ok(match first_wrote {
        Some(path) => Applied::Wrote {
            path,
            value: last_value,
        },
        None => Applied::NoChange,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a cgroup v2 scope under `td`: empty `cpu.weight` +
    /// `cpu.uclamp.{min,max}` files so the actuator's read-before-write
    /// has something to diff against. Returns the scope dir — that's
    /// what callers pass as `cgroup_root`.
    fn fixture(td: &tempfile::TempDir, weight: &str, min: &str, max: &str) -> PathBuf {
        let scope = td.path().join("sy-powerd.service");
        fs::create_dir_all(&scope).expect("mkdir scope");
        fs::write(scope.join(CPU_WEIGHT), format!("{weight}\n")).expect("seed weight");
        fs::write(scope.join(CPU_UCLAMP_MIN), format!("{min}\n")).expect("seed uclamp_min");
        fs::write(scope.join(CPU_UCLAMP_MAX), format!("{max}\n")).expect("seed uclamp_max");
        scope
    }

    /// Roadmap §16 required: set `uclamp_min=30`, read back, assert
    /// match. Belt + suspenders: also verify the [`Applied`] result
    /// names the touched leaf so the audit log records the right path.
    #[test]
    fn uclamp_min_round_trips() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let scope = fixture(&td, "100", "0", "100");
        const TARGET_MIN: u8 = 30;
        let target = CgroupOverrides {
            cpu_uclamp_min: Some(TARGET_MIN),
            ..Default::default()
        };
        let out = set_cgroup(target, &scope).expect("apply");
        match out {
            Applied::Wrote { path, value } => {
                assert_eq!(path, scope.join(CPU_UCLAMP_MIN));
                assert_eq!(value, "30");
            }
            other => panic!("expected Wrote, got {other:?}"),
        }
        let after = fs::read_to_string(scope.join(CPU_UCLAMP_MIN)).expect("read");
        assert_eq!(after.trim(), "30");
    }

    /// Idempotency: an arm whose override already matches the current
    /// scope value short-circuits to `NoChange`. Mirrors the
    /// platform_profile + EPP contracts so the audit log treats every
    /// actuator uniformly.
    #[test]
    fn no_change_when_every_field_matches() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let scope = fixture(&td, "200", "30", "70");
        let target = CgroupOverrides {
            cpu_uclamp_min: Some(30),
            cpu_uclamp_max: Some(70),
            cpu_weight: Some(200),
        };
        let out = set_cgroup(target, &scope).expect("apply");
        assert_eq!(out, Applied::NoChange);
    }

    /// `None` fields must not touch the corresponding leaf. The
    /// fixture starts with `cpu.weight=500`; an override that only
    /// pins `cpu_uclamp_max` must leave the weight at 500.
    #[test]
    fn absent_field_leaves_leaf_alone() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let scope = fixture(&td, "500", "0", "100");
        let target = CgroupOverrides {
            cpu_uclamp_max: Some(40),
            ..Default::default()
        };
        let _ = set_cgroup(target, &scope).expect("apply");
        let weight = fs::read_to_string(scope.join(CPU_WEIGHT)).expect("read");
        assert_eq!(weight, "500\n", "absent field must not touch leaf");
        let max = fs::read_to_string(scope.join(CPU_UCLAMP_MAX)).expect("read");
        assert_eq!(max.trim(), "40");
    }

    /// Out-of-range `cpu_weight` (kernel accepts 1..=10_000) surfaces
    /// as `CgroupError::OutOfRange` rather than a generic write
    /// failure — the audit log needs to record the structural reason.
    #[test]
    fn rejects_out_of_range_weight() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let scope = fixture(&td, "100", "0", "100");
        let target = CgroupOverrides {
            cpu_weight: Some(20_000),
            ..Default::default()
        };
        let err = set_cgroup(target, &scope).expect_err("must reject");
        let ce = err
            .downcast_ref::<CgroupError>()
            .expect("error must be CgroupError");
        assert!(
            matches!(ce, CgroupError::OutOfRange { field, .. } if *field == CPU_WEIGHT),
            "expected OutOfRange(cpu.weight), got {ce:?}",
        );
    }

    /// `MissingScope` path: when the cgroup dir doesn't exist
    /// (daemon not started, systemd hasn't materialised the scope),
    /// the actuator surfaces the structured error so the caller can
    /// warn-and-skip rather than panic.
    #[test]
    fn missing_scope_surfaces_structured_error() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let bogus = td.path().join("does-not-exist");
        let target = CgroupOverrides {
            cpu_uclamp_min: Some(10),
            ..Default::default()
        };
        let err = set_cgroup(target, &bogus).expect_err("must error");
        let ce = err
            .downcast_ref::<CgroupError>()
            .expect("error must be CgroupError");
        assert!(matches!(ce, CgroupError::MissingScope { .. }), "got {ce:?}");
    }
}
