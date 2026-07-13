//! `sy gpu --waybar` — emits a waybar JSON tile for GPU utilisation +
//! VRAM. Adapter over [`sy_core::sensors::gpu_amd`] and
//! [`sy_core::sensors::gpu_nvidia`]; both sensors do the sysfs walk
//! / `nvidia-smi` spawn so `sy mon` and the waybar tile share one
//! read path per metric (sy-mon ROADMAP Step 5). Both vendors are
//! enumerated and the tile tracks the card with the most VRAM — the
//! discrete dGPU on a hybrid box (NVIDIA RTX + AMD iGPU), or the lone
//! card on the sy reference Ryzen AI 9 HX 370.

use anyhow::Result;

use sy_core::sensors::{gpu_amd, gpu_nvidia};

const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

const MIB: u64 = 1024 * 1024;

#[derive(Debug, Default)]
struct Snapshot {
    name: String,
    util_pct: u32,
    vram_used_mib: u64,
    vram_total_mib: u64,
}

pub fn run(waybar: bool) -> Result<()> {
    let s = read_first_gpu().unwrap_or_default();
    if waybar {
        println!("{}", waybar_out(&s));
        return Ok(());
    }
    if s.vram_total_mib == 0 {
        println!("no GPU detected");
        return Ok(());
    }
    println!(
        "{}: util {}% — VRAM {:.1} / {:.1} GiB ({}%)",
        s.name,
        s.util_pct,
        s.vram_used_mib as f64 / 1024.0,
        s.vram_total_mib as f64 / 1024.0,
        (s.vram_used_mib * 100)
            .checked_div(s.vram_total_mib)
            .unwrap_or(0),
    );
    Ok(())
}

/// The GPU the tile should track: across every detected card (AMD sysfs +
/// NVIDIA), the one with the most VRAM. On a hybrid laptop that's the
/// discrete dGPU (e.g. an NVIDIA RTX with 24 GiB), NOT the integrated Radeon
/// (~0.5 GiB UMA carve-out) — the iGPU's near-zero util and tiny VRAM were
/// exactly the "strange info" the old AMD-first probe surfaced. A single-GPU
/// box (the sy reference Ryzen AI) still picks its one card. The sy mon popup
/// is where per-card detail for multi-GPU systems goes.
fn read_first_gpu() -> Option<Snapshot> {
    select_primary(all_gpus())
}

/// Every detected GPU, both vendors, normalised to [`Snapshot`]. Cards that
/// report no VRAM total are dropped (they can't drive the VRAM-pressure
/// class and signal "not really present").
fn all_gpus() -> Vec<Snapshot> {
    let mut snaps: Vec<Snapshot> = Vec::new();
    for card in gpu_amd::sample().cards {
        let total = card.vram_total_bytes.unwrap_or(0) / MIB;
        if total == 0 {
            continue;
        }
        snaps.push(Snapshot {
            name: card.name,
            util_pct: u32::from(card.busy_pct.unwrap_or(0)),
            vram_used_mib: card.vram_used_bytes.unwrap_or(0) / MIB,
            vram_total_mib: total,
        });
    }
    for gpu in gpu_nvidia::sample().gpus {
        let total = gpu.vram_total_mib.unwrap_or(0);
        if total == 0 {
            continue;
        }
        snaps.push(Snapshot {
            name: gpu.name,
            util_pct: u32::from(gpu.util_pct.unwrap_or(0)),
            vram_used_mib: gpu.vram_used_mib.unwrap_or(0),
            vram_total_mib: total,
        });
    }
    snaps
}

/// Pick the primary GPU: the one with the most VRAM. On a hybrid box (an
/// NVIDIA dGPU alongside an AMD iGPU) this is the discrete card; on a
/// single-GPU box it's the only card. `None` when nothing was detected.
fn select_primary(snaps: Vec<Snapshot>) -> Option<Snapshot> {
    snaps.into_iter().max_by_key(|s| s.vram_total_mib)
}

