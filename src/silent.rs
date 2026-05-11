//! Silent hours — quiet-output mode active during a configurable time
//! window (default 23:00 → 10:00, overnight). When active:
//!   - waybar shows a moon glyph
//!   - vol::pick warns if the user picks a loud sink
//!   - a daemon watches pactl events and warns on auto-switch to loud
//!   - tuned `performance` is dropped to `balanced` once on enable
//!
//! Override semantics (`~/.cache/sy/silent-state`):
//!   `on`  → forced active regardless of the clock
//!   `off` → forced inactive
//!   absent / empty / `auto` → follow the configured window
//!
//! Toggle flips the override to whichever value contradicts the current
//! effective state. To return to time-driven mode use `sy silent auto`.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::wifi;

// fa-moon-o (JetBrainsMono Nerd Font, U+F186) — crescent moon.
const GLYPH_MOON: &str = "\u{F186}";

#[derive(Deserialize, Debug, Clone)]
struct WindowCfg {
    #[serde(default = "default_start")]
    start: String,
    #[serde(default = "default_end")]
    end: String,
}

impl Default for WindowCfg {
    fn default() -> Self {
        Self {
            start: default_start(),
            end: default_end(),
        }
    }
}

fn default_start() -> String {
    "23:00".into()
}
fn default_end() -> String {
    "10:00".into()
}

pub fn run(action: Option<&str>, waybar: bool) -> Result<()> {
    if waybar {
        return waybar_out();
    }
    match action.unwrap_or("toggle") {
        "toggle" => toggle(),
        "enable" | "on" => set_state("on", true),
        "disable" | "off" => set_state("off", true),
        "auto" => set_state("auto", true),
        "status" => status_text(),
        "watch" => watch(),
        other => Err(anyhow!(
            "unknown silent action: {other} (toggle|enable|disable|auto|status|watch)"
        )),
    }
}

// -- paths ----------------------------------------------------------------

fn cfg_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/sy/silent.toml")
}

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = PathBuf::from(home).join(".cache/sy");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn state_path() -> PathBuf {
    cache_dir().join("silent-state")
}

fn last_path() -> PathBuf {
    cache_dir().join("silent-last")
}

// -- config loading -------------------------------------------------------

fn load_cfg() -> WindowCfg {
    let mut cfg = match fs::read_to_string(cfg_path()) {
        Ok(s) => toml::from_str::<WindowCfg>(&s).unwrap_or_default(),
        Err(_) => WindowCfg::default(),
    };
    if let Ok(s) = std::env::var("SY_SILENT_START") {
        cfg.start = s;
    }
    if let Ok(s) = std::env::var("SY_SILENT_END") {
        cfg.end = s;
    }
    cfg
}

fn read_state() -> String {
    fs::read_to_string(state_path())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn write_state(s: &str) -> Result<()> {
    let p = state_path();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    fs::write(&p, format!("{s}\n")).with_context(|| format!("write {}", p.display()))?;
    Ok(())
}

// -- time -----------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct HM {
    h: u8,
    m: u8,
}
impl HM {
    fn parse(s: &str) -> Option<Self> {
        let mut it = s.trim().splitn(2, ':');
        let h: u8 = it.next()?.parse().ok()?;
        let m: u8 = it.next().unwrap_or("0").parse().ok()?;
        if h > 23 || m > 59 {
            return None;
        }
        Some(Self { h, m })
    }
    fn minutes(&self) -> u32 {
        self.h as u32 * 60 + self.m as u32
    }
}

fn now_hm() -> HM {
    let out = Command::new("date").arg("+%H %M").output().ok();
    if let Some(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout);
            let mut it = s.split_whitespace();
            let h: u8 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let m: u8 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            return HM { h, m };
        }
    }
    HM { h: 0, m: 0 }
}

fn in_window(now: HM, start: HM, end: HM) -> bool {
    let n = now.minutes();
    let s = start.minutes();
    let e = end.minutes();
    if s <= e {
        s <= n && n < e
    } else {
        // wraps midnight
        n >= s || n < e
    }
}

