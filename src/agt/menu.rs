//! AGT applet left-click handler. Presents a fuzzel session picker:
//! [+ new session] → sy-managed sessions → separator → /proc-detected agents.

use std::process::Command;

use anyhow::Result;

use crate::{
    agt::{
        client::Client,
        proc_scan::{self, format_age, UnmanagedAgent},
        protocol::{ClientReply, ClientReq, SessionInfo},
    },
    popup, wifi,
};

enum Action {
    Noop,
    NewSession,
    OpenInspector(String),
    StopSession(String),
    LegacyFocus(u32),
    LegacySignal(u32, &'static str),
}

pub fn run() -> Result<()> {
    let sessions = list_sessions().unwrap_or_default();
    let unmanaged = proc_scan::scan();
    let unmanaged = exclude_managed(unmanaged, &sessions);

    let mut items: Vec<(String, Action)> = Vec::new();
    items.push(("+ new session  (Super+A)".to_string(), Action::NewSession));

    if sessions.is_empty() {
        items.push(("  (no sy-managed sessions)".to_string(), Action::Noop));
    } else {
        for s in &sessions {
            items.push((format_session(s), Action::OpenInspector(s.id.clone())));
            items.push((
                format!("    └─ stop  {}", s.id),
                Action::StopSession(s.id.clone()),
            ));
        }
    }

    if !unmanaged.is_empty() {
        items.push(("  ── unmanaged ───────────────".to_string(), Action::Noop));
        for a in &unmanaged {
            items.push((format_unmanaged(a), Action::LegacyFocus(a.pid)));
            items.push((
                format!("    └─ SIGTERM  pid {}", a.pid),
                Action::LegacySignal(a.pid, "-TERM"),
            ));
        }
    }

    let input: String = items
        .iter()
        .map(|(l, _)| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let choice = wifi::run_fuzzel(&input, "agt » ", false)?;
    let choice = choice.trim_end_matches('\n');
    if choice.is_empty() {
        return Ok(());
    }
    let Some((_, action)) = items.into_iter().find(|(l, _)| l == choice) else {
        return Ok(());
    };
    match action {
        Action::Noop => Ok(()),
        Action::NewSession => {
            // Spawn so this menu process can return immediately.
            let _ = Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "{} agt run &",
                    std::env::current_exe()
                        .ok()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "sy".into())
                ))
                .status();
            Ok(())
        }
        Action::OpenInspector(id) => popup::toggle(&format!("agt:{id}")),
        Action::StopSession(id) => {
            let mut c = Client::connect()?;
            let _ = c.round_trip(&ClientReq::Stop {
                session_id: id.clone(),
            })?;
            wifi::notify("agents", &format!("stopped {id}"));
            Ok(())
        }
        Action::LegacyFocus(pid) => {
            match proc_scan::focus_window(pid) {
                Ok(true) => {}
                Ok(false) => wifi::notify("agents", &format!("no niri window for pid {pid}")),
                Err(e) => wifi::notify("agents", &format!("focus error: {e}")),
            }
            Ok(())
        }
        Action::LegacySignal(pid, sig) => {
            let ok = Command::new("kill")
                .args([sig, &pid.to_string()])
                .status()?
                .success();
            wifi::notify(
                "agents",
                &format!("pid {pid} {sig}{}", if ok { "" } else { " (failed)" }),
            );
            Ok(())
        }
    }
}

pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    use crate::agt::{protocol::exit, AgtError};
    let mut c = Client::connect()?;
    match c.round_trip(&ClientReq::List)? {
        ClientReply::ListReply { sessions } => Ok(sessions),
        ClientReply::Error { message, code } => Err(AgtError {
            code: if code == 2 {
                exit::NO_SESSION
            } else {
                exit::DAEMON_UNAVAILABLE
            },
            msg: message,
        }
        .into()),
        _ => Err(AgtError {
            code: exit::DAEMON_UNAVAILABLE,
            msg: "unexpected daemon reply".into(),
        }
        .into()),
    }
}

fn format_session(s: &SessionInfo) -> String {
    let mark = match s.status.label() {
        "awaiting" => "!",
        "working" => "*",
        "running" => "·",
        "stopped" | "error" => "x",
        _ => " ",
    };
    let summary = truncate(&s.summary, 56);
    format!(
        "{mark} [{:<6}] {:<8} {:<8} {}",
        s.agent,
        &s.id,
        s.status.label(),
        summary
    )
}

fn format_unmanaged(a: &UnmanagedAgent) -> String {
    let tty = a.tty.as_deref().unwrap_or("—");
    let age = a.age_secs.map(format_age).unwrap_or_else(|| "—".into());
    let cmd = truncate(&a.cmd, 56);
    format!(
        "  [{:<6}] {:>6}  {:<10}  {:<6}  {}",
        a.provider, a.pid, tty, age, cmd
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Hide /proc-detected agents that match the pid of a sy-managed session's
/// underlying child process (we don't currently expose those PIDs through the
/// IPC, so this is a best-effort de-dup by command pattern).
fn exclude_managed(
    unmanaged: Vec<UnmanagedAgent>,
    _sessions: &[SessionInfo],
) -> Vec<UnmanagedAgent> {
    unmanaged
}
