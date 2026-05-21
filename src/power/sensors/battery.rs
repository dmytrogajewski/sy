//! Battery + AC reader: SOC %, AC online bool, instantaneous drain
//! rate in W.
//!
//! Walks `class/power_supply/` once per read:
//! - the first directory whose name starts with `BAT` is the
//!   primary battery (multi-battery laptops report on the first
//!   slot; SPEC §4's `battery_pct` feature is single-valued).
//! - any directory whose `type` is `Mains` (or, on kernels that
//!   don't set `type`, names starting with `AC`) reports `online`.
//!
//! Drain rate: prefer `power_now` (µW, signed on charge / discharge).
//! Falls back to `current_now × voltage_now` (µA × µV → pW → W) when
//! `power_now` is absent — the HX 370 reports the latter under
//! `BAT1/{current_now, voltage_now}`. While AC is online, drain is
//! forced to 0 W — the BAT discharging-current convention sometimes
//! reports a small negative trickle even when plugged.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use super::{Sensor, SensorReading};

const POWER_SUPPLY_DIR: &str = "class/power_supply";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatteryReading {
    /// State of charge, 0-100. None if no `BAT*` directory exists
    /// (desktop / dock-only configurations).
    pub soc_pct: Option<u8>,
    /// True if any `Mains`/`AC*` supply reports `online=1`.
    pub ac_online: bool,
    /// Drain rate in watts. Always 0 when `ac_online=true`; otherwise
    /// the absolute value of `power_now` (or the computed
    /// current×voltage product) in W.
    pub drain_w: f32,
}

#[derive(Debug, Default)]
pub struct BatterySensor;

impl BatterySensor {
    pub fn new() -> Self {
        Self
    }
}

impl Sensor for BatterySensor {
    fn read(&self, sysfs_root: &Path) -> Result<SensorReading> {
        let root = sysfs_root.join(POWER_SUPPLY_DIR);
        let (battery_dir, ac_online) = scan_supplies(&root)?;
        let soc_pct = battery_dir
            .as_ref()
            .and_then(|p| read_u8(&p.join("capacity")).ok());
        let raw_drain_w = match &battery_dir {
            Some(p) => read_drain_w(p).unwrap_or(0.0),
            None => 0.0,
        };
        // SPEC §4 contract: AC=true → drain=0. Kernels sometimes
        // report a residual ±100 mW on the charger; that noise must
        // not bleed into the bandit's reward function.
        let drain_w = if ac_online { 0.0 } else { raw_drain_w.abs() };
        Ok(SensorReading::Battery(BatteryReading {
            soc_pct,
            ac_online,
            drain_w,
        }))
    }
}

/// Walk `class/power_supply/` once, returning (primary BAT dir, AC
/// online?). The walk is single-pass so a directory with both kinds
/// of nodes (rare laptops) is classified consistently.
fn scan_supplies(root: &Path) -> Result<(Option<PathBuf>, bool)> {
    let mut battery: Option<PathBuf> = None;
    let mut ac_online = false;
    // `class/power_supply/{BAT*,AC*}` entries are symlinks into
    // `/sys/devices/…`; `follow_links(true)` makes `is_dir()` report
    // the target's type rather than `false` for the symlink itself.
    for entry in WalkDir::new(root)
        .follow_links(true)
        .min_depth(1)
        .max_depth(1)
    {
        let entry = entry.with_context(|| format!("walk {}", root.display()))?;
        if !entry.file_type().is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.starts_with("BAT") && battery.is_none() {
            battery = Some(path.to_path_buf());
            continue;
        }
        if is_mains_supply(path, name) && read_online(path) {
            ac_online = true;
        }
    }
    Ok((battery, ac_online))
}

/// Treat a supply as the mains charger when either `type` reads
/// `Mains` (canonical) or, on kernels that don't surface the `type`
/// node, the directory name starts with `AC` (covers `AC`, `ACAD`,
/// `ADP*` aliases seen across vendors).
fn is_mains_supply(path: &Path, name: &str) -> bool {
    if let Ok(t) = std::fs::read_to_string(path.join("type")) {
        if t.trim() == "Mains" {
            return true;
        }
    }
    name.starts_with("AC") || name.starts_with("ADP")
}

