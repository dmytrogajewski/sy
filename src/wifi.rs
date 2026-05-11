use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

pub fn pick() -> Result<()> {
    let _ = Command::new("nmcli")
        .args(["dev", "wifi", "rescan"])
        .status();

    let entries = list();
    if entries.is_empty() {
        notify("wifi", "no networks found");
        return Ok(());
    }

    let lines: Vec<String> = entries
        .iter()
        .map(|(active, ssid, meta)| {
            let mark = if *active { "*" } else { " " };
            format!("{mark} {ssid:<24} {meta}")
        })
        .collect();

    let choice = run_fuzzel(&lines.join("\n"), "wifi » ", false)?;
    let choice = choice.trim();
    if choice.is_empty() {
        return Ok(());
    }

    let Some((_, ssid, _)) = entries.iter().find(|(_, ssid, _)| choice.contains(ssid.as_str())) else {
        notify("wifi", "could not resolve picked entry");
        return Ok(());
    };
    connect(ssid)
}

/// List available wi-fi networks as (active, ssid, meta) tuples.
pub fn list() -> Vec<(bool, String, String)> {
    let out = Command::new("nmcli")
        .args([
            "-t", "-f", "IN-USE,SSID,SIGNAL,SECURITY",
            "dev", "wifi", "list", "--rescan", "no",
        ])
        .output()
        .ok();
    let Some(o) = out else { return Vec::new(); };

    let mut entries: Vec<(bool, String, String)> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(parse_nmcli_line)
        .filter(|(_, ssid, _)| !ssid.is_empty() && ssid != "--")
        .collect();

    entries.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| signal(&b.2).cmp(&signal(&a.2)))
    });
    entries.dedup_by(|a, b| a.1 == b.1);
    entries
}

/// Connect to an SSID: activate a saved profile if present, otherwise
/// prompt for a password and create a new connection.
pub fn connect(ssid: &str) -> Result<()> {
    let existing = Command::new("nmcli")
        .args(["-t", "-f", "NAME", "connection", "show"])
        .output()
        .context("nmcli show")?;
    let saved = String::from_utf8_lossy(&existing.stdout)
        .lines()
        .any(|l| l == ssid);

    if saved {
        let status = Command::new("nmcli")
            .args(["connection", "up", "id", ssid])
            .status()?;
        notify(
            "wifi",
            &if status.success() {
                format!("connected: {ssid}")
            } else {
                format!("connect failed: {ssid}")
            },
        );
        return Ok(());
    }

    let pass = run_fuzzel("", &format!("password for {ssid} » "), true)?;
    let pass = pass.trim_end_matches('\n').to_string();
    if pass.is_empty() {
        return Ok(());
    }

    let status = Command::new("nmcli")
        .args(["dev", "wifi", "connect", ssid, "password", &pass])
        .status()?;

    notify(
        "wifi",
        &if status.success() {
            format!("connected: {ssid}")
        } else {
            format!("connect failed: {ssid}")
        },
    );
    Ok(())
}

fn parse_nmcli_line(l: &str) -> Option<(bool, String, String)> {
    let parts = parse_colon_fields(l);
    if parts.len() < 4 {
        return None;
    }
    let active = parts[0] == "*";
    let ssid = parts[1].clone();
    let signal_pct: u32 = parts[2].parse().unwrap_or(0);
    let sec = if parts[3].is_empty() {
        "open".to_string()
    } else {
        parts[3].clone()
    };
    Some((active, ssid, format!("{signal_pct:>3}%  {sec}")))
}

/// Parse a colon-separated line from `nmcli -t`, honoring backslash escapes.
pub fn parse_colon_fields(l: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut esc = false;
    for c in l.chars() {
        if esc {
            cur.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == ':' {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    parts
}

fn signal(meta: &str) -> u32 {
    meta.split('%').next().and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

pub fn run_fuzzel(input: &str, prompt: &str, password: bool) -> Result<String> {
    let mut cmd = Command::new("fuzzel");
    cmd.arg("--dmenu");
    // Empty stdin in dmenu mode makes fuzzel exit immediately. `--prompt-only`
    // tells fuzzel not to wait for stdin and implies --lines=0, turning the
    // window into a free-text input box.
    if input.is_empty() {
        cmd.arg(format!("--prompt-only={prompt}"));
    } else {
        cmd.arg(format!("--prompt={prompt}"));
    }
    if password {
        cmd.arg("--password");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn fuzzel")?;
    if !input.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes())?;
        }
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub fn notify(summary: &str, body: &str) {
    // -a sy so notif::watch filters these out — they're status messages
    // sy emits about its own state, not user-facing notifications worth
    // archiving.
    let _ = Command::new("notify-send")
        .args(["-a", "sy", summary, body])
        .status();
}
