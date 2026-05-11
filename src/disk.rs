//! Disk applet: hidden when free space on `/` ≥ threshold; opens a fuzzel
//! menu of cleaning strategies otherwise. The strategy layer
//! (`strategies::CleanStrategy`) is the extension point for an ML ranker.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

pub mod strategies;

use strategies::{Outcome, Probe, Ranker, ReclaimableRanker};

const DEFAULT_THRESHOLD_GIB: u64 = 30;
const ENV_THRESHOLD: &str = "SY_DISK_THRESHOLD_GIB";

// fa-hdd-o (U+F0A0) — JetBrainsMono Nerd Font.
const GLYPH_DISK: &str = "\u{F0A0}";

pub fn run(waybar: bool, threshold_gib: Option<u64>) -> Result<()> {
    let threshold = resolve_threshold(threshold_gib).saturating_mul(1024 * 1024 * 1024);
    if waybar {
        waybar_out(threshold)
    } else {
        menu(threshold)
    }
}

// CLIG: flag > env > default.
fn resolve_threshold(flag: Option<u64>) -> u64 {
    if let Some(v) = flag {
        return v;
    }
    if let Ok(s) = std::env::var(ENV_THRESHOLD) {
        if let Ok(n) = s.parse::<u64>() {
            return n;
        }
    }
    DEFAULT_THRESHOLD_GIB
}

fn free_bytes(path: &str) -> Result<u64> {
    let out = Command::new("df")
        .args(["--output=avail", "-B1", path])
        .output()
        .context("df")?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().next())
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| anyhow!("could not parse df output"))
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T"];
    if n < 1024 {
        return format!("{n}B");
    }
    let mut v = n as f64;
    let mut idx = 0;
    while v >= 1024.0 && idx < UNITS.len() - 1 {
        v /= 1024.0;
        idx += 1;
    }
    if v >= 100.0 {
        format!("{:.0}{}", v, UNITS[idx])
    } else if v >= 10.0 {
        format!("{:.1}{}", v, UNITS[idx])
    } else {
        format!("{:.2}{}", v, UNITS[idx])
    }
}

fn waybar_out(threshold: u64) -> Result<()> {
    let free = free_bytes("/").unwrap_or(u64::MAX);
    if free >= threshold {
        // Empty text → CSS rule collapses the tile (style.css adds zero
        // padding to `.hidden`).
        println!(r#"{{"text":"","class":"hidden","tooltip":""}}"#);
        return Ok(());
    }
    let class = if free < threshold / 2 { "critical" } else { "warning" };
    let text = format!("{GLYPH_DISK} {}", human_bytes(free));
    let tooltip = format!(
        "{} free / threshold {}\\nclick: cleanup",
        human_bytes(free),
        human_bytes(threshold)
    );
    println!(r#"{{"text":"{text}","class":"{class}","tooltip":"{tooltip}"}}"#);
    Ok(())
}

fn menu(threshold: u64) -> Result<()> {
    let free_before = free_bytes("/").unwrap_or(0);
    let strats = strategies::registered();
    let mut probes: Vec<(String, Probe)> = Vec::new();
    for s in &strats {
        if !s.available() {
            continue;
        }
        let p = s.probe().unwrap_or_default();
        probes.push((s.id().to_string(), p));
    }

    if probes.is_empty() {
        crate::wifi::notify("disk", "no cleaning strategies available");
        return Ok(());
    }

    let order = ReclaimableRanker.rank(&probes);

    // Build menu rows in ranked order; remember the strategy index so we
    // can apply without rebuilding the registered list.
    let mut rows: Vec<(String, usize, Probe)> = Vec::new();
    for id in order {
        let Some(idx) = strats.iter().position(|s| s.id() == id) else {
            continue;
        };
        let Some((_, p)) = probes.iter().find(|(i, _)| i == id) else {
            continue;
        };
        let s = &strats[idx];
        let hint = p.items.first().map(|x| x.as_str()).unwrap_or("");
        let line = if hint.is_empty() {
            format!(
                "{:<20} {:>8}  {}",
                s.label(),
                human_bytes(p.reclaimable),
                s.description()
            )
        } else {
            format!(
                "{:<20} {:>8}  {} — {}",
                s.label(),
                human_bytes(p.reclaimable),
                s.description(),
                hint
            )
        };
        rows.push((line, idx, p.clone()));
    }

    let input = rows
        .iter()
        .map(|(l, _, _)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "disk » free {} / threshold {} » ",
        human_bytes(free_before),
        human_bytes(threshold)
    );
    let choice = crate::wifi::run_fuzzel(&input, &prompt, false)?;
    let choice = choice.trim_end_matches('\n');
    if choice.is_empty() {
        return Ok(());
    }
    let Some((_, idx, probe)) = rows.into_iter().find(|(l, _, _)| l == choice) else {
        return Ok(());
    };
    let strat = &strats[idx];

    let outcome = strat.apply(&probe).unwrap_or_default();
    let free_after = free_bytes("/").unwrap_or(0);
    log_history(strat.id(), &probe, &outcome, free_before, free_after);

    let log_tail = outcome.log.first().map(|s| s.as_str()).unwrap_or("");
    let head = format!(
        "{}: reclaimed {} (free {} → {})",
        strat.label(),
        human_bytes(outcome.reclaimed),
        human_bytes(free_before),
        human_bytes(free_after)
    );
    let body = if log_tail.is_empty() {
        head
    } else {
        format!("{head}\n{log_tail}")
    };
    crate::wifi::notify("disk", &body);
    refresh_waybar();
    Ok(())
}

fn log_history(id: &str, probe: &Probe, outcome: &Outcome, before: u64, after: u64) {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let dir = PathBuf::from(home).join(".local/share/sy");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("disk-history.jsonl");
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = serde_json::json!({
        "ts": ts,
        "strategy": id,
        "probe_bytes": probe.reclaimable,
        "probe_items": probe.items,
        "reclaimed_bytes": outcome.reclaimed,
        "log": outcome.log,
        "free_before": before,
        "free_after": after,
    });
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    use std::io::Write;
    let _ = writeln!(f, "{}", entry);
}

fn refresh_waybar() {
    let _ = Command::new("sh")
        .arg("-c")
        .arg("pkill -RTMIN+13 waybar 2>/dev/null")
        .status();
}
