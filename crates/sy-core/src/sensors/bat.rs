//! `/sys/class/power_supply/<BAT*>/{status,capacity,energy_now,
//! energy_full,power_now,current_now,voltage_now}` parser. Unlike the
//! procfs sensors, the kernel surfaces battery state as a directory of
//! one-value-per-file knobs rather than a single text blob; the "pure
//! parser" contract is therefore "given a `(name -> contents)` map
//! for one battery, produce the Sample". The I/O wrapper
//! [`sample()`] does the directory walk and feeds the map.
//!
//! Why a map and not the raw `&Path`? The map keeps the parser
//! testable from a literal `[("capacity", "73"), …]` fixture without
//! either tempdir setup or the per-file I/O contract that would force
//! the test to mirror sysfs layout.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Battery charging state as the kernel exposes it. We keep the
/// canonical sysfs strings as variants so the popup can render a
/// glyph per state without re-parsing a `String`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatteryStatus {
    Charging,
    Discharging,
    Full,
    NotCharging,
    Unknown,
}

impl BatteryStatus {
    /// Map the sysfs `status` token to a typed variant. Whitespace
    /// trimming is the caller's responsibility — the map values in
    /// [`parse_power_supply`] are already trimmed.
    fn from_token(raw: &str) -> Self {
        match raw {
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            "Full" => Self::Full,
            "Not charging" => Self::NotCharging,
            _ => Self::Unknown,
        }
    }
}

/// One battery's worth of state at one instant. `energy_*` and
/// `power_w` are `Option` because not every kernel/driver populates
/// them; the panel decides whether to surface "unknown" or to fall
/// back to the `current_now × voltage_now` product (handled in
/// [`parse_power_supply`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatterySample {
    /// Directory name (`BAT0`, `BAT1`, …). Lets a multi-battery
    /// laptop disambiguate downstream.
    pub name: String,
    pub status: BatteryStatus,
    /// State of charge, 0..=100.
    pub capacity_pct: u8,
    /// Stored energy in µWh, if `energy_now` is present.
    pub energy_now_uwh: Option<u64>,
    /// Design / present-full energy in µWh, if `energy_full` is
    /// present. Mon's "time-to-empty" math needs both.
    pub energy_full_uwh: Option<u64>,
    /// Instantaneous drain rate in W (positive = drawing power). Set
    /// from `power_now` (µW) when available, else derived from
    /// `current_now × voltage_now` (µA × µV → pW → W).
    pub power_w: Option<f32>,
}

/// One sensor tick of battery state. Empty `batteries` Vec means no
/// `BAT*` directory was found — the typical desktop case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatSample {
    pub batteries: Vec<BatterySample>,
}

/// Pure parser: take one battery directory's `(filename, contents)`
/// map and produce a [`BatterySample`]. Missing files yield `None`
/// optional fields; an absent or unparseable `capacity` drops the
/// whole battery (returns `None`) — a battery without a SOC reading
/// is not useful to the popup.
pub fn parse_power_supply(name: &str, files: &HashMap<&str, &str>) -> Option<BatterySample> {
    let capacity_pct = files.get("capacity")?.trim().parse::<u8>().ok()?;
    let status = files
        .get("status")
        .map(|s| BatteryStatus::from_token(s.trim()))
        .unwrap_or(BatteryStatus::Unknown);
    let energy_now_uwh = files
        .get("energy_now")
        .and_then(|s| s.trim().parse::<u64>().ok());
    let energy_full_uwh = files
        .get("energy_full")
        .and_then(|s| s.trim().parse::<u64>().ok());
    // power_now is signed (negative while charging on some drivers);
    // store the absolute value as the drain magnitude.
    let power_w = files
        .get("power_now")
        .and_then(|s| s.trim().parse::<i64>().ok())
        .map(|uw| (uw.unsigned_abs() as f32) / 1_000_000.0)
        .or_else(|| {
            let cur_ua = files.get("current_now")?.trim().parse::<i64>().ok()?;
            let volt_uv = files.get("voltage_now")?.trim().parse::<i64>().ok()?;
            // µA × µV = pW; ÷ 1e12 lands in W. i128 keeps the product
            // exact for any plausible laptop range.
            let pw = (cur_ua as i128) * (volt_uv as i128);
            Some((pw.unsigned_abs() as f64 / 1e12) as f32)
        });
    Some(BatterySample {
        name: name.to_string(),
        status,
        capacity_pct,
        energy_now_uwh,
        energy_full_uwh,
        power_w,
    })
}

