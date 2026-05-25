//! `/sys/class/drm/card*/device/{gpu_busy_percent, mem_info_vram_used,
//! mem_info_vram_total, hwmon/.../temp1_input,
//! hwmon/.../power1_average}` reader for AMDGPU cards.
//!
//! Like [`super::bat`], the kernel surfaces AMDGPU state as a directory
//! of one-value-per-file knobs rather than a single text blob; the
//! "pure parser" contract is therefore "given a `(name -> contents)`
//! map for one card, produce the Sample". The I/O wrapper
//! [`sample()`] does the `/sys/class/drm/card*` walk and feeds the
//! map.
//!
//! Why a map and not the raw `&Path`? The map keeps the parser
//! testable from a literal `[("gpu_busy_percent", "37"), …]` fixture
//! without either tempdir setup or the per-file I/O contract that
//! would force the test to mirror sysfs layout.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One AMDGPU card's worth of state at one instant. All accelerator
/// knobs are `Option` because the kernel populates a different subset
/// per driver / per card (APU vs. dGPU vs. partial hwmon binding).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuAmdSample {
    /// `card*` directory name (`card0`, `card1`, …). Lets a
    /// multi-GPU box disambiguate downstream.
    pub name: String,
    /// `gpu_busy_percent` — 0..=100. The kernel computes this over a
    /// short rolling window, so a single read is a meaningful util
    /// reading (unlike CPU /proc/stat which needs a delta).
    pub busy_pct: Option<u8>,
    /// VRAM in use in bytes, from `mem_info_vram_used`.
    pub vram_used_bytes: Option<u64>,
    /// VRAM total in bytes, from `mem_info_vram_total`.
    pub vram_total_bytes: Option<u64>,
    /// Edge / junction temperature in Celsius, from
    /// `hwmon/.../temp1_input` (millidegrees in sysfs, Celsius here).
    pub temp_c: Option<f32>,
    /// Average package power draw in watts, from
    /// `hwmon/.../power1_average` (microwatts in sysfs, watts here).
    pub power_w: Option<f32>,
}

/// One sensor tick of AMDGPU state — every `card*` directory the
/// kernel surfaces under `/sys/class/drm`. Empty `cards` Vec means
/// no AMDGPU was found (NVIDIA-only or no discrete GPU).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuAmdSnapshot {
    pub cards: Vec<GpuAmdSample>,
}

/// Pure parser: take one card's `(filename, contents)` map and
/// produce a [`GpuAmdSample`]. Missing files yield `None` optional
/// fields. `name` is the caller's responsibility (`card0` etc.) — the
/// map intentionally doesn't carry it so two callers can share a
/// fixture across cards.
pub fn parse_drm_card(name: &str, files: &HashMap<&str, &str>) -> GpuAmdSample {
    let busy_pct = files
        .get("gpu_busy_percent")
        .and_then(|s| s.trim().parse::<u8>().ok());
    let vram_used_bytes = files
        .get("mem_info_vram_used")
        .and_then(|s| s.trim().parse::<u64>().ok());
    let vram_total_bytes = files
        .get("mem_info_vram_total")
        .and_then(|s| s.trim().parse::<u64>().ok());
    // hwmon temp1_input is millidegrees C; convert to C. The hwmon
    // dir name varies (`hwmon0`, `hwmon1`, …) so callers flatten it
    // to the leaf filename `temp1_input` in the map.
    let temp_c = files
        .get("temp1_input")
        .and_then(|s| s.trim().parse::<i32>().ok())
        .map(|mdeg| mdeg as f32 / 1_000.0);
    // hwmon power1_average is microwatts.
    let power_w = files
        .get("power1_average")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|uw| uw as f32 / 1_000_000.0);
    GpuAmdSample {
        name: name.to_string(),
        busy_pct,
        vram_used_bytes,
        vram_total_bytes,
        temp_c,
        power_w,
    }
}

/// Locate the single `hwmon*` subdirectory under a card's `device`
/// directory. AMDGPU binds exactly one hwmon node per card; the
/// number suffix (`hwmon0` vs `hwmon3`) depends on probe order.
fn find_hwmon_dir(device_dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(device_dir.join("hwmon")).ok()?;
    for ent in entries.flatten() {
        let name = ent.file_name();
        let n = name.to_string_lossy();
        if n.starts_with("hwmon") {
            return Some(ent.path());
        }
    }
    None
}

