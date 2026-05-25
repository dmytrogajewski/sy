//! Waybar control surface. Right now it only owns the `sy apply`
//! post-step that puts the bar's new `config.jsonc` / `style.css`
//! into effect after a deploy. Future home for unifying the
//! `pkill -RTMIN+N waybar` per-applet refresh signals scattered
//! across the per-applet modules (`src/bright.rs`, `src/silent.rs`,
//! …).

use std::{
    process::{Command, Stdio},
    thread,
    time::Duration,
};

/// Put a new waybar config into effect after `sy apply` rendered
/// `configs/waybar/*` to disk.
///
/// Why not `pkill -SIGUSR2 waybar` (the signal waybar's docs
/// nominally bind to "reload config"): on Waybar 0.14 the in-place
/// reload re-parses CSS reliably but does NOT re-apply
/// `modules-left` / `modules-center` / `modules-right` membership
/// changes — tiles added or removed from those arrays stay frozen
/// on the rail until the process is replaced. Killing and
/// respawning is the only reliable path for the `sy apply` flow,
/// which is exactly the case where the rail composition is most
/// likely to have moved.
///
/// niri's `spawn-at-startup "waybar"` only fires once at compositor
/// boot, so a respawn here has to do its own `Command::new("waybar")`
/// with the user's wayland env restored from the previous waybar
/// process. No-op when no waybar is running.
pub fn reload() {
    // Snapshot first — once pkill fires, /proc/<pid>/environ is gone.
    let Some(env) = peek_waybar_env() else {
        // No live waybar to mirror — nothing to do. Stay quiet so
        // idempotent `sy apply` runs don't grow noise.
        return;
    };
    let killed = Command::new("pkill")
        .args(["-TERM", "-x", "waybar"])
        .status();
    match killed {
        Ok(s) if s.success() => {}
        Ok(_) => return, // race: waybar exited between peek and pkill
        Err(e) => {
            eprintln!("  ! waybar restart: pkill failed: {e}");
            return;
        }
    }
    // Brief wait so waybar releases its wayland layer-shell surface
    // before the respawn binds a new one — otherwise the new bar
    // sometimes lands behind a stale surface and renders empty.
    thread::sleep(Duration::from_millis(300));

    let spawn = Command::new("setsid")
        .arg("waybar")
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawn {
        Ok(_) => println!("  ↻ waybar restarted"),
        Err(e) => eprintln!("  ! waybar respawn failed: {e}"),
    }
}

/// Snapshot the full env of a currently-running waybar so a respawn
/// inherits the exact environment niri's `spawn-at-startup` gave it
/// at compositor boot. Returns `None` when no waybar PID can be
/// found. Reads `/proc/<pid>/environ` — the process is owned by the
/// invoking user, so the read is permitted.
///
/// A previous attempt whitelisted only WAYLAND_DISPLAY +
/// XDG_RUNTIME_DIR + DISPLAY + the dbus address, but that dropped
/// `NIRI_SOCKET` (the IPC socket waybar's `niri/workspaces` and
/// `niri/window` modules need to talk to the compositor) and the
/// workspace + window-title tiles silently disappeared from the
/// rail after every `sy apply`. The full-env copy is the smallest
/// safe surface: whatever niri set is what the respawn gets, no
/// per-module knowledge required here.
fn peek_waybar_env() -> Option<Vec<(String, String)>> {
    let pid = first_waybar_pid()?;
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in raw.split(|b| *b == 0) {
        let Ok(s) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some((k, v)) = s.split_once('=') {
            out.push((k.to_string(), v.to_string()));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Lowest PID matching exactly `waybar`. Avoids `pgrep -x` plus a
/// subprocess for one filesystem walk.
fn first_waybar_pid() -> Option<u32> {
    let rd = std::fs::read_dir("/proc").ok()?;
    let mut pids: Vec<u32> = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(s) = name.to_str() else { continue };
        let Ok(pid) = s.parse::<u32>() else { continue };
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
            continue;
        };
        if comm.trim() == "waybar" {
            pids.push(pid);
        }
    }
    pids.sort();
    pids.into_iter().next()
}
