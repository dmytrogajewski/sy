//! AMD iGPU reader: `gpu_busy_percent` + `pp_power_profile_mode`.
//!
//! Discovers the integrated AMD card by walking `class/drm/card*` and
//! filtering on `device/vendor == 0x1002` (AMD). Discrete GPUs from
//! other vendors (`0x10de` NVIDIA, `0x8086` Intel) are skipped — only
//! the integrated AMD GPU is governed by `sy power` (Step 16's
//! `apply::igpu` writes `pp_power_profile_mode`).
//!
//! `gpu_busy_percent` is the percentage of the last sampling interval
//! the GPU spent doing work (0-100). `pp_power_profile_mode` is a
//! multi-line table listing preset modes; the active preset is the
//! row marked with a trailing `*`. Step 8's snapshot assembler reads
//! `IgpuReading::busy_pct` directly into the 12-channel feature vec.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use walkdir::WalkDir;

use super::{Sensor, SensorReading};

const DRM_DIR: &str = "class/drm";
const AMD_VENDOR: &str = "0x1002";
const VENDOR_FILE: &str = "device/vendor";
const BUSY_FILE: &str = "device/gpu_busy_percent";
const PROFILE_FILE: &str = "device/pp_power_profile_mode";
/// Legacy AMDGPU knob present on kernels that don't expose
/// `pp_power_profile_mode` (this HX 370 / kernel 7.0.6 host).
/// Roadmap §H3 fallback.
pub(crate) const LEGACY_DPM_FILE: &str = "device/power_dpm_force_performance_level";

/// AMDGPU `power_dpm_force_performance_level` enum — the legacy
/// counterpart to `pp_power_profile_mode`. `snake_case` matches the
/// raw sysfs strings (`auto`, `profile_peak`, …) so the round-trip
/// through serde is lossless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpmLevel {
    Auto,
    Low,
    High,
    Manual,
    ProfileStandard,
    ProfileMinSclk,
    ProfileMinMclk,
    ProfilePeak,
}

/// SPEC §4 / arm table: the preset enum the bandit's `igpu_mode`
/// dimension picks from. `Other` carries vendor-specific rows so the
/// arm enumerator (Step 14) can drop unknowns without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgpuProfileMode {
    BootupDefault,
    ThreeDFullScreen,
    PowerSaving,
    Video,
    Vr,
    Compute,
    Custom,
    VideoEncoder,
    Other(String),
    /// Roadmap §H3: surfaced when the iGPU only exposes the legacy
    /// `power_dpm_force_performance_level` knob (no
    /// `pp_power_profile_mode`). Carries the kernel-reported level so
    /// the bandit's context still has a populated `active_profile`.
    LegacyDpmLevel(DpmLevel),
}

#[derive(Debug, Clone, PartialEq)]
pub struct IgpuReading {
    /// `gpu_busy_percent` 0-100. None when the integrated AMD card
    /// has no `gpu_busy_percent` node (older kernels).
    pub busy_pct: Option<u8>,
    /// Active row in `pp_power_profile_mode`. None when the file is
    /// absent (some APU kernel builds skip it).
    pub active_profile: Option<IgpuProfileMode>,
}

#[derive(Debug, Default)]
pub struct IgpuSensor;

impl IgpuSensor {
    pub fn new() -> Self {
        Self
    }
}

impl Sensor for IgpuSensor {
    fn read(&self, sysfs_root: &Path) -> Result<SensorReading> {
        let card = find_amd_card(sysfs_root)?;
        let busy_pct = read_busy_pct(&card.join(BUSY_FILE)).ok();
        let active_profile = read_active_profile(&card.join(PROFILE_FILE))
            .ok()
            .or_else(|| read_legacy_dpm_level(&card.join(LEGACY_DPM_FILE)).ok());
        Ok(SensorReading::Igpu(IgpuReading {
            busy_pct,
            active_profile,
        }))
    }
}

