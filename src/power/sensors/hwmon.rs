//! `hwmon` reader: k10temp `Tctl` (package temp) + amdgpu `edge` +
//! `power1_average`.
//!
//! The hwmon nodes are not at a fixed path — they're numbered in
//! discovery order, so the same kernel boot may put k10temp at
//! `hwmon5` one day and `hwmon3` the next. We walk `class/hwmon/`
//! once per `read`, match on `name`, and pick out the relevant temp
//! / power channels.

use std::path::Path;

use anyhow::{Context, Result};
use walkdir::WalkDir;

use super::{Sensor, SensorReading};

const HWMON_DIR: &str = "class/hwmon";
const K10TEMP_NAME: &str = "k10temp";
const AMDGPU_NAME: &str = "amdgpu";

/// hwmon reports temperature in millidegrees Celsius and power
/// (`power1_average`) in microwatts. Step 8's snapshot assembler is
/// responsible for converting μW → W when populating the feature vec.
const MILLIDEG_PER_DEG: f32 = 1000.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HwmonReading {
    /// k10temp Tctl in °C. Required — absence is a parse error.
    pub tctl_c: f32,
    /// amdgpu `edge` in °C. Optional: integrated GPUs may not expose
    /// it on every kernel.
    pub edge_c: Option<f32>,
    /// amdgpu `power1_average` in microwatts. Optional for the same
    /// reason as `edge_c`.
    pub package_power_uw: Option<u32>,
}

#[derive(Debug, Default)]
pub struct HwmonSensor;

impl HwmonSensor {
    pub fn new() -> Self {
        Self
    }
}

impl Sensor for HwmonSensor {
    fn read(&self, sysfs_root: &Path) -> Result<SensorReading> {
        let root = sysfs_root.join(HWMON_DIR);
        let mut k10 = None;
        let mut amdgpu = None;
        // `class/hwmon/*` entries are symlinks into `/sys/devices/…`;
        // `follow_links(true)` makes `walkdir` resolve them so
        // `file_type().is_dir()` reports the target's type, not the
        // symlink's. Without this, every real-host entry is skipped.
        for entry in WalkDir::new(&root)
            .follow_links(true)
            .min_depth(1)
            .max_depth(1)
        {
            let entry = entry.with_context(|| format!("walk {}", root.display()))?;
            if !entry.file_type().is_dir() {
                continue;
            }
            let node = entry.path();
            let name = read_trim(&node.join("name")).unwrap_or_default();
            match name.as_str() {
                K10TEMP_NAME => k10 = Some(node.to_path_buf()),
                AMDGPU_NAME => amdgpu = Some(node.to_path_buf()),
                _ => {}
            }
        }
        let k10 = k10.ok_or_else(|| {
            anyhow::anyhow!("k10temp hwmon node missing under {}", root.display())
        })?;
        let tctl_c = read_milli_deg(&k10.join("temp1_input"))?;
        let (edge_c, package_power_uw) = match amdgpu {
            Some(p) => (
                read_milli_deg(&p.join("temp1_input")).ok(),
                read_u32(&p.join("power1_average")).ok(),
            ),
            None => (None, None),
        };
        Ok(SensorReading::Hwmon(HwmonReading {
            tctl_c,
            edge_c,
            package_power_uw,
        }))
    }
}

fn read_trim(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(s.trim().to_string())
}

fn read_u32(path: &Path) -> Result<u32> {
    let raw = read_trim(path)?;
    raw.parse::<u32>()
        .with_context(|| format!("parse u32 at {}: {raw:?}", path.display()))
}

fn read_milli_deg(path: &Path) -> Result<f32> {
    let raw = read_trim(path)?;
    let n: i32 = raw
        .parse()
        .with_context(|| format!("parse i32 at {}: {raw:?}", path.display()))?;
    Ok(n as f32 / MILLIDEG_PER_DEG)
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

    /// Roadmap §2 test: `tctl_within_plausible_range`. A k10temp read
    /// outside [20, 110] °C is almost certainly a unit-mismatch bug
    /// (deg vs millideg).
    const TCTL_MIN_C: f32 = 20.0;
    const TCTL_MAX_C: f32 = 110.0;

    /// Step H2: real `/sys/class/hwmon/hwmonN` entries are symlinks
    /// into `/sys/devices/...`. `walkdir` does not descend through
    /// symlinks by default, so the walker must opt in via
    /// `follow_links(true)`. Without it, the daemon sees zero hwmon
    /// nodes on the real HX 370 host and `tctl_c` serialises as null.
    #[test]
    fn follows_symlinks_in_sysfs_class_hwmon() {
        const TCTL_MILLIDEG: &str = "82375\n";
        const EXPECTED_C: f32 = 82.375;
        const EPS_C: f32 = 0.01;
        let temp = tempfile::TempDir::new().expect("tempdir");
        let class_dir = temp.path().join("class/hwmon");
        let device_dir = temp.path().join("devices/pci0/hwmon/hwmon5");
        std::fs::create_dir_all(&device_dir).expect("mkdir device");
        std::fs::create_dir_all(&class_dir).expect("mkdir class");
        std::fs::write(device_dir.join("name"), "k10temp\n").expect("write name");
        std::fs::write(device_dir.join("temp1_input"), TCTL_MILLIDEG).expect("write temp");
        std::os::unix::fs::symlink(&device_dir, class_dir.join("hwmon5")).expect("symlink");
        let r = HwmonSensor::new().read(temp.path()).expect("read");
        let h = match r {
            SensorReading::Hwmon(h) => h,
            other => panic!("expected Hwmon, got {other:?}"),
        };
        assert!(
            (h.tctl_c - EXPECTED_C).abs() < EPS_C,
            "tctl_c {} expected ~{EXPECTED_C}",
            h.tctl_c
        );
    }

    #[test]
    fn tctl_within_plausible_range() {
        let r = HwmonSensor::new()
            .read(&fixture("hx370"))
            .expect("hwmon read");
        let h = match r {
            SensorReading::Hwmon(h) => h,
            other => panic!("expected Hwmon reading, got {other:?}"),
        };
        assert!(
            (TCTL_MIN_C..=TCTL_MAX_C).contains(&h.tctl_c),
            "Tctl {} out of plausible range [{TCTL_MIN_C}, {TCTL_MAX_C}]",
            h.tctl_c,
        );
        // amdgpu fixture has both edge and power1_average populated.
        assert!(h.edge_c.is_some(), "amdgpu edge expected from fixture");
        assert!(
            h.package_power_uw.is_some(),
            "amdgpu power1_average expected from fixture"
        );
    }
}