/// Best-effort read of every well-known knob for one battery
/// directory. Missing files are skipped silently; the parser sees a
/// shorter map and emits `None`s where appropriate.
fn read_battery_files(dir: &Path) -> HashMap<String, String> {
    const KNOBS: &[&str] = &[
        "status",
        "capacity",
        "energy_now",
        "energy_full",
        "power_now",
        "current_now",
        "voltage_now",
    ];
    let mut out = HashMap::new();
    for knob in KNOBS {
        if let Ok(raw) = fs::read_to_string(dir.join(knob)) {
            out.insert((*knob).to_string(), raw);
        }
    }
    out
}

/// I/O wrapper: walk `/sys/class/power_supply/` once, collect every
/// `BAT*` directory, slurp its known knobs into a map, and call
/// [`parse_power_supply`]. Order is sorted by directory name so
/// `BAT0` precedes `BAT1` on multi-battery laptops.
pub fn sample() -> Option<BatSample> {
    let dir = Path::new("/sys/class/power_supply");
    let entries = fs::read_dir(dir).ok()?;
    let mut bat_dirs: Vec<_> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("BAT"))
                .unwrap_or(false)
        })
        .collect();
    bat_dirs.sort_by_key(|e| e.file_name());
    let mut batteries = Vec::new();
    for entry in bat_dirs {
        let name = entry.file_name().to_string_lossy().to_string();
        let files = read_battery_files(&entry.path());
        let map: HashMap<&str, &str> = files
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if let Some(b) = parse_power_supply(&name, &map) {
            batteries.push(b);
        }
    }
    Some(BatSample { batteries })
}

#[cfg(test)]
mod tests {
    use super::{parse_power_supply, BatteryStatus};
    use std::collections::HashMap;

    #[test]
    fn parse_power_supply_charging() {
        // On-charger snapshot: positive power_now, status=Charging.
        // Numbers chosen to be distinct so they catch a swapped field.
        const POWER_UW: i64 = 12_500_000; // 12.5 W
        let raw = [
            ("status", "Charging\n"),
            ("capacity", "73\n"),
            ("energy_now", "55000000\n"),
            ("energy_full", "75000000\n"),
            ("power_now", "12500000\n"),
        ];
        let files: HashMap<&str, &str> = raw.iter().copied().collect();
        let s = parse_power_supply("BAT0", &files).expect("well-formed charging fixture");
        assert_eq!(s.name, "BAT0");
        assert_eq!(s.status, BatteryStatus::Charging);
        assert_eq!(s.capacity_pct, 73);
        assert_eq!(s.energy_now_uwh, Some(55_000_000));
        assert_eq!(s.energy_full_uwh, Some(75_000_000));
        let power_w = s.power_w.expect("power_now populates power_w");
        assert!(
            (power_w - (POWER_UW as f32 / 1_000_000.0)).abs() < 0.001,
            "power_w {power_w} should be 12.5 W",
        );
    }

    #[test]
    fn parse_power_supply_discharging() {
        // Off-charger: status=Discharging, no power_now node — only
        // current_now × voltage_now is available. 1.0 A × 12.0 V →
        // 12.0 W expected.
        const EXPECTED_W: f32 = 12.0;
        const EPS_W: f32 = 0.001;
        let raw = [
            ("status", "Discharging\n"),
            ("capacity", "42\n"),
            ("current_now", "1000000\n"),  // 1.0 A
            ("voltage_now", "12000000\n"), // 12.0 V
        ];
        let files: HashMap<&str, &str> = raw.iter().copied().collect();
        let s = parse_power_supply("BAT1", &files).expect("well-formed discharging fixture");
        assert_eq!(s.name, "BAT1");
        assert_eq!(s.status, BatteryStatus::Discharging);
        assert_eq!(s.capacity_pct, 42);
        // No energy_* in this fixture.
        assert!(s.energy_now_uwh.is_none());
        assert!(s.energy_full_uwh.is_none());
        let power_w = s.power_w.expect("current×voltage populates power_w");
        assert!(
            (power_w - EXPECTED_W).abs() < EPS_W,
            "power_w {power_w} should be ~{EXPECTED_W} W",
        );
    }
}
