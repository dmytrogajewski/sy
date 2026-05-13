//! `sy npu --waybar` — emits a waybar JSON tile for the AMD Ryzen AI
//! NPU. Reports actual utilisation %, computed from the kernel's
//! pm_runtime counters at `/sys/.../power/runtime_{active,suspended}_time`:
//!
//!     util_pct = Δactive / (Δactive + Δsuspended) over the
//!     interval between this and the previous sample.
//!
//! We persist the last sample in $XDG_RUNTIME_DIR/sy-npu.last so each
//! waybar tick (2 s by default) gets a real 2-second utilisation
//! window. The first tick after boot/reset has no prior sample → we
//! fall back to the binary D0/D3 power-state read.

use std::path::{Path, PathBuf};

use anyhow::Result;

const ACCEL_DEV: &str = "/dev/accel/accel0";
const POWER_STATE_PATH: &str = "/sys/class/accel/accel0/device/power_state";
const FW_VERSION_PATH: &str = "/sys/class/accel/accel0/device/fw_version";
const RUNTIME_ACTIVE: &str = "/sys/class/accel/accel0/device/power/runtime_active_time";
const RUNTIME_SUSPENDED: &str = "/sys/class/accel/accel0/device/power/runtime_suspended_time";

const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

#[derive(Debug, Default)]
struct Snapshot {
    present: bool,
    active: bool,
    util_pct: u32, // 0..=100, computed from pm_runtime deltas
    fw_version: String,
    holders: Vec<String>,
    bdf: String,
    name: String,
}

pub fn run(waybar: bool) -> Result<()> {
    let s = snapshot();
    if waybar {
        return waybar_out(&s);
    }
    if !s.present {
        println!("no AMD XDNA NPU detected (no /dev/accel/accel0)");
        return Ok(());
    }
    println!(
        "{} @ {}\n  util:     {}%\n  state:    {}\n  firmware: {}\n  holders:  {}",
        if s.name.is_empty() { "NPU" } else { &s.name },
        s.bdf,
        s.util_pct,
        if s.active { "active (D0)" } else { "idle (D3)" },
        if s.fw_version.is_empty() {
            "?"
        } else {
            &s.fw_version
        },
        if s.holders.is_empty() {
            "(none)".to_string()
        } else {
            s.holders.join(", ")
        },
    );
    Ok(())
}

fn snapshot() -> Snapshot {
    let mut s = Snapshot::default();
    if !Path::new(ACCEL_DEV).exists() {
        return s;
    }
    s.present = true;
    s.active = std::fs::read_to_string(POWER_STATE_PATH)
        .ok()
        .map(|v| v.trim() == "D0")
        .unwrap_or(false);
    s.util_pct = read_util_pct(s.active);
    s.fw_version = std::fs::read_to_string(FW_VERSION_PATH)
        .ok()
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    s.bdf = read_bdf().unwrap_or_default();
    s.name = read_pci_name(&s.bdf);
    s.holders = find_holders();
    s
}

/// Compute NPU utilisation as a percentage over the interval since the
/// previous waybar tick. Falls back to a binary 100/0 read of
/// `power_state` if we have no prior sample (first tick, missing
/// $XDG_RUNTIME_DIR, etc.).
fn read_util_pct(active_now: bool) -> u32 {
    let Some((active, suspended)) = read_pm_counters() else {
        return if active_now { 100 } else { 0 };
    };
    let cache = sample_cache_path();
    let prev = std::fs::read_to_string(&cache).ok().and_then(parse_sample);
    if let Err(e) = persist_sample(&cache, active, suspended) {
        // Non-fatal: we just won't have a delta next tick.
        let _ = e;
    }
    let Some((p_active, p_suspended)) = prev else {
        return if active_now { 100 } else { 0 };
    };
    let d_active = active.saturating_sub(p_active);
    let d_suspended = suspended.saturating_sub(p_suspended);
    let total = d_active + d_suspended;
    if total == 0 {
        // No time elapsed in either bucket → the device is either
        // perfectly idle (D3 the whole time, both counters frozen
        // because of runtime PM) or perfectly busy. The current
        // power_state read disambiguates.
        return if active_now { 100 } else { 0 };
    }
    ((d_active * 100) / total).min(100) as u32
}

