//! Mako-driven permission prompt for ACP `session/request_permission`.
//! Auto-allows after a timeout so the agent never deadlocks on user input.

use std::{process::Stdio, time::Duration};

use tokio::process::Command;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

pub async fn ask(summary: &str, body: &str, timeout: Duration) -> Decision {
    let key = Uuid::new_v4().simple().to_string();
    let synch = format!("string:x-canonical-private-synchronous:agt-perm-{key}");
    let spawn = Command::new("notify-send")
        .args([
            "-a",
            "sy",
            "-u",
            "critical",
            "--action=allow=Allow",
            "--action=deny=Deny",
            "--wait",
            "-h",
            &synch,
            summary,
            body,
        ])
        .stdout(Stdio::piped())
        .spawn();

    let Ok(child) = spawn else {
        return Decision::Allow; // notify-send missing → fail open
    };

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if key == "deny" {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }
        _ => Decision::Allow,
    }
}
