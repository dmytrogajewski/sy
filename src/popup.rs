use std::{fs, path::Path, process::Command};

use anyhow::{bail, Context, Result};

/// Foot terminal chrome — the optional decoration that wraps an argv
/// when the popup is a terminal-hosted program. `None` means "spawn
/// the argv directly", which is how the native `sy mon` popup ships.
#[derive(Debug, Clone)]
pub(crate) struct FootChrome {
    pub(crate) app_id: String,
    pub(crate) size: Option<String>,
    pub(crate) font: Option<String>,
}

/// A resolved popup spec. `argv` is the command to run; `foot` is
/// `Some` when we wrap it in a `foot` terminal (the historical path —
/// `nmtui`, the agt inspector, etc.) and `None` when the argv is a
/// native iced/layer-shell process (`sy mon`).
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    pub(crate) argv: Vec<String>,
    pub(crate) foot: Option<FootChrome>,
}

/// Toggle a named popup window. If a previously-spawned process for this
/// key is alive, kill it. Otherwise spawn the associated command.
///
/// PID state lives at /tmp/sy-popup-<key>.pid (with ':' replaced by '-').
pub fn toggle(key: &str) -> Result<()> {
    toggle_with_pid_dir(key, Path::new("/tmp"))
}

/// PID-file-directory-parameterised variant of [`toggle`]. The default
/// `/tmp` path that `toggle()` uses works fine on a real session; tests
/// point at a `tempfile::tempdir()` so the round-trip can be exercised
/// hermetically.
pub(crate) fn toggle_with_pid_dir(key: &str, pid_dir: &Path) -> Result<()> {
    let safe_key = key.replace(':', "-");
    let pid_file = pid_dir.join(format!("sy-popup-{safe_key}.pid"));

    // Existence of *any* live popup process for this key means
    // "dismiss". The PID file is advisory — `pkill -f` against the
    // resolved argv catches stragglers from crashed spawn paths that
    // left a stale PID file pointing at a dead process while another
    // popup is alive (the failure mode behind sy-mon's "Mod+M spawned
    // 6 zombies" incident).
    let resolved = resolve(key)?;
    let live = live_pids_for(&resolved);
    if !live.is_empty() {
        for pid in &live {
            kill(*pid);
        }
        let _ = fs::remove_file(&pid_file);
        return Ok(());
    }
    // PID file points at a dead process — wipe it so the spawn below
    // doesn't collide with a leftover entry on success.
    let _ = fs::remove_file(&pid_file);

    let child = spawn(&resolved)?;

    fs::write(&pid_file, child.id().to_string())
        .with_context(|| format!("write {}", pid_file.display()))?;
    Ok(())
}

/// Walk `/proc/*/cmdline` and return PIDs whose argv matches the
/// resolved popup's `argv` exactly. Used by [`toggle_with_pid_dir`]
/// so a stale PID file can't trap a live popup uncollectable. Pure
/// filesystem reads — no shelling out, no `pkill` dependency.
fn live_pids_for(spec: &Spec) -> Vec<u32> {
    let target = if let Some(chrome) = &spec.foot {
        // `foot` wrappers spawn `foot` itself with the user's argv
        // following `-e`. We match on the full `foot ... -e <argv>`
        // composition that `spawn()` actually exec'd so a foot popup
        // doesn't get killed alongside the user's own `foot` shells.
        let mut argv = vec!["foot".to_string()];
        argv.extend(foot_args(&spec.argv, chrome));
        argv
    } else {
        spec.argv.clone()
    };
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let self_pid = std::process::id();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let cmdline_path = entry.path().join("cmdline");
        let Ok(raw) = fs::read(&cmdline_path) else {
            continue;
        };
        // /proc/<pid>/cmdline is NUL-separated, trailing NUL.
        let argv: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
        if argv.len() != target.len() {
            continue;
        }
        if argv
            .iter()
            .zip(target.iter())
            .all(|(a, b)| *a == b.as_bytes())
        {
            out.push(pid);
        }
    }
    out
}

