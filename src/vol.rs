use std::process::Command;

use anyhow::{anyhow, Result};

use crate::{sound, wifi};

const SINK: &str = "@DEFAULT_AUDIO_SINK@";
const SOURCE: &str = "@DEFAULT_AUDIO_SOURCE@";

// Font Awesome volume glyphs (JetBrainsMono Nerd Font).
const VOL_MUTE: &str = "\u{F026}";
const VOL_DOWN: &str = "\u{F027}";
const VOL_UP: &str = "\u{F028}";

pub fn run(action: Option<&str>, waybar: bool) -> Result<()> {
    if waybar {
        return waybar_out();
    }
    let Some(a) = action else {
        return Err(anyhow!(
            "missing action (expected up|down|mute|mic-mute|pick) — pass --waybar for bar JSON"
        ));
    };
    match a {
        "up" => step("5%+", 988.0),
        "down" => step("5%-", 494.0),
        "mute" => toggle(SINK, "Volume"),
        "mic-mute" => toggle(SOURCE, "Mic"),
        "pick" | "sinks" | "output" => pick_sink(),
        _ => Err(anyhow!(
            "unknown vol action: {a} (expected up|down|mute|mic-mute|pick)"
        )),
    }
}

fn waybar_out() -> Result<()> {
    let (vol, muted) = read_volume(SINK);
    let pct = (vol * 100.0).round() as i32;
    let class = if muted {
        "muted"
    } else if pct <= 33 {
        "low"
    } else if pct <= 66 {
        "mid"
    } else {
        "high"
    };
    let glyph = if muted {
        VOL_MUTE
    } else if pct <= 50 {
        VOL_DOWN
    } else {
        VOL_UP
    };
    let tooltip = if muted {
        format!("volume: muted ({pct}%)")
    } else {
        format!("volume: {pct}%")
    };
    println!(r#"{{"text":"{glyph}","class":"{class}","tooltip":"{tooltip}","alt":"{pct}"}}"#);
    Ok(())
}

fn read_volume(sink: &str) -> (f32, bool) {
    let out = match Command::new("wpctl").args(["get-volume", sink]).output() {
        Ok(o) => o,
        Err(_) => return (0.0, false),
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let muted = s.contains("MUTED");
    let vol: f32 = s
        .split_whitespace()
        .nth(1)
        .and_then(|t| t.parse().ok())
        .unwrap_or(0.0);
    (vol, muted)
}

fn refresh_waybar() {
    let _ = Command::new("sh")
        .arg("-c")
        .arg("pkill -RTMIN+11 waybar 2>/dev/null")
        .status();
}

fn step(delta: &str, beep_hz: f32) -> Result<()> {
    let _ = Command::new("wpctl").args(["set-mute", SINK, "0"]).status();
    Command::new("wpctl")
        .args(["set-volume", SINK, delta, "-l", "1.0"])
        .status()?;
    let _ = sound::blip(beep_hz, 45);
    notify(SINK, "Volume")
}

fn toggle(sink: &str, label: &str) -> Result<()> {
    Command::new("wpctl").args(["set-mute", sink, "toggle"]).status()?;
    notify(sink, label)
}

fn notify(sink: &str, label: &str) -> Result<()> {
    let out = Command::new("wpctl").args(["get-volume", sink]).output()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let muted = s.contains("MUTED");
    let vol: f32 = s
        .split_whitespace()
        .nth(1)
        .and_then(|t| t.parse().ok())
        .unwrap_or(0.0);
    let pct = (vol * 100.0).round() as i32;
    let body = if muted {
        format!("{label}: muted")
    } else {
        format!("{label}: {pct}%")
    };

    let _ = Command::new("notify-send")
        .args([
            "-a",
            "sy",
            "-t",
            "800",
            "-h",
            &format!("string:x-canonical-private-synchronous:sy-{}", label.to_lowercase()),
            "-h",
            &format!("int:value:{}", pct),
            &body,
        ])
        .status();
    refresh_waybar();
    Ok(())
}

struct SinkInfo {
    name: String,
    description: String,
    default: bool,
}

fn list_sinks() -> Vec<SinkInfo> {
    // Force LC_ALL=C: pactl localizes "Sink #", "Name:", "Description:" headers,
    // and we key parsing off the English strings.
    let default_name = Command::new("pactl")
        .env("LC_ALL", "C")
        .arg("get-default-sink")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let Some(out) = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(["list", "sinks"])
        .output()
        .ok()
    else {
        return Vec::new();
    };
    let s = String::from_utf8_lossy(&out.stdout);

    let mut sinks: Vec<SinkInfo> = Vec::new();
    let mut name = String::new();
    let mut desc = String::new();
    let mut in_block = false;
    let flush = |sinks: &mut Vec<SinkInfo>, n: &mut String, d: &mut String| {
        if !n.is_empty() {
            let default = *n == default_name;
            sinks.push(SinkInfo {
                name: std::mem::take(n),
                description: std::mem::take(d),
                default,
            });
        } else {
            n.clear();
            d.clear();
        }
    };
    for line in s.lines() {
        if line.starts_with("Sink #") {
            if in_block {
                flush(&mut sinks, &mut name, &mut desc);
            }
            in_block = true;
            continue;
        }
        if !in_block {
            continue;
        }
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("Name: ") {
            name = rest.to_string();
        } else if let Some(rest) = l.strip_prefix("Description: ") {
            desc = rest.to_string();
        }
    }
    if in_block {
        flush(&mut sinks, &mut name, &mut desc);
    }
    sinks
}

fn pick_sink() -> Result<()> {
    let sinks = list_sinks();
    if sinks.is_empty() {
        notify_text("vol", "no audio outputs found");
        return Ok(());
    }

    let lines: Vec<String> = sinks
        .iter()
        .map(|s| {
            let mark = if s.default { "*" } else { " " };
            let label = if s.description.is_empty() {
                &s.name
            } else {
                &s.description
            };
            format!("{mark} {label}")
        })
        .collect();

    let choice = wifi::run_fuzzel(&lines.join("\n"), "output » ", false)?;
    let choice = choice.trim_end_matches('\n');
    if choice.trim().is_empty() {
        return Ok(());
    }

    let Some(idx) = lines.iter().position(|l| l == choice) else {
        notify_text("vol", "could not resolve picked output");
        return Ok(());
    };
    let picked = &sinks[idx];

    // Silent-hours guard: warn (don't block) when picking a loud output.
    if crate::silent::is_active() && !crate::silent::sink_is_quiet(&picked.name) {
        let label = if picked.description.is_empty() {
            &picked.name
        } else {
            &picked.description
        };
        wifi::notify(
            "silent",
            &format!("loud output picked while silent mode is on: {label}"),
        );
    }

    let ok = Command::new("pactl")
        .args(["set-default-sink", &picked.name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if ok {
        move_streams_to(&picked.name);
    }

    let label = if picked.description.is_empty() {
        &picked.name
    } else {
        &picked.description
    };
    let body = if ok {
        format!("output: {label}")
    } else {
        format!("output: {label} (failed)")
    };
    notify_text("vol", &body);
    Ok(())
}

/// Move every existing playback stream onto `sink` so the switch is felt
/// immediately by apps that opened on the previous default.
fn move_streams_to(sink: &str) {
    let Some(out) = Command::new("pactl")
        .args(["list", "short", "sink-inputs"])
        .output()
        .ok()
    else {
        return;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some(id) = line.split_whitespace().next() else {
            continue;
        };
        let _ = Command::new("pactl")
            .args(["move-sink-input", id, sink])
            .status();
    }
}

fn notify_text(summary: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "sy", "-t", "1500", summary, body])
        .status();
}