// -- effective state ------------------------------------------------------

pub fn is_active() -> bool {
    effective(&load_cfg(), now_hm(), &read_state())
}

fn effective(cfg: &WindowCfg, now: HM, override_s: &str) -> bool {
    match override_s {
        "on" => true,
        "off" => false,
        _ => {
            let Some(start) = HM::parse(&cfg.start) else {
                return false;
            };
            let Some(end) = HM::parse(&cfg.end) else {
                return false;
            };
            in_window(now, start, end)
        }
    }
}

// -- mutations ------------------------------------------------------------

fn toggle() -> Result<()> {
    let cur = is_active();
    set_state(if cur { "off" } else { "on" }, true)
}

fn set_state(new_state: &str, side_effects: bool) -> Result<()> {
    let was_active = is_active();
    write_state(new_state)?;
    let now_active = is_active();
    if side_effects {
        let _ = fs::write(last_path(), if now_active { "on\n" } else { "off\n" });
        if was_active != now_active {
            announce_transition(now_active);
        }
        refresh_waybar();
    }
    Ok(())
}

/// Emit the single state-transition notification, applying enter-side
/// effects (pwr → balanced unless the user pinned perf, loud-sink hint)
/// along the way.
fn announce_transition(now_active: bool) {
    let mut msg = String::from(if now_active { "on" } else { "off" });
    if now_active {
        if crate::pwr::set_balanced_if_performance() {
            msg.push_str(" · pwr→balanced");
        } else if crate::pwr::read_pin().as_deref() == Some("perf") {
            msg.push_str(" · pwr pinned: perf");
        }
        if let Some(name) = current_default_sink() {
            if !sink_is_quiet(&name) {
                msg.push_str(" · loud output");
            }
        }
    }
    wifi::notify("silent mode", &msg);
}

// -- sink classification --------------------------------------------------

pub fn sink_is_quiet(sink_name: &str) -> bool {
    if sink_name.starts_with("bluez_output.") {
        return true;
    }
    let Some(port) = active_port_for(sink_name) else {
        return false;
    };
    let map = port_type_map();
    matches!(
        map.get(&port).map(String::as_str),
        Some("headphones") | Some("headset")
    )
}

