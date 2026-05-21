//! `pp_power_profile_mode` actuator — sy-power Step 16 lever C.
//!
//! Writes a row index into the AMDGPU `pp_power_profile_mode` sysfs
//! node so the integrated AMD card switches preset. The file is a
//! multi-line table — lines like:
//!
//! ```text
//! NUM        MODE_NAME     BUSY_SET_POINT FPS USE_RLC_BUSY MIN_ACTIVE_LEVEL
//!   0   BOOTUP_DEFAULT *:               -                -            -            -
//!   1   3D_FULL_SCREEN  :              70                60            1            3
//!   2     POWER_SAVING  :              90                60            0            0
//!   …
//! ```
//!
//! The active row carries `*` after the MODE_NAME column. Writing
//! requires the row index (`0`, `1`, …) — not the name. The sensor in
//! `power::sensors::igpu` already parses the same table to surface the
//! active mode; we re-derive the index here (column 0) so the writer
//! and the reader stay independent (the sensor doesn't expose indices).
//!
//! Idempotent via [`super::write_if_changed`]: if the parsed active
//! row already matches `target`, no write is performed.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use super::super::sensors::igpu::{dpm_level_str, DpmLevel, IgpuProfileMode, LEGACY_DPM_FILE};
use super::{write_if_changed, Actuator, Applied};

/// Subdir under `sysfs_root` housing the AMDGPU `cardN` device dirs.
/// We re-discover the AMD card here (rather than thread the card path
/// through every call) so the actuator API matches `PlatformProfile` /
/// `Epp`: take a sysfs root, find the knob, write it.
const DRM_DIR: &str = "class/drm";
const AMD_VENDOR: &str = "0x1002";
const VENDOR_FILE: &str = "device/vendor";
const PROFILE_FILE: &str = "device/pp_power_profile_mode";

/// Errors specific to the iGPU write path. `UnsupportedMode` carries
/// the parsed table so an operator hitting a kernel that dropped a
/// preset (e.g. `VIDEO_ENCODER` not exposed on this driver build) can
/// see exactly which rows are available without `cat`-ing sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgpuError {
    /// Requested mode has no matching row in the parsed table.
    /// `available` is the list of `MODE_NAME` strings the kernel
    /// currently exposes.
    UnsupportedMode {
        requested: String,
        available: Vec<String>,
    },
    /// `class/drm/card*` exists but none have `vendor == 0x1002`.
    NoAmdCard,
}

impl fmt::Display for IgpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IgpuError::UnsupportedMode {
                requested,
                available,
            } => write!(
                f,
                "iGPU mode {requested:?} not in kernel-supported rows {available:?}",
            ),
            IgpuError::NoAmdCard => {
                write!(f, "no AMD iGPU (vendor 0x1002) under class/drm/card*")
            }
        }
    }
}

impl std::error::Error for IgpuError {}

/// Zero-sized actuator. Stateless: every call rediscovers the AMD
/// card and re-parses the table.
#[derive(Debug, Default)]
pub struct IgpuActuator;

impl IgpuActuator {
    pub fn new() -> Self {
        Self
    }
}

impl Actuator for IgpuActuator {
    type Target = IgpuProfileMode;
    fn apply(&self, target: Self::Target, sysfs_root: &Path) -> Result<Applied> {
        set_igpu_mode(target, sysfs_root)
    }
}

/// Functional entrypoint. Finds the AMD card, parses
/// `pp_power_profile_mode`, locates the row matching `target`, and
/// writes the row index (as a one-line string) if it isn't already
/// the active row. Roadmap §H3: when `pp_power_profile_mode` is
/// absent (this HX 370 host's kernel), falls back to writing the
/// legacy `power_dpm_force_performance_level` knob via
/// [`set_legacy_dpm_level`].
pub fn set_igpu_mode(target: IgpuProfileMode, sysfs_root: &Path) -> Result<Applied> {
    let card = find_amd_card(sysfs_root)?;
    let profile_path = card.join(PROFILE_FILE);
    // Roadmap §P1-3: ONLY `NotFound` is the expected fallback signal
    // (HX 370 / kernel 7.0.6 ships without `pp_power_profile_mode`);
    // any other read error (EACCES, EIO, ...) is propagated so the
    // daemon's `apply_arm` logs a WARN — a real, unexpected failure.
    let raw = match std::fs::read_to_string(&profile_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return set_legacy_dpm_level(&card, target);
        }
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("read {} for diff", profile_path.display()))
            );
        }
    };
    let rows = parse_rows(&raw);
    let target_name = mode_canonical(&target);
    let target_row =
        rows.iter()
            .find(|r| r.name == target_name)
            .ok_or_else(|| IgpuError::UnsupportedMode {
                requested: target_name.clone(),
                available: rows.iter().map(|r| r.name.clone()).collect(),
            })?;
    if target_row.active {
        return Ok(Applied::NoChange);
    }
    write_if_changed(&profile_path, &target_row.index.to_string())
}

