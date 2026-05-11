//! `sy agt waybar` — waybar custom-module JSON.
//! Counts sy-managed sessions + unmanaged /proc agents; class reflects the
//! most-severe state (awaiting > busy > active > idle).

use std::io::Write;

use anyhow::Result;

use crate::agt::{
    client::Client,
    proc_scan,
    protocol::{ClientReply, ClientReq, SessionInfo, SessionStatus},
};

pub fn run() -> Result<()> {
    let sessions = fetch_sessions().unwrap_or_default();
    let unmanaged = proc_scan::scan();
    let total = sessions.len() + unmanaged.len();

    let class = severity_class(&sessions, total);
    let tooltip = build_tooltip(&sessions, &unmanaged);

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        r#"{{"text":"AGT:{total}","tooltip":"{tooltip}","class":"{class}","alt":"{total}"}}"#
    )?;
    out.flush()?;
    Ok(())
}

fn fetch_sessions() -> Result<Vec<SessionInfo>> {
    let mut c = Client::connect()?;
    match c.round_trip(&ClientReq::List)? {
        ClientReply::ListReply { sessions } => Ok(sessions),
        _ => Ok(Vec::new()),
    }
}

fn severity_class(sessions: &[SessionInfo], total: usize) -> &'static str {
    if total == 0 {
        return "idle";
    }
    if sessions
        .iter()
        .any(|s| matches!(s.status, SessionStatus::Awaiting))
    {
        return "awaiting";
    }
    if sessions
        .iter()
        .any(|s| matches!(s.status, SessionStatus::Working))
    {
        return "busy";
    }
    "active"
}

fn build_tooltip(sessions: &[SessionInfo], unmanaged: &[proc_scan::UnmanagedAgent]) -> String {
    let mut lines: Vec<String> = Vec::new();
    if sessions.is_empty() && unmanaged.is_empty() {
        return "no agents running".to_string();
    }
    if !sessions.is_empty() {
        lines.push("managed:".into());
        for s in sessions {
            lines.push(format!("  {} {} [{}]", s.id, s.agent, s.status.label()));
        }
    }
    if !unmanaged.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push("unmanaged:".into());
        let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
        for a in unmanaged {
            *counts.entry(a.provider).or_insert(0) += 1;
        }
        for (p, n) in counts {
            lines.push(format!("  {p}: {n}"));
        }
    }
    lines.join("\\n")
}