fn read_pm_counters() -> Option<(u64, u64)> {
    let a = std::fs::read_to_string(RUNTIME_ACTIVE).ok()?;
    let s = std::fs::read_to_string(RUNTIME_SUSPENDED).ok()?;
    let av: u64 = a.trim().parse().ok()?;
    let sv: u64 = s.trim().parse().ok()?;
    Some((av, sv))
}

fn parse_sample(s: String) -> Option<(u64, u64)> {
    let mut it = s.split_whitespace();
    let a: u64 = it.next()?.parse().ok()?;
    let s: u64 = it.next()?.parse().ok()?;
    Some((a, s))
}

fn persist_sample(path: &Path, active: u64, suspended: u64) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    std::fs::write(path, format!("{active} {suspended}\n"))
}

fn sample_cache_path() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("sy-npu.last");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/sy-npu.last"))
}

fn read_bdf() -> Option<String> {
    let link = std::fs::read_link("/sys/class/accel/accel0/device").ok()?;
    Some(
        link.file_name()?
            .to_string_lossy()
            .trim_start_matches("0000:")
            .to_string(),
    )
}

fn read_pci_name(_bdf: &str) -> String {
    // The PCI vendor string from lspci is `Strix/Krackan/Strix Halo
    // Neural Processing Unit` — useless for telling those three SKUs
    // apart. The CPU model name from /proc/cpuinfo *does* pin it down
    // (`AMD Ryzen AI 9 HX 370` → Strix Point, etc.), so use that.
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_default();
    let short = cpu
        .strip_prefix("AMD Ryzen AI ")
        .map(|s| {
            s.split_once(" w/ ")
                .map(|(left, _)| left.to_string())
                .unwrap_or_else(|| s.to_string())
        })
        .unwrap_or(cpu);
    if short.is_empty() {
        "NPU".to_string()
    } else {
        format!("NPU on {short}")
    }
}

fn find_holders() -> Vec<String> {
    let mut holders = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return holders;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if !n.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid = n.into_owned();
        let fd_dir: PathBuf = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        let mut hit = false;
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path()) {
                if target.to_string_lossy() == ACCEL_DEV {
                    hit = true;
                    break;
                }
            }
        }
        if hit {
            let comm = std::fs::read_to_string(entry.path().join("comm"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| format!("pid {pid}"));
            holders.push(comm);
        }
    }
    holders.sort();
    holders.dedup();
    holders
}

fn waybar_out(s: &Snapshot) -> Result<()> {
    if !s.present {
        println!(r#"{{"text":"","class":"absent","tooltip":""}}"#);
        return Ok(());
    }
    let bar = BARS[(s.util_pct as usize * (BARS.len() - 1) / 100).min(BARS.len() - 1)];
    let class = if s.util_pct >= 70 {
        "active"
    } else if s.util_pct == 0 {
        "idle"
    } else {
        "active"
    };
    let name = if s.name.is_empty() { "NPU" } else { &s.name };
    let holders = if s.holders.is_empty() {
        "(none)".to_string()
    } else {
        s.holders.join(", ")
    };
    let tooltip = format!(
        "{}\\nutil {}%\\nstate {}\\nFW {}\\nopen by: {}",
        name,
        s.util_pct,
        if s.active { "D0 (active)" } else { "D3 (idle)" },
        if s.fw_version.is_empty() {
            "?"
        } else {
            &s.fw_version
        },
        holders,
    );
    // 󰍛 = nerd-font chip glyph; matches the CPU/RAM bar styling.
    println!(
        r#"{{"text":"󰍛 {bar}","class":"{class}","tooltip":"{tooltip}","percentage":{pct}}}"#,
        pct = s.util_pct
    );
    Ok(())
}