/// Walk `class/drm/card*` and return the path of the first card whose
/// `device/vendor` reads `0x1002`. Discrete GPUs from other vendors
/// are skipped — `sy power` only governs the integrated AMD iGPU.
fn find_amd_card(sysfs_root: &Path) -> Result<PathBuf> {
    let root = sysfs_root.join(DRM_DIR);
    // `class/drm/cardN` entries are symlinks into `/sys/devices/…`;
    // `follow_links(true)` makes the walker resolve them so any
    // future `is_dir()` filter keeps reporting the target's type.
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
        // Match `card0`, `card1`, …; skip `card1-DP-1`, `renderD128`, etc.
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
    anyhow::bail!(
        "no AMD iGPU found under {} (vendor {AMD_VENDOR})",
        root.display()
    )
}

/// `cardN` with `N` an integer; reject `card1-DP-1`, `card2-eDP-1`, etc.
fn is_primary_card(name: &str) -> bool {
    name.starts_with("card") && name[4..].chars().all(|c| c.is_ascii_digit()) && name.len() > 4
}

fn read_busy_pct(path: &Path) -> Result<u8> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .trim()
        .to_string();
    raw.parse::<u8>()
        .with_context(|| format!("parse u8 at {}: {raw:?}", path.display()))
}

fn read_active_profile(path: &Path) -> Result<IgpuProfileMode> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_active_profile(&raw)
        .ok_or_else(|| anyhow::anyhow!("no active profile (`*`-marked row) in {}", path.display()))
}

/// Roadmap §H3 fallback: parse `power_dpm_force_performance_level`
/// into `IgpuProfileMode::LegacyDpmLevel(DpmLevel)`. The file is a
/// single line containing one of the [`DpmLevel`] strings.
fn read_legacy_dpm_level(path: &Path) -> Result<IgpuProfileMode> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let level = parse_dpm_level(raw.trim())
        .ok_or_else(|| anyhow::anyhow!("unknown DPM level {:?} at {}", raw, path.display()))?;
    Ok(IgpuProfileMode::LegacyDpmLevel(level))
}

/// Parse a `power_dpm_force_performance_level` value. Returns `None`
/// for unrecognised strings so the caller can surface a typed error
/// instead of silently masquerading as a known level.
pub(crate) fn parse_dpm_level(raw: &str) -> Option<DpmLevel> {
    Some(match raw.trim() {
        "auto" => DpmLevel::Auto,
        "low" => DpmLevel::Low,
        "high" => DpmLevel::High,
        "manual" => DpmLevel::Manual,
        "profile_standard" => DpmLevel::ProfileStandard,
        "profile_min_sclk" => DpmLevel::ProfileMinSclk,
        "profile_min_mclk" => DpmLevel::ProfileMinMclk,
        "profile_peak" => DpmLevel::ProfilePeak,
        _ => return None,
    })
}

/// Canonical sysfs string for a [`DpmLevel`]. Inverse of
/// [`parse_dpm_level`]; used both by the sensor's serde path and by
/// the actuator's legacy-knob writer.
pub(crate) fn dpm_level_str(level: DpmLevel) -> &'static str {
    match level {
        DpmLevel::Auto => "auto",
        DpmLevel::Low => "low",
        DpmLevel::High => "high",
        DpmLevel::Manual => "manual",
        DpmLevel::ProfileStandard => "profile_standard",
        DpmLevel::ProfileMinSclk => "profile_min_sclk",
        DpmLevel::ProfileMinMclk => "profile_min_mclk",
        DpmLevel::ProfilePeak => "profile_peak",
    }
}

/// Find the `pp_power_profile_mode` row whose name field ends with `*`
/// (the kernel-tagged active preset) and return it as `IgpuProfileMode`.
fn parse_active_profile(raw: &str) -> Option<IgpuProfileMode> {
    for line in raw.lines() {
        // Active row format from the AMDGPU driver: indented columns
        // joined by whitespace, with a trailing `*` on the active
        // mode name. The leading `NUM` header row never carries `*`.
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // The active marker is the bare `*` token that immediately
        // follows the MODE_NAME column. We scan column tokens and
        // look for one that is exactly `*`; when found, the *previous*
        // token (or its prefix before any colon) is the mode name.
        let cols: Vec<&str> = trimmed.split_ascii_whitespace().collect();
        if let Some(idx) = cols.iter().position(|t| *t == "*" || *t == "*:") {
            if idx == 0 {
                continue;
            }
            return Some(parse_mode_name(cols[idx - 1]));
        }
    }
    None
}

