use std::{fs, path::Path, process::Command};

use anyhow::{bail, Context, Result};

/// Toggle a named popup window. If a previously-spawned process for this
/// key is alive, kill it. Otherwise spawn the associated command.
///
/// PID state lives at /tmp/sy-popup-<key>.pid (with ':' replaced by '-').
pub fn toggle(key: &str) -> Result<()> {
    let safe_key = key.replace(':', "-");
    let pid_file = format!("/tmp/sy-popup-{safe_key}.pid");

    if let Ok(contents) = fs::read_to_string(&pid_file) {
        if let Ok(pid) = contents.trim().parse::<u32>() {
            if is_alive(pid) {
                kill(pid);
                let _ = fs::remove_file(&pid_file);
                return Ok(());
            }
        }
    }

    let sy_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "sy".to_string());

    struct Spec {
        app_id: String,
        argv: Vec<String>,
        size: Option<String>,
        font: Option<String>,
    }
    let spec = if let Some(id) = key.strip_prefix("agt:") {
        Spec {
            app_id: format!("sy-agt-{id}"),
            argv: vec![sy_path.clone(), "agt".into(), "inspect".into(), id.into()],
            size: Some("100x32".into()),
            font: Some("JetBrainsMono Nerd Font:size=10".into()),
        }
    } else {
        match key {
            "agents" => Spec {
                app_id: "sy-agents".into(),
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "while :; do clear; sy agt list; sleep 2; done".into(),
                ],
                size: None,
                font: None,
            },
            "nmtui" => Spec {
                app_id: "sy-nmtui".into(),
                argv: vec!["nmtui".into()],
                size: None,
                font: None,
            },
            "cal" => Spec {
                app_id: "sy-cal".into(),
                argv: vec![sy_path.clone(), "cal".into()],
                size: Some("24x11".into()),
                font: Some("JetBrainsMono Nerd Font:size=9".into()),
            },
            other => bail!("unknown popup key: {other}"),
        }
    };

    let mut args: Vec<String> = vec![
        "--app-id".into(),
        spec.app_id.clone(),
        "-T".into(),
        spec.app_id,
    ];
    if let Some(s) = spec.size {
        args.push(format!("--window-size-chars={s}"));
    }
    if let Some(f) = spec.font {
        args.push(format!("--font={f}"));
    }
    args.push("-e".into());
    for a in &spec.argv {
        args.push(a.clone());
    }

    let child = Command::new("foot")
        .args(&args)
        .spawn()
        .context("spawn foot")?;

    fs::write(&pid_file, child.id().to_string())
        .with_context(|| format!("write {pid_file}"))?;
    Ok(())
}

fn is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn kill(pid: u32) {
    let _ = Command::new("kill").arg(pid.to_string()).status();
}
