//! Mako-driven permission prompt for ACP `session/request_permission`.
//! Auto-allows after a timeout so the agent never deadlocks on user input.

use std::{path::Path, process::Stdio, time::Duration};

use tokio::process::Command;
use ulid::Ulid;
use uuid::Uuid;

use crate::agt::{
    audit::{self, AuditDecision, AuditRecord},
    policy::{Decision as PolicyDecision, Resolver},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// Policy-aware entry point. The resolver's three-way verdict short-
/// circuits the `notify-send` prompt:
/// - `PolicyDecision::Allow` → `Decision::Allow` immediately.
/// - `PolicyDecision::Deny`  → `Decision::Deny` immediately.
/// - `PolicyDecision::ConsentRequired` → fall through to [`ask`] so
///   the existing mako-action-button UX still drives the choice.
///
/// The plain [`ask`] remains for callers that don't have a resolver
/// yet (the ACP `session/request_permission` path during the Step 1
/// transition window). Step 6 replaces the `notify-send` fallback
/// for `EveryCall` with the `sy approve <token>` flow.
pub async fn ask_with_policy(
    resolver: &Resolver,
    tool: &str,
    argv: &[String],
    summary: &str,
    body: &str,
    timeout: Duration,
    request_id: Option<Ulid>,
) -> Decision {
    // SPEC §4.4 "Audit log": every sandbox decision (allow / deny /
    // consent) lands in both sinks. Audit before the action so a
    // crash mid-prompt still records the policy verdict.
    let verdict = resolver.decide(tool, argv);
    let audit_dir = audit::default_audit_dir();
    emit_policy_audit(resolver, tool, argv, &verdict, &audit_dir, request_id);
    match verdict {
        PolicyDecision::Allow => Decision::Allow,
        PolicyDecision::Deny => Decision::Deny,
        PolicyDecision::ConsentRequired { .. } => ask(summary, body, timeout).await,
    }
}

/// Emit the dual-sink audit record for `verdict`. Extracted from
/// [`ask_with_policy`] so callers (and tests) can target a specific
/// audit directory without env-var mutation. `trace_id` is lifted
/// from `sy_core::obs::current_trace_ctx()` and `request_id` is the
/// originating IPC v1 envelope's id threaded down by the daemon
/// (arch-ipc-v1 Step 6 / arch-agent-sandbox follow-up).
pub(crate) fn emit_policy_audit(
    resolver: &Resolver,
    tool: &str,
    argv: &[String],
    verdict: &PolicyDecision,
    audit_dir: &Path,
    request_id: Option<Ulid>,
) {
    let (audit_decision, reason) = match verdict {
        PolicyDecision::Allow => (AuditDecision::Allow, None),
        PolicyDecision::Deny => (AuditDecision::Deny, Some(format!("policy denied {tool}"))),
        PolicyDecision::ConsentRequired { reason } => {
            (AuditDecision::Consent, Some(reason.clone()))
        }
    };
    let trace_id = sy_core::obs::current_trace_ctx().map(|c| c.trace_id.0);
    audit::emit(
        &AuditRecord::now(tool, resolver.fingerprint(), audit_decision, argv.to_vec())
            .with_reason(reason)
            .with_trace_id(trace_id)
            .with_request_id(request_id),
        audit_dir,
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// SPEC §4.4 + arch-ipc-v1 Step 6: when the daemon's permission
    /// handler threads the IPC envelope's `request_id` into
    /// [`ask_with_policy`], it must land on the emitted `AuditRecord`
    /// so journald (`SY_REQUEST_ID`) and JSONL queries can correlate
    /// the decision back to the originating call. We target
    /// [`emit_policy_audit`] directly with a tempdir so the unit test
    /// stays hermetic — production threading is covered by the
    /// daemon-level test that constructs the resolver from disk.
    #[test]
    fn audit_record_carries_request_id() {
        let policy_root = workspace_policy_root();
        let resolver =
            Resolver::load(&policy_root, "strict", None, &policy_root).expect("load strict");
        let tmp = tempfile::tempdir().expect("tempdir");
        let request_id = Ulid::new();
        let verdict = resolver.decide("/usr/bin/cat", &["/etc/shadow".into()]);
        emit_policy_audit(
            &resolver,
            "/usr/bin/cat",
            &["/etc/shadow".into()],
            &verdict,
            tmp.path(),
            Some(request_id),
        );
        let body = std::fs::read_to_string(tmp.path().join("audit.jsonl")).expect("audit jsonl");
        let line = body.lines().next().expect("one line");
        let v: serde_json::Value = serde_json::from_str(line).expect("parse line");
        assert_eq!(
            v["request_id"].as_str(),
            Some(request_id.to_string().as_str()),
            "request_id round-trips into JSONL record; line={line}"
        );
    }

    fn workspace_policy_root() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("configs")
            .join("policy")
    }
}