fn parse_mode_name(raw: &str) -> IgpuProfileMode {
    match raw.trim_end_matches(':') {
        "BOOTUP_DEFAULT" => IgpuProfileMode::BootupDefault,
        "3D_FULL_SCREEN" => IgpuProfileMode::ThreeDFullScreen,
        "POWER_SAVING" => IgpuProfileMode::PowerSaving,
        "VIDEO" => IgpuProfileMode::Video,
        "VR" => IgpuProfileMode::Vr,
        "COMPUTE" => IgpuProfileMode::Compute,
        "CUSTOM" => IgpuProfileMode::Custom,
        "VIDEO_ENCODER" => IgpuProfileMode::VideoEncoder,
        other => IgpuProfileMode::Other(other.to_string()),
    }
}

/// Canonical wire string for an `IgpuProfileMode`. Mirrors the
/// `pp_power_profile_mode` row names AMDGPU exposes; the arm-config
/// deserializer (below) rejects anything that maps onto `Other` or
/// `LegacyDpmLevel`. Returns an owned `String` because the H3
/// fallback variant must format a runtime-built `legacy:<level>` tag
/// (no `'static` slice available for it).
fn mode_as_str(m: &IgpuProfileMode) -> String {
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
        IgpuProfileMode::LegacyDpmLevel(l) => format!("legacy:{}", dpm_level_str(*l)),
    }
}

impl Serialize for IgpuProfileMode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&mode_as_str(self))
    }
}