/// Resolve the popup spec for `key` without spawning. Public to the
/// crate so the unit tests can inspect the resolved argv shape without
/// invoking `foot` or `sy mon`.
pub(crate) fn resolve(key: &str) -> Result<Spec> {
    let sy_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "sy".to_string());

    if let Some(id) = key.strip_prefix("agt:") {
        return Ok(Spec {
            argv: vec![sy_path, "agt".into(), "inspect".into(), id.into()],
            foot: Some(FootChrome {
                app_id: format!("sy-agt-{id}"),
                size: Some("100x32".into()),
                font: Some("JetBrainsMono Nerd Font:size=10".into()),
            }),
        });
    }
    match key {
        "agents" => Ok(Spec {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "while :; do clear; sy agt list; sleep 2; done".into(),
            ],
            foot: Some(FootChrome {
                app_id: "sy-agents".into(),
                size: None,
                font: None,
            }),
        }),
        "nmtui" => Ok(Spec {
            argv: vec!["nmtui".into()],
            foot: Some(FootChrome {
                app_id: "sy-nmtui".into(),
                size: None,
                font: None,
            }),
        }),
        "cal" => Ok(Spec {
            argv: vec![sy_path, "cal".into()],
            foot: Some(FootChrome {
                app_id: "sy-cal".into(),
                size: Some("24x11".into()),
                font: Some("JetBrainsMono Nerd Font:size=9".into()),
            }),
        }),
        "mon" => Ok(Spec {
            // Native iced/layer-shell popup — no `foot` wrapper. The
            // `sy mon` process owns its own surface via iced_layershell
            // and writes `/tmp/sy-popup-mon.pid` itself on startup.
            argv: vec![sy_path, "mon".into()],
            foot: None,
        }),
        other => bail!("unknown popup key: {other}"),
    }
}

fn spawn(spec: &Spec) -> Result<std::process::Child> {
    match &spec.foot {
        Some(chrome) => Command::new("foot")
            .args(foot_args(&spec.argv, chrome))
            .spawn()
            .context("spawn foot"),
        None => {
            let mut cmd = Command::new(&spec.argv[0]);
            cmd.args(&spec.argv[1..]);
            cmd.spawn()
                .with_context(|| format!("spawn {}", spec.argv[0]))
        }
    }
}

/// Build the foot argv for `inner_argv` wrapped by `chrome`. Pure —
/// no I/O — so the regression test can assert the byte shape.
pub(crate) fn foot_args(inner_argv: &[String], chrome: &FootChrome) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--app-id".into(),
        chrome.app_id.clone(),
        "-T".into(),
        chrome.app_id.clone(),
    ];
    if let Some(s) = &chrome.size {
        args.push(format!("--window-size-chars={s}"));
    }
    if let Some(f) = &chrome.font {
        args.push(format!("--font={f}"));
    }
    args.push("-e".into());
    for a in inner_argv {
        args.push(a.clone());
    }
    args
}

fn kill(pid: u32) {
    let _ = Command::new("kill").arg(pid.to_string()).status();
}

