//! Notifications: dbus-watcher daemon, bar JSON, persistent per-day log,
//! and a 2-pane TUI reader.
//!
//! Storage layout (XDG data):
//!   ~/.local/share/sy/notifications/YYYY/MM/DD/notifications.json
//! one JSON array per day, each entry:
//!   {"id":"uuid","ts":<unix>, "app","icon","summary","body","read":bool}
//!
//! Bar count = number of unread entries across the most recent 30 days.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{sound, wifi};

const SCAN_DAYS: u32 = 30;
const NF_BELL: &str = "\u{F0F3}"; // fa-bell

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub ts: u64,
    pub app: String,
    #[serde(default)]
    pub icon: String,
    pub summary: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub read: bool,
}

pub fn run(action: &str, rest: &[String]) -> Result<()> {
    match action {
        "watch" => watch(),
        "waybar" => waybar_out(),
        "count" => {
            println!("{}", unread_count());
            Ok(())
        }
        "clear" => clear(),
        "menu" => menu(),
        "list" => list_cmd(rest),
        "show" => {
            let id = rest.first().ok_or_else(|| anyhow!("show: missing id"))?;
            show(id)
        }
        "read" => {
            let id = rest.first().ok_or_else(|| anyhow!("read: missing id"))?;
            mark_read(id, true)?;
            refresh_waybar();
            Ok(())
        }
        _ => Err(anyhow!(
            "unknown notif action: {action} (expected watch|waybar|count|clear|menu|list|show|read)"
        )),
    }
}

// -- storage paths ----------------------------------------------------------

fn data_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/share/sy/notifications")
}

fn day_dir(ts: u64) -> PathBuf {
    let (y, m, d) = ts_to_ymd(ts);
    data_root().join(format!("{y:04}")).join(format!("{m:02}")).join(format!("{d:02}"))
}

fn day_file(ts: u64) -> PathBuf {
    day_dir(ts).join("notifications.json")
}

// -- date helpers (no chrono dep needed for this) ---------------------------

fn ts_to_ymd(ts: u64) -> (i32, u32, u32) {
    // Use `date(1)` for locale-correct day boundaries (handles DST etc.).
    let out = Command::new("date")
        .args(["-d", &format!("@{ts}"), "+%Y %m %d"])
        .output()
        .ok();
    if let Some(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout);
            let mut it = s.split_whitespace();
            let y = it.next().and_then(|x| x.parse().ok()).unwrap_or(1970);
            let m = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
            let d = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
            return (y, m, d);
        }
    }
    (1970, 1, 1)
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// -- IO ---------------------------------------------------------------------

fn read_day(path: &Path) -> Vec<Record> {
    let Ok(s) = fs::read_to_string(path) else {
        return Vec::new();
    };
    if s.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str(&s).unwrap_or_default()
}

