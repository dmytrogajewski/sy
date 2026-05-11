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
    println!(
        r#"{{"text":"󰢮 {bar}","class":"{class}","tooltip":"{tooltip}","percentage":{pct}}}"#,
        pct = s.util_pct
    );
    Ok(())
}
