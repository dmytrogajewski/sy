//! `platform_profile` actuator — sy-power Step 15 lever A.
//!
//! Writes to `/sys/firmware/acpi/platform_profile` after validating
//! the requested value against `…/platform_profile_choices`. Idempotent
//! via [`super::write_if_changed`]: if the kernel already reports the
//! requested profile we short-circuit to [`Applied::NoChange`].
//!
//! Polkit caveat documented in `super::mod.rs`: the production polkit
//! rule (Step 13) grants `org.sy.PowerProfile.SetProfile` to `wheel`
//! for a *future* D-Bus path (Step 36). For Step 15 we write the
//! sysfs node directly; Fedora 43 ships it `wheel:wheel rw-rw-r--`,
//! which is exactly the permission set the rule prepares for.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::super::sensors::platform::PlatformProfile;
use super::{write_if_changed, Actuator, Applied};

/// Sysfs nodes consumed by the platform_profile actuator. `_choices`
/// is the kernel-supplied whitespace list of supported values — we
/// validate against it so attempting `performance` on a quiet-only
/// laptop fails loud (with the operator-readable hint listing what
/// the host *does* support) instead of EINVAL-ing at write time.
const PROFILE_PATH: &str = "firmware/acpi/platform_profile";
const PROFILE_CHOICES_PATH: &str = "firmware/acpi/platform_profile_choices";

/// Actuator errors. Distinct from `anyhow::Error` so the daemon can
/// `downcast_ref` and match on the structural failure mode (unsupported
/// vs sysfs IO).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformError {
    /// Requested profile not present in `platform_profile_choices`.
    /// `available` is the kernel-reported list so the operator can
    /// pick a valid value without `cat`-ing sysfs by hand.
    UnsupportedProfile {
        requested: String,
        available: Vec<String>,
    },
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlatformError::UnsupportedProfile {
                requested,
                available,
            } => write!(
                f,
                "platform profile {requested:?} not in kernel-supported choices {available:?}",
            ),
        }
    }
}

impl std::error::Error for PlatformError {}

/// Zero-sized actuator type. Stateless: every call re-reads sysfs.
/// Kept as a struct (not a free function) so it slots into the
/// [`Actuator`] trait the daemon (Step 19) walks generically.
#[derive(Debug, Default)]
pub struct PlatformProfileActuator;

impl PlatformProfileActuator {
    pub fn new() -> Self {
        Self
    }
}

impl Actuator for PlatformProfileActuator {
    type Target = PlatformProfile;
    fn apply(&self, target: Self::Target, sysfs_root: &Path) -> Result<Applied> {
        set_platform_profile(target, sysfs_root)
    }
}

/// Functional entrypoint — `PlatformProfileActuator::apply` is a thin
/// shim over this. Split so callers that already know the value
/// (e.g. `cli::status`'s no-op probe) don't have to construct the
/// zero-sized struct.
pub fn set_platform_profile(value: PlatformProfile, sysfs_root: &Path) -> Result<Applied> {
    let rendered = render(&value);
    let choices = read_choices(sysfs_root)?;
    if !choices.iter().any(|c| c == &rendered) {
        return Err(PlatformError::UnsupportedProfile {
            requested: rendered,
            available: choices,
        }
        .into());
    }
    write_if_changed(&sysfs_root.join(PROFILE_PATH), &rendered)
}

/// Render the enum back to the canonical sysfs string. Mirrors
/// `sensors::platform::profile_as_str` but kept local to avoid
/// exporting a sensor-internal helper.
fn render(p: &PlatformProfile) -> String {
    match p {
        PlatformProfile::Quiet => "quiet".to_string(),
        PlatformProfile::Balanced => "balanced".to_string(),
        PlatformProfile::BalancedPerformance => "balanced-performance".to_string(),
        PlatformProfile::Performance => "performance".to_string(),
        PlatformProfile::LowPower => "low-power".to_string(),
        PlatformProfile::Cool => "cool".to_string(),
        PlatformProfile::Other(s) => s.clone(),
    }
}

/// Read and split `platform_profile_choices`. Each token is one
/// kernel-supported value verbatim — we compare on the rendered
/// enum, not on the enum's variant identity, so vendor-specific
/// strings (`PlatformProfile::Other(_)`) round-trip cleanly.
fn read_choices(sysfs_root: &Path) -> Result<Vec<String>> {
    let path: PathBuf = sysfs_root.join(PROFILE_CHOICES_PATH);
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(raw.split_ascii_whitespace().map(String::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Build a minimal `firmware/acpi/` tree under `td` mirroring the
    /// shipped `fixtures/sys/hx370/` shape: `platform_profile` set to
    /// `balanced` and `platform_profile_choices` listing the SPEC §4
    /// trio. Returns the sysfs root the actuator should be invoked
    /// against.
    fn fixture(td: &tempfile::TempDir, current: &str, choices: &str) -> PathBuf {
        let root = td.path().to_path_buf();
        let acpi = root.join("firmware/acpi");
        fs::create_dir_all(&acpi).expect("mkdir acpi");
        fs::write(acpi.join("platform_profile"), format!("{current}\n"))
            .expect("seed platform_profile");
        fs::write(
            acpi.join("platform_profile_choices"),
            format!("{choices}\n"),
        )
        .expect("seed platform_profile_choices");
        root
    }

    /// Idempotency contract: setting the *current* profile returns
    /// `NoChange` and the sysfs file is byte-identical afterwards.
    #[test]
    fn skip_when_already_matches() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture(&td, "balanced", "quiet balanced performance");
        let out = set_platform_profile(PlatformProfile::Balanced, &root).expect("apply");
        assert_eq!(out, Applied::NoChange);
        let after = fs::read_to_string(root.join(PROFILE_PATH)).expect("read after");
        assert_eq!(after, "balanced\n");
    }

    /// Choices-list validation: a value not present in
    /// `platform_profile_choices` must surface as
    /// `PlatformError::UnsupportedProfile`. The error must name the
    /// requested value AND the kernel-supported set so the operator
    /// can fix `power.toml` without `cat`-ing sysfs themselves.
    #[test]
    fn rejects_unknown_profile() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture(&td, "balanced", "quiet balanced performance");
        let err = set_platform_profile(PlatformProfile::LowPower, &root)
            .expect_err("low-power not in HX 370 choices");
        let pe = err
            .downcast_ref::<PlatformError>()
            .expect("error must be PlatformError");
        match pe {
            PlatformError::UnsupportedProfile {
                requested,
                available,
            } => {
                assert_eq!(requested, "low-power");
                assert!(
                    available.contains(&"balanced".to_string()),
                    "available must list kernel choices, got {available:?}",
                );
            }
        }
    }

    /// A mismatched write succeeds and the actuator reports the path
    /// it touched + the value it wrote. Belt-and-suspenders alongside
    /// `apply/mod.rs::write_if_changed_writes_on_mismatch` — same
    /// shape but exercised through the strongly-typed actuator entry.
    #[test]
    fn writes_when_mismatch() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture(&td, "balanced", "quiet balanced performance");
        let out = set_platform_profile(PlatformProfile::Performance, &root).expect("apply");
        match out {
            Applied::Wrote { path, value } => {
                assert_eq!(path, root.join(PROFILE_PATH));
                assert_eq!(value, "performance");
            }
            other => panic!("expected Wrote, got {other:?}"),
        }
        let after = fs::read_to_string(root.join(PROFILE_PATH)).expect("read after");
        assert_eq!(after, "performance");
    }
}