fn read_online(path: &Path) -> bool {
    std::fs::read_to_string(path.join("online"))
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// Returns the instantaneous drain rate in watts, preferring the
/// canonical `power_now` (µW) and falling back to
/// `current_now × voltage_now` (µA × µV → pW → ÷ 1e12 W).
fn read_drain_w(bat: &Path) -> Result<f32> {
    if let Ok(power_uw) = read_i64(&bat.join("power_now")) {
        return Ok(power_uw as f32 / 1_000_000.0);
    }
    let cur_ua = read_i64(&bat.join("current_now"))?;
    let volt_uv = read_i64(&bat.join("voltage_now"))?;
    // pW = µA · µV; ÷ 1e12 to land in W. Both factors are i64 so
    // the intermediate product stays exact up to ~9.2 × 10^18.
    let pw = (cur_ua as i128) * (volt_uv as i128);
    Ok((pw as f64 / 1e12) as f32)
}

fn read_u8(path: &Path) -> Result<u8> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .trim()
        .to_string();
    raw.parse::<u8>()
        .with_context(|| format!("parse u8 at {}: {raw:?}", path.display()))
}

fn read_i64(path: &Path) -> Result<i64> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?
        .trim()
        .to_string();
    raw.parse::<i64>()
        .with_context(|| format!("parse i64 at {}: {raw:?}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/power/fixtures/sys")
            .join(name)
    }

    /// SPEC §4 contract: when `AC/online=1` the drain rate is 0 W
    /// regardless of any residual `power_now` reading. The hx370
    /// fixture is captured on-charger (AC=1, BAT0/power_now=0).
    #[test]
    fn ac_online_forces_drain_zero() {
        let r = BatterySensor::new()
            .read(&fixture("hx370"))
            .expect("battery read");
        let b = match r {
            SensorReading::Battery(b) => b,
            other => panic!("expected Battery reading, got {other:?}"),
        };
        assert!(b.ac_online, "hx370 fixture is on charger");
        assert_eq!(b.drain_w, 0.0, "AC=true → drain pinned to 0 W");
        assert_eq!(b.soc_pct, Some(100));
    }

    /// Step H2: real `/sys/class/power_supply/{BAT*,AC*}` entries
    /// are symlinks into `/sys/devices/…`. The walker uses
    /// `entry.file_type().is_dir()` which returns `false` for
    /// symlinks — `follow_links(true)` is required so the target
    /// type is reported and the BAT directory is not skipped.
    #[test]
    fn follows_symlinks_in_sysfs_class_power_supply() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let class_dir = temp.path().join("class/power_supply");
        let bat_dev = temp.path().join("devices/LNXSYSTM:00/PNP0C0A:00/BAT1");
        let ac_dev = temp.path().join("devices/LNXSYSTM:00/ACPI0003:00/ACAD");
        std::fs::create_dir_all(&bat_dev).expect("mkdir bat");
        std::fs::create_dir_all(&ac_dev).expect("mkdir ac");
        std::fs::create_dir_all(&class_dir).expect("mkdir class");
        std::fs::write(bat_dev.join("capacity"), "100\n").expect("capacity");
        std::fs::write(bat_dev.join("power_now"), "0\n").expect("power");
        std::fs::write(ac_dev.join("type"), "Mains\n").expect("type");
        std::fs::write(ac_dev.join("online"), "1\n").expect("online");
        std::os::unix::fs::symlink(&bat_dev, class_dir.join("BAT1")).expect("symlink bat");
        std::os::unix::fs::symlink(&ac_dev, class_dir.join("ACAD")).expect("symlink ac");
        let r = BatterySensor::new().read(temp.path()).expect("read");
        let b = match r {
            SensorReading::Battery(b) => b,
            other => panic!("expected Battery, got {other:?}"),
        };
        assert_eq!(b.soc_pct, Some(100));
        assert!(b.ac_online, "ACAD symlink target reports online=1");
    }

    /// SPEC §4 contract: on battery, `drain_w` reflects the
    /// `power_now` µW reading (12.5 W in the sibling fixture).
    /// Same test name as the roadmap §3 entry
    /// (`drain_rate_from_energy_delta`): the µW counter *is* the
    /// kernel's energy-delta-derived integration, just pre-divided.
    #[test]
    fn drain_rate_from_energy_delta() {
        const EXPECTED_W: f32 = 12.5;
        const EPS_W: f32 = 0.001;
        let r = BatterySensor::new()
            .read(&fixture("hx370-on-battery"))
            .expect("battery read");
        let b = match r {
            SensorReading::Battery(b) => b,
            other => panic!("expected Battery reading, got {other:?}"),
        };
        assert!(!b.ac_online, "on-battery fixture is unplugged");
        assert!(
            (b.drain_w - EXPECTED_W).abs() < EPS_W,
            "drain_w {} expected ~{EXPECTED_W} W",
            b.drain_w,
        );
        assert_eq!(b.soc_pct, Some(73));
    }
}