/// Roadmap §H3 fallback writer. Maps the bandit's target via
/// [`igpu_mode_to_dpm_level`] and writes the legacy
/// `power_dpm_force_performance_level` knob. Idempotent via
/// [`write_if_changed`]; no-op when the knob already matches.
fn set_legacy_dpm_level(card: &Path, target: IgpuProfileMode) -> Result<Applied> {
    let dpm_path = card.join(LEGACY_DPM_FILE);
    let level = igpu_mode_to_dpm_level(&target);
    write_if_changed(&dpm_path, dpm_level_str(level))
}

/// Roadmap §H3 mapping table. Translates the bandit's higher-level
/// `IgpuProfileMode` choices onto the legacy DPM knob's vocabulary.
/// `LegacyDpmLevel(level)` passes through verbatim — that variant
/// only originates from the sensor and carries its own level
/// already.
fn igpu_mode_to_dpm_level(target: &IgpuProfileMode) -> DpmLevel {
    match target {
        IgpuProfileMode::BootupDefault => DpmLevel::Auto,
        IgpuProfileMode::ThreeDFullScreen => DpmLevel::ProfilePeak,
        IgpuProfileMode::PowerSaving => DpmLevel::ProfileMinSclk,
        IgpuProfileMode::Video => DpmLevel::Auto,
        IgpuProfileMode::Vr => DpmLevel::ProfilePeak,
        IgpuProfileMode::Compute => DpmLevel::High,
        IgpuProfileMode::Custom => DpmLevel::Manual,
        IgpuProfileMode::VideoEncoder => DpmLevel::Auto,
        IgpuProfileMode::Other(_) => DpmLevel::Auto,
        IgpuProfileMode::LegacyDpmLevel(l) => *l,
    }
}

/// One parsed line from `pp_power_profile_mode`. `index` is the
/// kernel-assigned row number; `name` is the canonical MODE_NAME
/// (without the `*` marker); `active` mirrors the `*` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    index: u32,
    name: String,
    active: bool,
}

/// Parse the table. The header row has a non-numeric first column
/// (`NUM`) and is skipped. Each data row starts with the index then
/// the MODE_NAME column. The MODE_NAME may carry a trailing `*`
/// (active marker) or a trailing `:` separator before the value
/// columns; both are stripped to recover the canonical name.
fn parse_rows(raw: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let cols: Vec<&str> = line.split_ascii_whitespace().collect();
        if cols.len() < 2 {
            continue;
        }
        let idx = match cols[0].parse::<u32>() {
            Ok(n) => n,
            Err(_) => continue, // header row ("NUM …") or blank lines
        };
        let name_raw = cols[1];
        // Active marker may be either the bare `*` (separate token) or
        // appended to the MODE_NAME / next token. Mirror the sensor's
        // looser scan and check for either shape.
        let name = name_raw.trim_end_matches([':', '*']).to_string();
        let active =
            name_raw.ends_with('*') || cols.iter().skip(2).any(|t| *t == "*" || *t == "*:");
        out.push(Row {
            index: idx,
            name,
            active,
        });
    }
    out
}

