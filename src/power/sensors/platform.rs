//! `platform_profile` ACPI lever.
//!
//! Reads `/sys/firmware/acpi/platform_profile` + the
//! `…_choices` whitespace-separated enum. On the HX 370 the choices
//! are exactly `quiet balanced performance`; other vendors expose a
//! superset (`low-power cool quiet balanced balanced-performance
//! performance`). The sensor reports all choices verbatim — the arm
//! enumerator (Step 14) is responsible for ignoring values that have
//! no matching bandit arm.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{Sensor, SensorReading};

const PROFILE_PATH: &str = "firmware/acpi/platform_profile";
const PROFILE_CHOICES_PATH: &str = "firmware/acpi/platform_profile_choices";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformProfile {
    Quiet,
    Balanced,
    BalancedPerformance,
    Performance,
    LowPower,
    Cool,
    /// Vendor-specific profile we did not pre-enumerate. Carried
    /// verbatim so the arm table can drop it without parsing twice.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformReading {
    pub current: PlatformProfile,
    pub choices: Vec<PlatformProfile>,
}

#[derive(Debug, Default)]
pub struct PlatformSensor;

impl PlatformSensor {
    pub fn new() -> Self {
        Self
    }
}

impl Sensor for PlatformSensor {
    fn read(&self, sysfs_root: &Path) -> Result<SensorReading> {
        let raw_current = read_trim(&sysfs_root.join(PROFILE_PATH))?;
        let raw_choices = read_trim(&sysfs_root.join(PROFILE_CHOICES_PATH))?;
        let current = parse_profile(&raw_current);
        let choices = raw_choices
            .split_ascii_whitespace()
            .map(parse_profile)
            .collect();
        Ok(SensorReading::Platform(PlatformReading {
            current,
            choices,
        }))
    }
}

fn read_trim(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(s.trim().to_string())
}

fn parse_profile(raw: &str) -> PlatformProfile {
    match raw {
        "quiet" => PlatformProfile::Quiet,
        "balanced" => PlatformProfile::Balanced,
        "balanced-performance" | "balanced_performance" => PlatformProfile::BalancedPerformance,
        "performance" => PlatformProfile::Performance,
        "low-power" | "low_power" => PlatformProfile::LowPower,
        "cool" => PlatformProfile::Cool,
        other => PlatformProfile::Other(other.to_string()),
    }
}

/// Canonical string rendering. `Other(s)` round-trips through `s` so a
/// vendor-specific sensor reading can be serialised verbatim, while
/// the deserializer (below) rejects unknown strings outright — the
/// arm config must enumerate known values only.
fn profile_as_str(p: &PlatformProfile) -> &str {
    match p {
        PlatformProfile::Quiet => "quiet",
        PlatformProfile::Balanced => "balanced",
        PlatformProfile::BalancedPerformance => "balanced-performance",
        PlatformProfile::Performance => "performance",
        PlatformProfile::LowPower => "low-power",
        PlatformProfile::Cool => "cool",
        PlatformProfile::Other(s) => s.as_str(),
    }
}

impl Serialize for PlatformProfile {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(profile_as_str(self))
    }
}

impl<'de> Deserialize<'de> for PlatformProfile {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        match parse_profile(&raw) {
            PlatformProfile::Other(_) => Err(serde::de::Error::custom(format!(
                "unknown platform_profile {raw:?} (expected one of: quiet, balanced, balanced-performance, performance, low-power, cool)",
            ))),
            v => Ok(v),
        }
    }
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

    /// Step 14 needs `[[arms]] platform_profile = "quiet"` to map onto
    /// the sensor enum. The deserializer mirrors `parse_profile` for
    /// the canonical SPEC §4 trio and rejects the `Other` fallback so
    /// a typo in `power.toml` fails loud at config-load.
    #[test]
    fn deserializes_canonical_strings() {
        let q: PlatformProfile = serde_json::from_str("\"quiet\"").expect("quiet");
        let b: PlatformProfile = serde_json::from_str("\"balanced\"").expect("balanced");
        let p: PlatformProfile = serde_json::from_str("\"performance\"").expect("performance");
        assert_eq!(q, PlatformProfile::Quiet);
        assert_eq!(b, PlatformProfile::Balanced);
        assert_eq!(p, PlatformProfile::Performance);
    }

    #[test]
    fn deserialize_rejects_unknown_value() {
        let err = serde_json::from_str::<PlatformProfile>("\"ludicrous\"")
            .expect_err("unknown profile must error");
        assert!(
            err.to_string().contains("ludicrous"),
            "error must name the bad value: {err}",
        );
    }

    #[test]
    fn parses_choices_quiet_balanced_performance() {
        let r = PlatformSensor::new()
            .read(&fixture("hx370"))
            .expect("platform read");
        let p = match r {
            SensorReading::Platform(p) => p,
            other => panic!("expected Platform reading, got {other:?}"),
        };
        assert_eq!(p.current, PlatformProfile::Balanced);
        assert_eq!(
            p.choices,
            vec![
                PlatformProfile::Quiet,
                PlatformProfile::Balanced,
                PlatformProfile::Performance,
            ],
        );
    }
}
