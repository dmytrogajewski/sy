//! `sy gpu --waybar` — emits a waybar JSON tile for GPU utilisation +
//! VRAM. Adapter over [`sy_core::sensors::gpu_amd`] and
//! [`sy_core::sensors::gpu_nvidia`]; both sensors do the sysfs walk
//! / `nvidia-smi` spawn so `sy mon` and the waybar tile share one
//! read path per metric (sy-mon ROADMAP Step 5). AMD is probed
//! first because that's the most common case on the sy reference
//! Ryzen AI 9 HX 370; NVIDIA is the fallback.

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

/// Probe AMD first (the sy reference platform), then NVIDIA. Returns
/// the first card that reports a non-zero VRAM total — that's the
/// tile's "GPU present" signal. Multi-GPU boxes are out of scope for
/// the waybar tile (one glyph, one bar); the sy mon popup is where
/// per-card detail goes.
fn read_first_gpu() -> Option<Snapshot> {
    if let Some(card) = gpu_amd::sample().cards.into_iter().next() {
        let total = card.vram_total_bytes.unwrap_or(0) / MIB;
        if total > 0 {
            return Some(Snapshot {
                name: card.name,
                util_pct: u32::from(card.busy_pct.unwrap_or(0)),
                vram_used_mib: card.vram_used_bytes.unwrap_or(0) / MIB,
                vram_total_mib: total,
            });
        }
    }
    let gpu = gpu_nvidia::sample().gpus.into_iter().next()?;
    Some(Snapshot {
        name: gpu.name,
        util_pct: u32::from(gpu.util_pct.unwrap_or(0)),
        vram_used_mib: gpu.vram_used_mib.unwrap_or(0),
        vram_total_mib: gpu.vram_total_mib.unwrap_or(0),
    })
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