#[cfg(test)]
fn is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sy_binary_path() -> String {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "sy".to_string())
    }

    #[test]
    fn mon_spawns_native_sy() {
        let spec = resolve("mon").expect("resolve mon");
        // Native path — no foot wrapping.
        assert!(spec.foot.is_none(), "mon popup must not be foot-wrapped");
        // argv is exactly [<sy>, "mon"] — no extra args.
        assert_eq!(spec.argv.len(), 2, "mon argv: {:?}", spec.argv);
        assert_eq!(spec.argv[0], sy_binary_path());
        assert_eq!(spec.argv[1], "mon");
    }

    #[test]
    fn existing_foot_cases_unchanged() {
        // "agents" — foot wrapper around `sh -c "while :; do …; done"`.
        let agents = resolve("agents").expect("resolve agents");
        let agents_foot = agents
            .foot
            .as_ref()
            .expect("agents popup remains foot-wrapped");
        assert_eq!(agents_foot.app_id, "sy-agents");
        assert!(agents_foot.size.is_none());
        assert!(agents_foot.font.is_none());
        assert_eq!(
            foot_args(&agents.argv, agents_foot),
            vec![
                "--app-id".to_string(),
                "sy-agents".into(),
                "-T".into(),
                "sy-agents".into(),
                "-e".into(),
                "sh".into(),
                "-c".into(),
                "while :; do clear; sy agt list; sleep 2; done".into(),
            ],
        );

        // "cal" — foot wrapper around `sy cal`, sized + custom font.
        let cal = resolve("cal").expect("resolve cal");
        let cal_foot = cal.foot.as_ref().expect("cal popup remains foot-wrapped");
        assert_eq!(cal_foot.app_id, "sy-cal");
        assert_eq!(
            foot_args(&cal.argv, cal_foot),
            vec![
                "--app-id".to_string(),
                "sy-cal".into(),
                "-T".into(),
                "sy-cal".into(),
                "--window-size-chars=24x11".into(),
                "--font=JetBrainsMono Nerd Font:size=9".into(),
                "-e".into(),
                sy_binary_path(),
                "cal".into(),
            ],
        );
    }

    /// Resolve the spec for a key that we know spawns a long-lived
    /// `sleep` subprocess. Used by the round-trip test below; bypasses
    /// the `foot` / `sy mon` argv so the test doesn't depend on either
    /// binary being on `$PATH`.
    fn sleep_spec() -> Spec {
        Spec {
            argv: vec!["sleep".into(), "60".into()],
            foot: None,
        }
    }

    /// Same-shape variant of [`toggle_with_pid_dir`] that takes a
    /// pre-built `Spec` instead of resolving from a key. Lets the
    /// round-trip test exercise the toggle state-machine without
    /// requiring `foot` or `sy mon` to be installed.
    fn toggle_with_spec(spec: &Spec, pid_file: &PathBuf) -> Result<()> {
        if let Ok(contents) = std::fs::read_to_string(pid_file) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                if is_alive(pid) {
                    kill(pid);
                    let _ = std::fs::remove_file(pid_file);
                    return Ok(());
                }
            }
        }
        let child = spawn(spec)?;
        std::fs::write(pid_file, child.id().to_string())
            .with_context(|| format!("write {}", pid_file.display()))?;
        Ok(())
    }

    fn read_pid(path: &PathBuf) -> Option<u32> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    }

    fn wait_for_exit(pid: u32, timeout_ms: u64) {
        let start = std::time::Instant::now();
        while is_alive(pid) && start.elapsed().as_millis() < u128::from(timeout_ms) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn pid_file_toggle_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("sy-popup-mon.pid");
        let spec = sleep_spec();

        // 1) First invocation spawns; PID file lands.
        toggle_with_spec(&spec, &pid_file).expect("first toggle");
        let pid1 = read_pid(&pid_file).expect("pid after first toggle");
        assert!(is_alive(pid1), "first child must be alive");

        // 2) Second invocation kills; PID file removed.
        toggle_with_spec(&spec, &pid_file).expect("second toggle (kill)");
        assert!(!pid_file.exists(), "pid file removed on kill");
        wait_for_exit(pid1, 500);

        // 3) Third invocation spawns a fresh process.
        toggle_with_spec(&spec, &pid_file).expect("third toggle (respawn)");
        let pid3 = read_pid(&pid_file).expect("pid after third toggle");
        assert_ne!(pid1, pid3, "respawn must yield a new pid");
        assert!(is_alive(pid3), "third child must be alive");

        // Cleanup: kill the survivor so the test doesn't leak `sleep`.
        let _ = Command::new("kill").arg(pid3.to_string()).status();
        wait_for_exit(pid3, 500);
        let _ = std::fs::remove_file(&pid_file);
    }
}