/// Read one card's sysfs knobs into a `(filename, contents)` map.
/// Missing files are silently absent — the parser treats absence as
/// `None`. The map's keys live as long as the returned `Vec<String>`
/// holder.
fn collect_card_files(device_dir: &Path) -> Vec<(String, String)> {
    const TOP_KNOBS: [&str; 3] = [
        "gpu_busy_percent",
        "mem_info_vram_used",
        "mem_info_vram_total",
    ];
    const HWMON_KNOBS: [&str; 2] = ["temp1_input", "power1_average"];
    let mut out = Vec::new();
    for knob in TOP_KNOBS {
        if let Ok(raw) = fs::read_to_string(device_dir.join(knob)) {
            out.push((knob.to_string(), raw));
        }
    }
    if let Some(hwmon) = find_hwmon_dir(device_dir) {
        for knob in HWMON_KNOBS {
            if let Ok(raw) = fs::read_to_string(hwmon.join(knob)) {
                out.push((knob.to_string(), raw));
            }
        }
    }
    out
}

/// I/O wrapper: walks `/sys/class/drm/card*` and produces a sample
/// for each AMDGPU card. Returns an empty `GpuAmdSnapshot` (not
/// `None`) when `/sys/class/drm` is missing — that's the
/// no-GPU/container case, not a sensor failure.
pub fn sample() -> GpuAmdSnapshot {
    let mut cards = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return GpuAmdSnapshot { cards };
    };
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    for ent in entries.flatten() {
        let name = ent.file_name();
        let n = name.to_string_lossy();
        // Match `card0`, `card1`, … but not `card0-DP-1` (connector
        // nodes have a hyphen). The kernel emits one card-N dir per
        // physical card; connectors hang off as siblings.
        let Some(suffix) = n.strip_prefix("card") else {
            continue;
        };
        if !suffix.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        names.push((n.into_owned(), ent.path().join("device")));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, device_dir) in names {
        if !device_dir.exists() {
            continue;
        }
        let owned = collect_card_files(&device_dir);
        let map: HashMap<&str, &str> = owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        // AMDGPU is identifiable by the presence of any of the
        // AMD-specific knobs; if none parsed, this is likely an
        // i915/nouveau card — skip.
        if map.is_empty() {
            continue;
        }
        cards.push(parse_drm_card(&name, &map));
    }
    GpuAmdSnapshot { cards }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_drm_card_present() {
        // Canned fixture: card0 with all knobs populated. Values come
        // from a Ryzen AI 9 HX 370 reference reading captured in
        // SPEC §2 "Technical context".
        let files: HashMap<&str, &str> = [
            ("gpu_busy_percent", "37"),
            ("mem_info_vram_used", "1073741824"),
            ("mem_info_vram_total", "8589934592"),
            ("temp1_input", "52000"),
            ("power1_average", "12500000"),
        ]
        .into_iter()
        .collect();
        let s = parse_drm_card("card0", &files);
        assert_eq!(s.name, "card0");
        assert_eq!(s.busy_pct, Some(37));
        assert_eq!(s.vram_used_bytes, Some(1_073_741_824));
        assert_eq!(s.vram_total_bytes, Some(8_589_934_592));
        // 52000 mC = 52.0 C.
        assert_eq!(s.temp_c, Some(52.0));
        // 12_500_000 uW = 12.5 W.
        assert_eq!(s.power_w, Some(12.5));
    }

    #[test]
    fn missing_hwmon_returns_none() {
        // Partial sysfs: top-level knobs present, hwmon entirely
        // absent (a common state on freshly probed APUs before the
        // thermal binding settles).
        let files: HashMap<&str, &str> = [
            ("gpu_busy_percent", "5"),
            ("mem_info_vram_used", "0"),
            ("mem_info_vram_total", "8589934592"),
        ]
        .into_iter()
        .collect();
        let s = parse_drm_card("card1", &files);
        assert_eq!(s.name, "card1");
        assert_eq!(s.busy_pct, Some(5));
        assert_eq!(s.vram_used_bytes, Some(0));
        assert_eq!(s.vram_total_bytes, Some(8_589_934_592));
        assert!(s.temp_c.is_none());
        assert!(s.power_w.is_none());
    }
}