fn write_day(path: &Path, items: &[Record]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let s = serde_json::to_string_pretty(items)?;
    fs::write(path, s).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn append_record(rec: Record) -> Result<()> {
    let path = day_file(rec.ts);
    let mut items = read_day(&path);
    items.push(rec);
    write_day(&path, &items)
}

// -- aggregation ------------------------------------------------------------

/// Walk the last `days` days of notification files, newest first.
fn list_recent(days: u32) -> Vec<Record> {
    let mut out = Vec::new();
    let now = now_ts();
    for back in 0..days {
        let ts = now.saturating_sub((back as u64) * 86400);
        let p = day_file(ts);
        let mut items = read_day(&p);
        // newest within the day first
        items.sort_by_key(|r| std::cmp::Reverse(r.ts));
        out.extend(items);
    }
    out
}

fn unread_count() -> usize {
    list_recent(SCAN_DAYS).into_iter().filter(|r| !r.read).count()
}

// -- waybar -----------------------------------------------------------------

fn waybar_out() -> Result<()> {
    let n = unread_count();
    let class = if n == 0 { "zero" } else { "has" };
    let text = format!("{NF_BELL} {n}");
    let tooltip = format!(
        "{n} unread notification{}\\nleft-click: open · right-click: clear all",
        if n == 1 { "" } else { "s" }
    );
    println!(r#"{{"text":"{text}","class":"{class}","tooltip":"{tooltip}"}}"#);
    Ok(())
}

// -- mutations --------------------------------------------------------------

fn clear() -> Result<()> {
    // Mark every record in the last SCAN_DAYS days as read; dismiss any
    // active mako popups too so the screen stays in sync.
    let now = now_ts();
    for back in 0..SCAN_DAYS {
        let ts = now.saturating_sub((back as u64) * 86400);
        let p = day_file(ts);
        let mut items = read_day(&p);
        let mut changed = false;
        for r in items.iter_mut() {
            if !r.read {
                r.read = true;
                changed = true;
            }
        }
        if changed {
            write_day(&p, &items)?;
        }
    }
    let _ = Command::new("makoctl").args(["dismiss", "--all"]).status();
    refresh_waybar();
    Ok(())
}

fn mark_read(id: &str, value: bool) -> Result<()> {
    let now = now_ts();
    for back in 0..SCAN_DAYS {
        let ts = now.saturating_sub((back as u64) * 86400);
        let p = day_file(ts);
        let mut items = read_day(&p);
        let mut hit = false;
        for r in items.iter_mut() {
            if r.id == id {
                r.read = value;
                hit = true;
                break;
            }
        }
        if hit {
            write_day(&p, &items)?;
            return Ok(());
        }
    }
    Ok(())
}

// -- read helpers -----------------------------------------------------------

fn show(id: &str) -> Result<()> {
    let recs = list_recent(SCAN_DAYS);
    let Some(r) = recs.into_iter().find(|r| r.id == id) else {
        return Err(anyhow!("notification not found: {id}"));
    };
    print!("{}", format_one(&r));
    Ok(())
}

fn format_one(r: &Record) -> String {
    let when = format_ts(r.ts);
    let read = if r.read { "read" } else { "unread" };
    let mut s = String::new();
    s.push_str(&format!("{}\n", r.summary));
    s.push_str(&format!("{}  ·  {}  ·  {}\n", r.app, when, read));
    s.push_str("\n");
    if r.body.is_empty() {
        s.push_str("(no body)\n");
    } else {
        s.push_str(&r.body);
        if !r.body.ends_with('\n') {
            s.push('\n');
        }
    }
    s
}

fn format_ts(ts: u64) -> String {
    let out = Command::new("date")
        .args(["-d", &format!("@{ts}"), "+%Y-%m-%d %H:%M"])
        .output()
        .ok();
    if let Some(o) = out {
        if o.status.success() {
            return String::from_utf8_lossy(&o.stdout).trim().to_string();
        }
    }
    ts.to_string()
}

fn list_cmd(args: &[String]) -> Result<()> {
    let json = args.iter().any(|a| a == "--json");
    let limit: usize = args
        .windows(2)
        .find(|w| w[0] == "--limit")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(usize::MAX);
    let days: u32 = args
        .windows(2)
        .find(|w| w[0] == "--days")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(SCAN_DAYS);

    let mut recs = list_recent(days);
    recs.truncate(limit);

    if json {
        let s = serde_json::to_string(&recs)?;
        println!("{s}");
    } else {
        for r in recs {
            let mark = if r.read { ' ' } else { '●' };
            println!(
                "{mark} {}  {:<12}  {}",
                format_ts(r.ts),
                truncate(&r.app, 12),
                truncate(&r.summary, 60)
            );
        }
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// -- watch (dbus-monitor) ---------------------------------------------------

fn watch() -> Result<()> {
    let mut child = Command::new("dbus-monitor")
        .args([
            "--session",
            "interface='org.freedesktop.Notifications',member='Notify'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let rdr = BufReader::new(stdout);

    let mut pending: Option<Pending> = None;
    for line in rdr.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if pending.is_none() {
            if line.starts_with("method call") && line.contains("member=Notify") {
                pending = Some(Pending::default());
            }
            continue;
        }
        let p = pending.as_mut().unwrap();
        let Some(s) = parse_string_arg(&line) else {
            continue;
        };
        // Collect the first four string args after the Notify header:
        // app_name, app_icon, summary, body. Anything later (action labels,
        // hint dict values) is ignored because we've already moved on.
        match p.state {
            0 => {
                p.app = s;
                p.state = 1;
            }
            1 => {
                p.icon = s;
                p.state = 2;
            }
            2 => {
                p.summary = s;
                p.state = 3;
            }
            3 => {
                p.body = s;
                let p = pending.take().unwrap();
                if p.app != "sy" {
                    on_new_notification(p);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Default)]
struct Pending {
    app: String,
    icon: String,
    summary: String,
    body: String,
    state: u8,
}

fn parse_string_arg(line: &str) -> Option<String> {
    let l = line.trim();
    let rest = l.strip_prefix("string \"")?;
    let inner = rest.strip_suffix('"')?;
    Some(inner.to_string())
}

fn on_new_notification(p: Pending) {
    let rec = Record {
        id: uuid::Uuid::new_v4().to_string(),
        ts: now_ts(),
        app: p.app,
        icon: p.icon,
        summary: p.summary,
        body: p.body,
        read: false,
    };
    let _ = append_record(rec);
    let _ = sound::blip(1200.0, 25);
    thread::sleep(Duration::from_millis(30));
    let _ = sound::blip(1200.0, 25);
    refresh_waybar();
}

fn refresh_waybar() {
    let _ = Command::new("sh")
        .arg("-c")
        .arg("pkill -RTMIN+8 waybar 2>/dev/null")
        .status();
}

// -- fuzzel menu ------------------------------------------------------------
//
// One overlay, one click — same UX shape as `sy bt`, `sy pwr`, `sy net`.
// Each notification row carries app, summary and a one-line body preview;
// selecting a row marks it as read. A `clear all` header marks every
// recent record as read in one shot.

enum MenuAction {
    Noop,
    Clear,
    MarkRead(String),
}

fn menu() -> Result<()> {
    let recs = list_recent(SCAN_DAYS);
    let unread = recs.iter().filter(|r| !r.read).count();

    let mut rows: Vec<(String, MenuAction)> = Vec::new();
    rows.push((
        format!("  clear all   ({unread} unread / {} total)", recs.len()),
        MenuAction::Clear,
    ));
    rows.push(("  ─────────────".into(), MenuAction::Noop));
    if recs.is_empty() {
        rows.push(("  (no notifications)".into(), MenuAction::Noop));
    } else {
        for r in &recs {
            let mark = if r.read { ' ' } else { '●' };
            let preview = r.body.replace('\n', " ");
            let line = format!(
                "{} {:<12} │ {:<40} │ {}",
                mark,
                truncate(&r.app, 12),
                truncate(&r.summary, 40),
                truncate(&preview, 60)
            );
            rows.push((line, MenuAction::MarkRead(r.id.clone())));
        }
    }

    let input = rows
        .iter()
        .map(|(l, _)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let choice = wifi::run_fuzzel(&input, "notif » ", false)?;
    let choice = choice.trim_end_matches('\n');
    if choice.is_empty() {
        return Ok(());
    }
    let Some((_, action)) = rows.into_iter().find(|(l, _)| l == choice) else {
        return Ok(());
    };
    match action {
        MenuAction::Noop => Ok(()),
        MenuAction::Clear => clear(),
        MenuAction::MarkRead(id) => {
            mark_read(&id, true)?;
            refresh_waybar();
            Ok(())
        }
    }
}