fn current_default_sink() -> Option<String> {
    let out = Command::new("pactl")
        .env("LC_ALL", "C")
        .arg("get-default-sink")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn active_port_for(sink_name: &str) -> Option<String> {
    let out = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(["list", "sinks"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let mut name = String::new();
    let mut active_port: Option<String> = None;
    for line in s.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("Name: ") {
            // flush previous block
            if name == sink_name {
                if let Some(p) = active_port {
                    return Some(p);
                }
            }
            name = rest.trim().to_string();
            active_port = None;
        } else if let Some(rest) = t.strip_prefix("Active Port: ") {
            active_port = Some(rest.trim().to_string());
        }
    }
    if name == sink_name {
        active_port
    } else {
        None
    }
}

/// Build a {port_name → port.type} map by parsing `pactl list cards`.
/// Port headers sit at 2-tab indent under each card's `Ports:` section;
/// the property `port.type = "..."` follows under `Properties:`.
fn port_type_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(["list", "cards"])
        .output()
    else {
        return map;
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<String> = None;
    for line in s.lines() {
        let tabs = line.chars().take_while(|c| *c == '\t').count();
        let trimmed = line.trim_start();
        if tabs == 2 {
            // potential port header — `<kebab-name>: <description>`. Profiles
            // also live at this indent but their names contain ':' (e.g.
            // `output:hdmi-stereo`); skip those.
            if let Some((name, _)) = trimmed.split_once(": ") {
                if !name.is_empty()
                    && !name.contains(':')
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                {
                    current = Some(name.to_string());
                    continue;
                }
            }
        }
        if let Some(c) = &current {
            if let Some(rest) = trimmed.strip_prefix("port.type = \"") {
                if let Some(end) = rest.find('"') {
                    map.insert(c.clone(), rest[..end].to_string());
                }
            }
        }
    }
    map
}

// -- status / waybar ------------------------------------------------------

fn status_text() -> Result<()> {
    let cfg = load_cfg();
    let override_s = read_state();
    let now = now_hm();
    let active = effective(&cfg, now, &override_s);
    let ov = if override_s.is_empty() {
        "auto"
    } else {
        override_s.as_str()
    };
    println!("active:   {active}");
    println!("override: {ov}");
    println!("window:   {} -> {}", cfg.start, cfg.end);
    if let Some(sink) = current_default_sink() {
        let q = sink_is_quiet(&sink);
        println!("sink:     {sink} ({})", if q { "quiet" } else { "loud" });
    }
    if let Some(pin) = crate::pwr::read_pin() {
        println!("pwr-pin:  {pin}");
    }
    Ok(())
}

fn waybar_out() -> Result<()> {
    let cfg = load_cfg();
    let override_s = read_state();
    let now = now_hm();
    let active = effective(&cfg, now, &override_s);
    if !active {
        println!(r#"{{"text":"","class":"hidden","tooltip":""}}"#);
        return Ok(());
    }
    let scope = match override_s.as_str() {
        "on" => "manual override".to_string(),
        _ => format!("auto window {}–{}", cfg.start, cfg.end),
    };
    let tooltip = format!("silent mode ({scope})\\nclick: toggle");
    println!(
        r#"{{"text":"{GLYPH_MOON}","class":"active","tooltip":"{tooltip}"}}"#
    );
    Ok(())
}

// -- daemon ---------------------------------------------------------------

fn watch() -> Result<()> {
    if !crate::which("pactl") {
        return Err(anyhow!("pactl not on PATH — silent watcher needs it"));
    }

    let (tx, rx) = mpsc::channel::<()>();

    // pactl subscribe → channel pings on relevant events.
    let tx_pactl = tx.clone();
    let mut pactl = Command::new("pactl")
        .env("LC_ALL", "C")
        .arg("subscribe")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn pactl subscribe")?;
    let stdout = pactl
        .stdout
        .take()
        .ok_or_else(|| anyhow!("pactl: no stdout"))?;
    thread::spawn(move || {
        let rdr = BufReader::new(stdout);
        for line in rdr.lines().flatten() {
            let l = line.trim();
            if l.contains("on server") || l.contains("on sink") {
                let _ = tx_pactl.send(());
            }
        }
    });

    // Clock tick — fires window transitions even when pactl is silent.
    let tx_tick = tx;
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(60));
        if tx_tick.send(()).is_err() {
            break;
        }
    });

    // Initial evaluation so we don't miss a state at startup.
    let mut last_loud: Option<String> = None;
    handle_tick(&mut last_loud);

    while rx.recv().is_ok() {
        handle_tick(&mut last_loud);
    }

    let _ = pactl.kill();
    Ok(())
}

fn handle_tick(last_loud: &mut Option<String>) {
    let cfg = load_cfg();
    let override_s = read_state();
    let now = now_hm();
    let now_active = effective(&cfg, now, &override_s);

    let last_active = fs::read_to_string(last_path())
        .map(|s| s.trim() == "on")
        .unwrap_or(false);

    if now_active != last_active {
        let _ = fs::write(last_path(), if now_active { "on\n" } else { "off\n" });
        announce_transition(now_active);
        refresh_waybar();
    }

    if now_active {
        if let Some(sink) = current_default_sink() {
            if !sink_is_quiet(&sink) {
                if last_loud.as_deref() != Some(sink.as_str()) {
                    wifi::notify("silent", &format!("loud output: {sink}"));
                    *last_loud = Some(sink);
                }
            } else {
                *last_loud = None;
            }
        }
    } else {
        *last_loud = None;
    }
}

fn refresh_waybar() {
    let _ = Command::new("sh")
        .arg("-c")
        .arg("pkill -RTMIN+14 waybar 2>/dev/null")
        .status();
}
