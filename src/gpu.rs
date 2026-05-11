//! `sy gpu --waybar` — emits a waybar JSON tile for NVIDIA GPU
//! utilisation + VRAM. Falls back gracefully when `nvidia-smi` is
//! absent (returns 0% / no tooltip).

use std::process::Command;

use anyhow::Result;

const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

#[derive(Debug, Default)]
struct Snapshot {
    name: String,
    util_pct: u32,
    vram_used_mib: u64,
    vram_total_mib: u64,
}

pub fn run(waybar: bool) -> Result<()> {
    let s = read_nvidia_smi().unwrap_or_default();
    if waybar {
        return waybar_out(&s);
    }
    if s.vram_total_mib == 0 {
        println!("no NVIDIA GPU detected");
        return Ok(());
    }
    println!(
        "{}: util {}% — VRAM {:.1} / {:.1} GiB ({}%)",
        s.name,
        s.util_pct,
        s.vram_used_mib as f64 / 1024.0,
        s.vram_total_mib as f64 / 1024.0,
        if s.vram_total_mib > 0 {
            (s.vram_used_mib * 100) / s.vram_total_mib
        } else {
            0
        },
    );
    Ok(())
}

fn read_nvidia_smi() -> Option<Snapshot> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.lines().next()?;
    let mut parts = line.split(',').map(|s| s.trim());
    let name = parts.next()?.to_string();
    let util = parts.next()?.parse().ok()?;
    let used = parts.next()?.parse().ok()?;
    let total = parts.next()?.parse().ok()?;
    Some(Snapshot {
        name,
        util_pct: util,
        vram_used_mib: used,
        vram_total_mib: total,
    })
}

fn waybar_out(s: &Snapshot) -> Result<()> {
    // No GPU detected → return an empty tile so waybar hides it.
    if s.vram_total_mib == 0 {
        println!(r#"{{"text":"","class":"absent","tooltip":""}}"#);
        return Ok(());
    }
    // We surface VRAM pressure (more interesting than util on a laptop
    // doing background inference) on the icon, and util in the tooltip.
    let vram_pct = ((s.vram_used_mib * 100) / s.vram_total_mib) as usize;
    let icon = BARS[(vram_pct * (BARS.len() - 1) / 100).min(BARS.len() - 1)];
    let class = if vram_pct >= 90 {
        "critical"
    } else if vram_pct >= 70 {
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
    println!(
        r#"{{"text":"󰤥 {icon}","class":"{class}","tooltip":"{tooltip}","percentage":{vram_pct}}}"#
    );
    Ok(())
}
