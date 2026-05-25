//! `sy npu --waybar` — emits a waybar JSON tile for the AMD Ryzen AI
//! NPU. Adapter over [`sy_core::sensors::npu_xdna::sample()`] — the
//! pm_runtime delta logic, holders walk, and `amdgpu_top --xdna`
//! fallback all live in `crates/sy-core/src/sensors/npu_xdna.rs` so
//! `sy mon` and the waybar tile share one read path per metric
//! (sy-mon ROADMAP Step 5).
//!
//! The only sysfs/procfs read still living here is `/proc/cpuinfo`
//! for the CPU model name in the tooltip — a UI concern that the
//! sensor crate deliberately doesn't carry.

use anyhow::Result;

use sy_core::sensors::npu_xdna::{self, NpuXdnaSample};

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
        println!("{}", waybar_out(&s));
        return Ok(());
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

/// Build the local tile view-model by projecting a `NpuXdnaSample`
/// from the shared sensor. The sample carries the pm_runtime delta,
/// the holders list, the BDF, and the firmware string; the only
/// per-tile bit added here is the friendly CPU-model name for the
/// tooltip (a UI concern that doesn't belong in `sy-core`).
fn snapshot() -> Snapshot {
    project(npu_xdna::sample())
}

fn project(sample: NpuXdnaSample) -> Snapshot {
    if !sample.present {
        return Snapshot::default();
    }
    let bdf = sample.bdf.unwrap_or_default();
    let name = read_pci_name(&bdf);
    Snapshot {
        present: true,
        active: sample.active,
        util_pct: sample.util_pct,
        fw_version: sample.fw_version.unwrap_or_default(),
        holders: sample.holders,
        bdf,
        name,
    }
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

fn waybar_out(s: &Snapshot) -> String {
    if !s.present {
        return r#"{"text":"","class":"absent","tooltip":""}"#.to_string();
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
    format!(
        r#"{{"text":"󰍛 {bar}","class":"{class}","tooltip":"{tooltip}","percentage":{pct}}}"#,
        pct = s.util_pct
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden file: the byte-exact waybar JSON the tile emits for a
    /// known synthetic snapshot. Locks the tooltip's `\n` escapes and
    /// the Nerd Font chip glyph so a `configs/waybar/*` consumer that
    /// already parses these strings keeps working after the read-path
    /// migration to `sy_core::sensors::npu_xdna`.
    const GOLDEN_NPU: &str = include_str!("../tests/snapshots/waybar/npu.json");
    const GOLDEN_NPU_ABSENT: &str = include_str!("../tests/snapshots/waybar/npu-absent.json");

    #[test]
    fn waybar_output_matches_snapshot() {
        let s = Snapshot {
            present: true,
            active: true,
            util_pct: 73,
            fw_version: "1.5.10".to_string(),
            holders: vec!["sy-aiplane".to_string()],
            bdf: "c5:00.1".to_string(),
            name: "NPU on 9 HX 370".to_string(),
        };
        assert_eq!(format!("{}\n", waybar_out(&s)), GOLDEN_NPU);
    }

    #[test]
    fn waybar_absent_matches_snapshot() {
        let s = Snapshot::default();
        assert_eq!(format!("{}\n", waybar_out(&s)), GOLDEN_NPU_ABSENT);
    }
}
