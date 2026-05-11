//! `sy npu --waybar` — emits a waybar JSON tile for the AMD Ryzen AI
//! NPU. Reads `/sys/class/accel/accel0/device/power_state` for the
//! active/idle signal and probes `/proc/*/fd/` to see who's holding
//! `/dev/accel/accel0` right now.
//!
//! `power_state == "D0"` → active (some process has the device open
//! and the firmware is in run state); `D3*` → idle. Sampling at
//! waybar's 2-second cadence catches sustained workloads; short
//! 100 ms inferences are mostly invisible.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

const ACCEL_DEV: &str = "/dev/accel/accel0";
const POWER_STATE_PATH: &str = "/sys/class/accel/accel0/device/power_state";
const FW_VERSION_PATH: &str = "/sys/class/accel/accel0/device/fw_version";

#[derive(Debug, Default)]
struct Snapshot {
    present: bool,
    active: bool,
    fw_version: String,
    holders: Vec<String>, // process names with the device fd open
    bdf: String,          // PCI bus:dev.fn
    name: String,         // "NPU Strix"
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
        "{} @ {}\n  state:    {}\n  firmware: {}\n  holders:  {}",
        if s.name.is_empty() { "NPU" } else { &s.name },
        s.bdf,
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
    s.fw_version = std::fs::read_to_string(FW_VERSION_PATH)
        .ok()
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    s.bdf = read_bdf().unwrap_or_default();
    s.name = read_pci_name(&s.bdf);
    s.holders = find_holders();
    s
}

fn read_bdf() -> Option<String> {
    // /sys/class/accel/accel0/device → /sys/devices/pci.../0000:xx:yy.z
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
    // Strip the marketing prefix ("AMD Ryzen AI 9 ") so the tooltip
    // is just "HX 370" or whatever.
    let short = cpu
        .strip_prefix("AMD Ryzen AI ")
        .map(|s| {
            // Strip the trailing iGPU annotation, e.g. "9 HX 370 w/ Radeon 890M".
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

/// Walk /proc/*/fd/ to find which processes have /dev/accel/accel0 open.
/// Permission-limited to fds the current user owns, which is fine —
/// usually only sy / our own daemons / xrt-smi.
fn find_holders() -> Vec<String> {
    let mut holders = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return holders };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if !n.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid = n.into_owned();
        let fd_dir: PathBuf = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else { continue };
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
        // No NPU → empty tile so waybar can hide it.
        println!(r#"{{"text":"","class":"absent","tooltip":""}}"#);
        return Ok(());
    }
    let icon = if s.active { "󰍛" } else { "󰒲" };
    let class = if s.active { "active" } else { "idle" };
    let name = if s.name.is_empty() { "NPU" } else { &s.name };
    let holders = if s.holders.is_empty() {
        "(none)".to_string()
    } else {
        s.holders.join(", ")
    };
    let tooltip = format!(
        "{}\\n{}\\nFW {}\\nopen by: {}",
        name,
        if s.active { "active" } else { "idle" },
        if s.fw_version.is_empty() {
            "?"
        } else {
            &s.fw_version
        },
        holders,
    );
    println!(
        r#"{{"text":"{icon}","class":"{class}","tooltip":"{tooltip}"}}"#
    );
    Ok(())
}