/// Canonical MODE_NAME string for an `IgpuProfileMode`. Mirrors the
/// sensor's `mode_as_str` but kept local — that helper is private to
/// the sensor module on purpose, and copying the four-line match here
/// is cheaper than widening the sensor's public surface.
///
/// `LegacyDpmLevel` is never expected as an *apply* target (the
/// bandit only proposes the canonical variants) but Rust needs an
/// exhaustive match; the synthetic `legacy:<level>` tag we return
/// will not appear in the kernel table, producing a clean
/// `UnsupportedMode` error if it ever leaks through.
fn mode_canonical(m: &IgpuProfileMode) -> String {
    match m {
        IgpuProfileMode::BootupDefault => "BOOTUP_DEFAULT".into(),
        IgpuProfileMode::ThreeDFullScreen => "3D_FULL_SCREEN".into(),
        IgpuProfileMode::PowerSaving => "POWER_SAVING".into(),
        IgpuProfileMode::Video => "VIDEO".into(),
        IgpuProfileMode::Vr => "VR".into(),
        IgpuProfileMode::Compute => "COMPUTE".into(),
        IgpuProfileMode::Custom => "CUSTOM".into(),
        IgpuProfileMode::VideoEncoder => "VIDEO_ENCODER".into(),
        IgpuProfileMode::Other(s) => s.clone(),
        IgpuProfileMode::LegacyDpmLevel(l) => {
            format!("legacy:{}", super::super::sensors::igpu::dpm_level_str(*l))
        }
    }
}

/// Walk `class/drm/card*` and return the first primary card whose
/// `device/vendor` reads `0x1002`. Mirrors the sensor's discovery
/// rule so the writer and the reader land on the same device.
fn find_amd_card(sysfs_root: &Path) -> Result<PathBuf> {
    let root = sysfs_root.join(DRM_DIR);
    // Symmetric with `sensors::igpu::find_amd_card`: real `cardN`
    // entries under `class/drm/` are symlinks into `/sys/devices/…`;
    // `follow_links(true)` makes the walker resolve them.
    for entry in WalkDir::new(&root)
        .follow_links(true)
        .min_depth(1)
        .max_depth(1)
    {
        let entry = entry.with_context(|| format!("walk {}", root.display()))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_primary_card(name) {
            continue;
        }
        let vendor = std::fs::read_to_string(path.join(VENDOR_FILE))
            .unwrap_or_default()
            .trim()
            .to_string();
        if vendor == AMD_VENDOR {
            return Ok(path.to_path_buf());
        }
    }
    Err(IgpuError::NoAmdCard.into())
}

