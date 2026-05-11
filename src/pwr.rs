//! Power button: fuzzel menu for tuned profiles + lock/suspend/reboot/shutdown/logout.
//! Uses Fedora's `tuned-adm` (polkit-managed, no sudo prompt for the current user).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;

use crate::{sound, wifi};

// `~/.cache/sy/pwr-pinned` records the last profile the user picked through
// the `sy pwr` menu. Silent mode reads this to avoid fighting an explicit
// user choice — see `set_balanced_if_performance`.
fn pin_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".cache/sy");
    let _ = fs::create_dir_all(&dir);
    Some(dir.join("pwr-pinned"))
}

pub fn read_pin() -> Option<String> {
    let path = pin_path()?;
    let s = fs::read_to_string(&path).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn write_pin(short: &str) {
    let Some(path) = pin_path() else {
        return;
    };
    let _ = fs::write(&path, format!("{short}\n"));
}

/// Profile entries: (short label shown on the bar, full label, tuned name).
/// The short label is `perf`/`bal`/`pwsv`; the full label is used in
/// tooltips and the menu.
const PROFILES: &[(&str, &str, &str)] = &[
    ("perf", "performance", "throughput-performance"),
    ("bal", "balanced", "balanced"),
    ("pwsv", "powersave", "powersave"),
];

pub fn run(waybar: bool) -> Result<()> {
    if waybar {
        return waybar_out();
    }
    menu()
}

fn waybar_out() -> Result<()> {
    use std::io::Write;
    let active = current_profile();
    let (short, full) = profile_label(&active);
    let class = match full {
        "performance" => "performance",
        "powersave" => "powersave",
        _ => "balanced",
    };
    let tooltip = format!("power profile: {active}\\nclick: power menu");
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        r#"{{"text":"{short}","tooltip":"{tooltip}","class":"{class}","alt":"{full}"}}"#
    )?;
    out.flush()?;
    Ok(())
}

fn menu() -> Result<()> {
    let active = current_profile();
    let (_, active_full) = profile_label(&active);

    let mut items: Vec<(String, Action)> = Vec::new();
    for (_short, full, _) in PROFILES {
        let mark = if *full == active_full { "*" } else { " " };
        items.push((
            format!("{mark} profile  {full}"),
            Action::SetProfile(full),
        ));
    }
    items.push(("  ───────────────".into(), Action::Noop));
    items.push(("  lock".into(), Action::Lock));
    items.push(("  suspend".into(), Action::Suspend));
    items.push(("  reboot".into(), Action::Reboot));
    items.push(("  shutdown".into(), Action::Shutdown));
    items.push(("  logout (niri quit)".into(), Action::Logout));

    let input: String = items
        .iter()
        .map(|(l, _)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let choice = wifi::run_fuzzel(&input, "pwr » ", false)?;
    let choice = choice.trim_end_matches('\n');
    if choice.is_empty() {
        return Ok(());
    }
    let Some((_, act)) = items.into_iter().find(|(l, _)| l == choice) else {
        return Ok(());
    };

    match act {
        Action::Noop => Ok(()),
        Action::SetProfile(label) => set_profile(label),
        Action::Lock => spawn_detached("swaylock", &["-f"]),
        Action::Suspend => confirm_then("suspend", || run_status("systemctl", &["suspend"])),
        Action::Reboot => confirm_then("reboot", || run_status("systemctl", &["reboot"])),
        Action::Shutdown => confirm_then("shutdown", || {
            let _ = sound::logout();
            run_status("systemctl", &["poweroff"])
        }),
        Action::Logout => confirm_then("logout", || {
            let _ = sound::logout();
            run_status("niri", &["msg", "action", "quit", "--skip-confirmation"])
        }),
    }
}

enum Action {
    Noop,
    SetProfile(&'static str),
    Lock,
    Suspend,
    Reboot,
    Shutdown,
    Logout,
}

fn current_profile() -> String {
    let out = Command::new("tuned-adm").arg("active").output().ok();
    let Some(o) = out else {
        return String::new();
    };
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .find_map(|l| l.split_once(": ").map(|(_, v)| v.trim().to_string()))
        .unwrap_or_default()
}

/// Map a tuned profile name to (short, full) labels.
fn profile_label(active: &str) -> (&'static str, &'static str) {
    for (short, full, tuned) in PROFILES {
        if active == *tuned {
            return (short, full);
        }
    }
    // Fallbacks for tuned profiles we don't expose directly.
    if active.contains("performance") {
        ("perf", "performance")
    } else if active.contains("powersave") {
        ("pwsv", "powersave")
    } else {
        ("bal", "balanced")
    }
}

/// If the active tuned profile is `throughput-performance` AND the user
/// hasn't explicitly pinned `perf` via `sy pwr`, switch to `balanced` and
/// refresh the bar. Returns true when a change was made. Used by silent
/// mode to keep fans quiet during quiet hours without fighting a user who
/// has deliberately asked for performance.
pub fn set_balanced_if_performance() -> bool {
    let active = current_profile();
    if !active.contains("performance") {
        return false;
    }
    if read_pin().as_deref() == Some("perf") {
        return false;
    }
    let ok = Command::new("tuned-adm")
        .args(["profile", "balanced"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        refresh_waybar();
    }
    ok
}

fn set_profile(full: &str) -> Result<()> {
    let Some((short, _, tuned)) = PROFILES.iter().find(|(_, f, _)| *f == full) else {
        return Ok(());
    };
    let ok = Command::new("tuned-adm")
        .args(["profile", tuned])
        .status()?
        .success();
    if ok {
        // Pin the user's choice so silent mode honors it on subsequent
        // transitions instead of forcing balanced.
        write_pin(short);
    }
    wifi::notify(
        "pwr",
        &format!("profile: {full}{}", if ok { "" } else { " (failed)" }),
    );
    refresh_waybar();
    Ok(())
}

fn confirm_then(action: &str, run: impl FnOnce() -> Result<()>) -> Result<()> {
    let input = "  yes\n  no";
    let choice = wifi::run_fuzzel(input, &format!("{action}? » "), false)?;
    if choice.trim_end_matches('\n').contains("yes") {
        run()
    } else {
        Ok(())
    }
}

fn run_status(cmd: &str, args: &[&str]) -> Result<()> {
    let _ = Command::new(cmd).args(args).status()?;
    Ok(())
}

fn spawn_detached(cmd: &str, args: &[&str]) -> Result<()> {
    let _ = Command::new(cmd).args(args).spawn()?;
    Ok(())
}

fn refresh_waybar() {
    let _ = Command::new("sh")
        .arg("-c")
        .arg("pkill -RTMIN+10 waybar 2>/dev/null")
        .status();
}
