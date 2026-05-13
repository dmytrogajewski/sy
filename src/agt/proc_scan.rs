//! /proc-walk to detect AI-agent processes started outside of sy-agentd.
//! Survives from the previous `src/agents.rs` so the AGT applet can still
//! show ad-hoc `claude`/`goose`/`cursor-agent` instances under a separator.

use std::{fs, time::Duration};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct UnmanagedAgent {
    pub provider: &'static str,
    pub pid: u32,
    pub cmd: String,
    pub tty: Option<String>,
    pub age_secs: Option<u64>,
}

const PROVIDERS: &[(&str, &[&str])] = &[
    ("claude", &["claude", "claude-code", "claude-code-acp"]),
    ("cursor", &["cursor-agent"]),
    ("aider", &["aider"]),
    ("codex", &["codex"]),
    ("goose", &["goose"]),
];

pub fn scan() -> Vec<UnmanagedAgent> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    let self_pid = std::process::id();
    let uptime = read_uptime().unwrap_or(0.0);
    let ticks = sysconf_clock_ticks() as f64;

    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(s) = fname.to_str() else { continue };
        let Ok(pid) = s.parse::<u32>() else { continue };
        if pid == self_pid {
            continue;
        }

        let Ok(bytes) = fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }

        let argv0 = bytes.split(|&b| b == 0).next().unwrap_or(&[]);
        let Ok(argv0_str) = std::str::from_utf8(argv0) else {
            continue;
        };
        let base = argv0_str.rsplit('/').next().unwrap_or(argv0_str);

        let Some(provider) = PROVIDERS
            .iter()
            .find_map(|(name, pats)| pats.iter().any(|p| base == *p).then_some(*name))
        else {
            continue;
        };

        let cmd = bytes
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| std::str::from_utf8(s).ok())
            .collect::<Vec<_>>()
            .join(" ");

        out.push(UnmanagedAgent {
            provider,
            pid,
            cmd,
            tty: read_tty(pid),
            age_secs: read_age(pid, uptime, ticks).map(|d| d.as_secs()),
        });
    }

    out.sort_by(|a, b| a.provider.cmp(b.provider).then(a.pid.cmp(&b.pid)));
    out
}

fn read_tty(pid: u32) -> Option<String> {
    let link = fs::read_link(format!("/proc/{pid}/fd/0")).ok()?;
    let s = link.to_string_lossy().to_string();
    if s.starts_with("/dev/pts/") || s.starts_with("/dev/tty") {
        Some(s.trim_start_matches("/dev/").to_string())
    } else {
        None
    }
}

fn read_uptime() -> Option<f64> {
    let s = fs::read_to_string("/proc/uptime").ok()?;
    s.split_whitespace().next()?.parse().ok()
}

fn read_age(pid: u32, uptime: f64, ticks: f64) -> Option<Duration> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat.rfind(')')?;
    let rest = &stat[rparen + 2..];
    let starttime: f64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    let age_secs = uptime - (starttime / ticks);
    if age_secs.is_finite() && age_secs >= 0.0 {
        Some(Duration::from_secs(age_secs as u64))
    } else {
        None
    }
}

fn sysconf_clock_ticks() -> i64 {
    unsafe {
        let t = sysconf(_SC_CLK_TCK);
        if t <= 0 {
            100
        } else {
            t
        }
    }
}

extern "C" {
    fn sysconf(name: i32) -> i64;
}
const _SC_CLK_TCK: i32 = 2;

pub fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86400, (secs % 86400) / 3600)
    }
}

pub fn focus_window(pid: u32) -> std::io::Result<bool> {
    use std::process::Command;
    let out = Command::new("niri")
        .args(["msg", "-j", "windows"])
        .output()?;
    if !out.status.success() {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let target_pid: i64 = pid as i64;
    let mut idx = 0usize;
    while let Some(p) = text[idx..].find("\"pid\":") {
        let absolute = idx + p;
        let after = &text[absolute + "\"pid\":".len()..];
        let num: String = after
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if num.parse::<i64>().ok() == Some(target_pid) {
            if let Some(rel) = text[..absolute].rfind("\"id\":") {
                let rest = &text[rel + "\"id\":".len()..];
                let id: String = rest
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if !id.is_empty() {
                    let ok = Command::new("niri")
                        .args(["msg", "action", "focus-window", "--id", &id])
                        .status()?
                        .success();
                    return Ok(ok);
                }
            }
        }
        idx = absolute + "\"pid\":".len();
    }
    Ok(false)
}