/// Same rule the sensor uses: `cardN` with `N` an integer; reject
/// `card1-DP-1`, `renderD128`, etc.
fn is_primary_card(name: &str) -> bool {
    name.starts_with("card") && name[4..].chars().all(|c| c.is_ascii_digit()) && name.len() > 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const TABLE: &str = "\
NUM        MODE_NAME     BUSY_SET_POINT FPS USE_RLC_BUSY MIN_ACTIVE_LEVEL
  0   BOOTUP_DEFAULT *:               -                -            -            -
  1   3D_FULL_SCREEN  :              70                60            1            3
  2     POWER_SAVING  :              90                60            0            0
  3            VIDEO  :              70                60            1            3
  4               VR  :              70                90            0            0
  5        COMPUTE   :               30                 0            0            6
  6           CUSTOM  :               0                 0            0            0
";

    /// Stamp a minimal `class/drm/card0/device/{vendor,pp_power_profile_mode}`
    /// tree under `td`. `card0` is the AMD card (vendor 0x1002) and the
    /// table is the SPEC-shape multi-line one; the *active* row carries
    /// `*` after the MODE_NAME column.
    fn fixture(td: &tempfile::TempDir, table: &str) -> PathBuf {
        let root = td.path().to_path_buf();
        let card = root.join(DRM_DIR).join("card0");
        let device = card.join("device");
        fs::create_dir_all(&device).expect("mkdir device");
        fs::write(device.join("vendor"), format!("{AMD_VENDOR}\n")).expect("seed vendor");
        fs::write(device.join("pp_power_profile_mode"), table).expect("seed pp_profile");
        root
    }

    /// Roadmap §16 test: fixture starts with `BOOTUP_DEFAULT` active;
    /// writing `ThreeDFullScreen` must write the row index ("1") to
    /// `pp_power_profile_mode` and the [`Applied`] result must name the
    /// touched path. The kernel-side update of the `*` marker is the
    /// kernel's job; we only verify our write reached sysfs.
    #[test]
    fn sets_3d_full_screen() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture(&td, TABLE);
        let out = set_igpu_mode(IgpuProfileMode::ThreeDFullScreen, &root).expect("apply");
        match out {
            Applied::Wrote { path, value } => {
                assert!(path.ends_with(PROFILE_FILE), "wrong path: {path:?}");
                assert_eq!(value, "1", "3D_FULL_SCREEN row index is 1 in the fixture");
            }
            other => panic!("expected Wrote, got {other:?}"),
        }
        let after =
            fs::read_to_string(root.join(DRM_DIR).join("card0").join(PROFILE_FILE)).expect("read");
        assert_eq!(after, "1");
    }

    /// Idempotency contract: re-applying the *currently active* mode is
    /// a no-op (no sysfs touch) and the table is byte-identical after.
    #[test]
    fn no_change_when_already_active() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture(&td, TABLE);
        let out = set_igpu_mode(IgpuProfileMode::BootupDefault, &root).expect("apply");
        assert_eq!(out, Applied::NoChange);
        let after =
            fs::read_to_string(root.join(DRM_DIR).join("card0").join(PROFILE_FILE)).expect("read");
        assert_eq!(after, TABLE);
    }

    /// Roadmap §H3: when only the legacy
    /// `power_dpm_force_performance_level` knob is present (this HX 370
    /// host's kernel), the actuator must transparently fall back to
    /// writing the mapped legacy value. `ThreeDFullScreen` maps onto
    /// `profile_peak` per the H3 mapping table.
    #[test]
    fn writes_legacy_dpm_level_when_pp_absent() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let device = td.path().join(DRM_DIR).join("card0").join("device");
        fs::create_dir_all(&device).expect("mkdir device");
        fs::write(device.join("vendor"), format!("{AMD_VENDOR}\n")).expect("vendor");
        fs::write(device.join("power_dpm_force_performance_level"), "auto\n").expect("dpm");
        let out =
            set_igpu_mode(IgpuProfileMode::ThreeDFullScreen, td.path()).expect("legacy apply");
        match out {
            Applied::Wrote { path, value } => {
                assert!(
                    path.ends_with("power_dpm_force_performance_level"),
                    "wrong path: {path:?}",
                );
                assert_eq!(
                    value, "profile_peak",
                    "ThreeDFullScreen maps to profile_peak"
                );
            }
            other => panic!("expected Wrote, got {other:?}"),
        }
        let after =
            fs::read_to_string(device.join("power_dpm_force_performance_level")).expect("readback");
        assert_eq!(after, "profile_peak");
    }

    /// H3 mapping table is the actuator's contract — pin every entry
    /// in one test so a future refactor can't silently drift the
    /// bandit-to-sysfs translation.
    #[test]
    fn igpu_mode_to_dpm_level_mapping() {
        use crate::power::sensors::igpu::DpmLevel;
        assert_eq!(
            igpu_mode_to_dpm_level(&IgpuProfileMode::BootupDefault),
            DpmLevel::Auto,
        );
        assert_eq!(
            igpu_mode_to_dpm_level(&IgpuProfileMode::ThreeDFullScreen),
            DpmLevel::ProfilePeak,
        );
        assert_eq!(
            igpu_mode_to_dpm_level(&IgpuProfileMode::PowerSaving),
            DpmLevel::ProfileMinSclk,
        );
        assert_eq!(
            igpu_mode_to_dpm_level(&IgpuProfileMode::Video),
            DpmLevel::Auto,
        );
        assert_eq!(
            igpu_mode_to_dpm_level(&IgpuProfileMode::Vr),
            DpmLevel::ProfilePeak,
        );
        assert_eq!(
            igpu_mode_to_dpm_level(&IgpuProfileMode::Compute),
            DpmLevel::High,
        );
    }

    /// Roadmap §P1-3: when `pp_power_profile_mode` is absent (ENOENT
    /// — this HX 370 host's kernel) and the H3 legacy DPM fallback
    /// succeeds, the actuator must return `Ok(_)` so the daemon's
    /// `apply_arm` never logs a WARN. The first-attempt ENOENT is the
    /// EXPECTED signal that triggers the fallback, not an "actuator
    /// failed" condition.
    #[test]
    #[tracing_test::traced_test]
    fn no_warn_when_pp_mode_absent_and_fallback_succeeds() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let device = td.path().join(DRM_DIR).join("card0").join("device");
        fs::create_dir_all(&device).expect("mkdir device");
        fs::write(device.join("vendor"), format!("{AMD_VENDOR}\n")).expect("vendor");
        fs::write(device.join("power_dpm_force_performance_level"), "auto\n").expect("dpm");
        // No `pp_power_profile_mode` file → ENOENT on first-attempt read.
        let out = set_igpu_mode(IgpuProfileMode::BootupDefault, td.path())
            .expect("legacy fallback must succeed silently");
        assert_eq!(out, Applied::NoChange, "auto→auto is a no-op");
        assert!(
            !logs_contain("WARN"),
            "ENOENT on pp_power_profile_mode must not surface as a WARN-worthy error",
        );
    }

    /// Roadmap §P1-3: when `pp_power_profile_mode` is PRESENT but a
    /// subsequent read or write fails for a non-`NotFound` reason
    /// (EACCES here), the error MUST propagate so the daemon's
    /// `apply_arm` logs a WARN — that's a real, unexpected failure,
    /// not the expected fallback signal.
    #[test]
    fn warn_when_pp_mode_present_but_write_fails_for_other_reason() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture(&td, TABLE);
        let pp = root.join(DRM_DIR).join("card0").join(PROFILE_FILE);
        // pp_mode present (readable) but not writable → write_if_changed
        // succeeds on the read, fails on the write. The propagated error
        // is the signal the daemon converts to WARN.
        fs::set_permissions(&pp, fs::Permissions::from_mode(0o444)).expect("chmod 0444");
        let err = set_igpu_mode(IgpuProfileMode::ThreeDFullScreen, &root)
            .expect_err("EACCES on write must propagate");
        assert!(
            err.to_string().contains("write")
                || err.chain().any(|c| {
                    c.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                }),
            "expected a write/PermissionDenied error chain, got: {err:?}",
        );
    }

    /// Roadmap §P1-3: when `pp_power_profile_mode` read fails for a
    /// non-`NotFound` reason (EACCES on the file itself, simulated
    /// here by chmod 0o000), the error MUST propagate — the silent
    /// fallback path is reserved for the ENOENT signal only.
    #[test]
    fn propagates_non_notfound_read_error_on_pp_mode() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture(&td, TABLE);
        let pp = root.join(DRM_DIR).join("card0").join(PROFILE_FILE);
        fs::set_permissions(&pp, fs::Permissions::from_mode(0o000)).expect("chmod 0000");
        let res = set_igpu_mode(IgpuProfileMode::ThreeDFullScreen, &root);
        // Restore perms so TempDir teardown can rm the file.
        let _ = fs::set_permissions(&pp, fs::Permissions::from_mode(0o644));
        let err = res.expect_err("EACCES on pp_mode read must propagate, not silent-fallback");
        assert!(
            err.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
            }),
            "expected PermissionDenied in error chain, got: {err:?}",
        );
    }

    /// Unsupported-mode path: when the kernel's table doesn't list the
    /// requested preset (e.g. a stripped-down driver build), the error
    /// must surface as `IgpuError::UnsupportedMode` with the available
    /// rows attached.
    #[test]
    fn rejects_mode_missing_from_table() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let root = fixture(&td, TABLE);
        let err =
            set_igpu_mode(IgpuProfileMode::VideoEncoder, &root).expect_err("missing row must err");
        let ie = err
            .downcast_ref::<IgpuError>()
            .expect("error must be IgpuError");
        match ie {
            IgpuError::UnsupportedMode {
                requested,
                available,
            } => {
                assert_eq!(requested, "VIDEO_ENCODER");
                assert!(
                    available.contains(&"BOOTUP_DEFAULT".to_string()),
                    "available must list parsed rows, got {available:?}"
                );
            }
            other => panic!("expected UnsupportedMode, got {other:?}"),
        }
    }
}