impl<'de> Deserialize<'de> for IgpuProfileMode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        // Round-trip the H3 fallback variant: snapshots written by a
        // legacy-DPM host must deserialise back losslessly even though
        // arm-config TOML never names this variant directly.
        if let Some(rest) = raw.strip_prefix("legacy:") {
            return parse_dpm_level(rest)
                .map(IgpuProfileMode::LegacyDpmLevel)
                .ok_or_else(|| serde::de::Error::custom(format!("unknown DPM level {rest:?}")));
        }
        match parse_mode_name(&raw) {
            IgpuProfileMode::Other(_) => Err(serde::de::Error::custom(format!(
                "unknown igpu_mode {raw:?} (expected one of: BOOTUP_DEFAULT, 3D_FULL_SCREEN, POWER_SAVING, VIDEO, VR, COMPUTE, CUSTOM, VIDEO_ENCODER)",
            ))),
            v => Ok(v),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/power/fixtures/sys")
            .join(name)
    }

    /// Roadmap §3 test: the HX 370 fixture has `gpu_busy_percent=0`
    /// (idle iGPU during snapshot capture). Reading the AMD card must
    /// surface that as `Some(0)` rather than swallow it as None.
    #[test]
    fn parses_busy_percent_zero_when_idle() {
        let r = IgpuSensor::new()
            .read(&fixture("hx370"))
            .expect("igpu read");
        let g = match r {
            SensorReading::Igpu(g) => g,
            other => panic!("expected Igpu reading, got {other:?}"),
        };
        assert_eq!(g.busy_pct, Some(0), "fixture iGPU is idle (busy=0)");
        assert_eq!(
            g.active_profile,
            Some(IgpuProfileMode::BootupDefault),
            "fixture has `*` on BOOTUP_DEFAULT row",
        );
    }

    /// Skips the discrete-NVIDIA `card1` (vendor 0x10de) fixture entry
    /// and lands on `card2` (AMD, vendor 0x1002). Without this, the
    /// daemon would govern the wrong device on dual-GPU laptops.
    #[test]
    fn skips_non_amd_card() {
        let card = find_amd_card(&fixture("hx370")).expect("find amd");
        assert!(
            card.ends_with("card2"),
            "expected card2 (AMD), got {}",
            card.display()
        );
    }

    /// Step 14 arm config maps `igpu_mode = "POWER_SAVING"` onto the
    /// sensor enum; deserialise must round-trip the canonical kernel
    /// names and reject anything that would fall through to `Other`.
    #[test]
    fn deserializes_canonical_kernel_names() {
        let p: IgpuProfileMode = serde_json::from_str("\"POWER_SAVING\"").expect("ps");
        let b: IgpuProfileMode = serde_json::from_str("\"BOOTUP_DEFAULT\"").expect("bd");
        let f: IgpuProfileMode = serde_json::from_str("\"3D_FULL_SCREEN\"").expect("3d");
        assert_eq!(p, IgpuProfileMode::PowerSaving);
        assert_eq!(b, IgpuProfileMode::BootupDefault);
        assert_eq!(f, IgpuProfileMode::ThreeDFullScreen);
    }

    #[test]
    fn deserialize_rejects_unknown_igpu_mode() {
        let err = serde_json::from_str::<IgpuProfileMode>("\"WARP_DRIVE\"")
            .expect_err("unknown mode must error");
        assert!(err.to_string().contains("WARP_DRIVE"), "{err}");
    }

    /// Step H2: real `/sys/class/drm/cardN` entries are symlinks into
    /// `/sys/devices/pci…`. Walker must opt in to `follow_links(true)`
    /// so the AMD card is discoverable via the canonical class path.
    #[test]
    fn follows_symlinks_in_sysfs_class_drm() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let class_dir = temp.path().join("class/drm");
        let device_dir = temp.path().join("devices/pci0/drm/card2");
        std::fs::create_dir_all(device_dir.join("device")).expect("mkdir device");
        std::fs::create_dir_all(&class_dir).expect("mkdir class");
        std::fs::write(device_dir.join("device/vendor"), "0x1002\n").expect("vendor");
        std::fs::write(device_dir.join("device/gpu_busy_percent"), "0\n").expect("busy");
        std::os::unix::fs::symlink(&device_dir, class_dir.join("card2")).expect("symlink");
        let r = IgpuSensor::new().read(temp.path()).expect("read");
        let g = match r {
            SensorReading::Igpu(g) => g,
            other => panic!("expected Igpu, got {other:?}"),
        };
        assert_eq!(g.busy_pct, Some(0));
    }

    #[test]
    fn rejects_non_primary_card_names() {
        assert!(is_primary_card("card2"));
        assert!(is_primary_card("card12"));
        assert!(!is_primary_card("card1-DP-1"));
        assert!(!is_primary_card("card"));
        assert!(!is_primary_card("renderD128"));
    }

    /// Roadmap §H3: on kernels that don't expose
    /// `pp_power_profile_mode` (this HX 370 host), the sensor must
    /// fall back to `power_dpm_force_performance_level` and surface
    /// the value via the `LegacyDpmLevel` variant — so the bandit
    /// still gets a populated `active_profile` channel.
    #[test]
    fn reads_power_dpm_force_performance_level_when_pp_absent() {
        let td = tempfile::TempDir::new().expect("tempdir");
        let device = td.path().join("class/drm/card0/device");
        std::fs::create_dir_all(&device).expect("mkdir device");
        std::fs::write(device.join("vendor"), format!("{AMD_VENDOR}\n")).expect("vendor");
        std::fs::write(device.join("gpu_busy_percent"), "0\n").expect("busy");
        std::fs::write(
            device.join("power_dpm_force_performance_level"),
            "profile_peak\n",
        )
        .expect("dpm");
        let r = IgpuSensor::new().read(td.path()).expect("igpu read");
        let g = match r {
            SensorReading::Igpu(g) => g,
            other => panic!("expected Igpu reading, got {other:?}"),
        };
        assert_eq!(
            g.active_profile,
            Some(IgpuProfileMode::LegacyDpmLevel(DpmLevel::ProfilePeak)),
            "fallback must surface the legacy DPM level",
        );
    }
}
