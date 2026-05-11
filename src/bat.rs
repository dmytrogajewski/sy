//! Battery applet — emits waybar JSON with a Font Awesome battery glyph
//! that changes with charge level. Charging state is shown by prepending
//! a bolt glyph.

use std::fs;
use std::path::Path;

use anyhow::Result;

// Font Awesome battery ramp (JetBrainsMono Nerd Font).
const BAT_EMPTY: &str = "\u{F244}"; // fa-battery-0
const BAT_QUARTER: &str = "\u{F243}"; // fa-battery-1
const BAT_HALF: &str = "\u{F242}"; // fa-battery-2
const BAT_THREE_Q: &str = "\u{F241}"; // fa-battery-3
const BAT_FULL: &str = "\u{F240}"; // fa-battery-4
const BOLT: &str = "\u{F0E7}"; // fa-bolt (charging)

pub fn run(waybar: bool) -> Result<()> {
    if waybar {
        waybar_out()
    } else {
        // Future: a battery info popup. For now this is bar-only.
        Ok(())
    }
}

fn waybar_out() -> Result<()> {
    let Some((cap, status)) = read_first_battery() else {
        // No battery (desktop) — emit empty so the tile collapses.
        println!(r#"{{"text":"","class":"hidden","tooltip":""}}"#);
        return Ok(());
    };
    let charging = status == "Charging";
    let critical = !charging && cap <= 15;
    let class = if charging {
        "charging"
    } else if critical {
        "critical"
    } else if cap <= 30 {
        "low"
    } else if cap <= 60 {
        "mid"
    } else if cap <= 99 {
        "high"
    } else {
        "full"
    };
    let body = bucket_glyph(cap);
    let text = if charging {
        format!("{BOLT}{body}")
    } else {
        body.to_string()
    };
    let tooltip = format!("battery {cap}% — {status}");
    println!(r#"{{"text":"{text}","class":"{class}","tooltip":"{tooltip}","alt":"{cap}"}}"#);
    Ok(())
}

fn bucket_glyph(cap: u8) -> &'static str {
    match cap {
        0..=20 => BAT_EMPTY,
        21..=40 => BAT_QUARTER,
        41..=60 => BAT_HALF,
        61..=80 => BAT_THREE_Q,
        _ => BAT_FULL,
    }
}

fn read_first_battery() -> Option<(u8, String)> {
    let dir = Path::new("/sys/class/power_supply");
    let rd = fs::read_dir(dir).ok()?;
    let mut entries: Vec<_> = rd.flatten().collect();
    // Stable order: BAT0 before BAT1 etc.
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if !n.starts_with("BAT") {
            continue;
        }
        let cap = fs::read_to_string(entry.path().join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok())?;
        let status = fs::read_to_string(entry.path().join("status"))
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "Unknown".into());
        return Some((cap, status));
    }
    None
}