fn waybar_out(s: &Snapshot) -> String {
    // No GPU detected → return an empty tile so waybar hides it.
    if s.vram_total_mib == 0 {
        return r#"{"text":"","class":"absent","tooltip":""}"#.to_string();
    }
    // Bar on GPU compute utilisation (the "is anything running?" signal
    // that mirrors the CPU module). VRAM pressure stays in the tooltip
    // and drives the warning/critical class so a near-OOM card still
    // shouts via the colour.
    let vram_pct = ((s.vram_used_mib * 100) / s.vram_total_mib) as u32;
    let bar = BARS[(s.util_pct as usize * (BARS.len() - 1) / 100).min(BARS.len() - 1)];
    let class = if vram_pct >= 90 || s.util_pct >= 95 {
        "critical"
    } else if vram_pct >= 70 || s.util_pct >= 70 {
        "warning"
    } else {
        "ok"
    };
    let tooltip = format!(
        "{}\\nutil {}%\\nVRAM {:.1} / {:.1} GiB ({}%)",
        s.name,
        s.util_pct,
        s.vram_used_mib as f64 / 1024.0,
        s.vram_total_mib as f64 / 1024.0,
        vram_pct,
    );
    // 󰢮 = nerd-font GPU glyph (replaces the old wifi-signal 󰤥). Pairs
    // with the CPU/RAM modules' " ▁..█" style.
    format!(
        r#"{{"text":"󰢮 {bar}","class":"{class}","tooltip":"{tooltip}","percentage":{pct}}}"#,
        pct = s.util_pct
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goldens lock the Nerd Font GPU glyph and the `\n`-escaped
    /// tooltip layout. `configs/waybar/modules.jsonc` already binds the
    /// `ok`/`warning`/`critical` classes to CSS rules.
    const GOLDEN_GPU_NV: &str = include_str!("../tests/snapshots/waybar/gpu-nvidia.json");
    const GOLDEN_GPU_AMD: &str = include_str!("../tests/snapshots/waybar/gpu-amd.json");
    const GOLDEN_GPU_ABSENT: &str = include_str!("../tests/snapshots/waybar/gpu-absent.json");

    #[test]
    fn waybar_output_matches_snapshot_nvidia() {
        let s = Snapshot {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            util_pct: 42,
            vram_used_mib: 8192,
            vram_total_mib: 24576,
        };
        assert_eq!(format!("{}\n", waybar_out(&s)), GOLDEN_GPU_NV);
    }

    #[test]
    fn select_primary_prefers_discrete_dgpu_over_integrated() {
        // Hybrid laptop: tiny AMD iGPU (~0.5 GiB UMA) + a 24 GiB NVIDIA
        // dGPU. The tile must track the dGPU — the card with real VRAM that
        // the user actually cares about — not the iGPU the old AMD-first
        // probe latched onto.
        let igpu = Snapshot {
            name: "card1".to_string(),
            util_pct: 2,
            vram_used_mib: 200,
            vram_total_mib: 512,
        };
        let dgpu = Snapshot {
            name: "NVIDIA GeForce RTX 5090 Laptop GPU".to_string(),
            util_pct: 10,
            vram_used_mib: 23014,
            vram_total_mib: 24463,
        };
        // Order must not matter: iGPU first (AMD-probed-first ordering).
        let picked = select_primary(vec![igpu, dgpu]).expect("a gpu");
        assert_eq!(picked.name, "NVIDIA GeForce RTX 5090 Laptop GPU");
        assert_eq!(picked.vram_total_mib, 24463);
    }

    #[test]
    fn select_primary_handles_single_and_empty() {
        assert!(select_primary(Vec::new()).is_none());
        let only = Snapshot {
            name: "card0".to_string(),
            util_pct: 5,
            vram_used_mib: 100,
            vram_total_mib: 8192,
        };
        assert_eq!(select_primary(vec![only]).expect("one gpu").name, "card0");
    }

    #[test]
    fn waybar_absent_matches_snapshot() {
        let s = Snapshot::default();
        assert_eq!(format!("{}\n", waybar_out(&s)), GOLDEN_GPU_ABSENT);
    }

    #[test]
    fn waybar_output_matches_snapshot_amd() {
        // The AMD path goes through `sensors::gpu_amd::GpuAmdSample`
        // and is normalised to the same `Snapshot` shape so the JSON
        // surface stays byte-identical to the NVIDIA tile (single
        // CSS contract in `configs/waybar/`). card0 with 37% busy,
        // 1 GiB / 8 GiB VRAM → vram_pct = 12, class = "ok".
        let s = Snapshot {
            name: "card0".to_string(),
            util_pct: 37,
            vram_used_mib: 1024,
            vram_total_mib: 8192,
        };
        assert_eq!(format!("{}\n", waybar_out(&s)), GOLDEN_GPU_AMD);
    }
}
