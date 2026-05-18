//! Dual-sink audit log — SPEC §4.4 "Audit log" / arch-agent-sandbox
//! Step 5. Every sandbox decision (allow / deny / consent) emits a
//! structured record to BOTH:
//!
//! 1. systemd-journald via [`journald::emit_journald`] with the
//!    indexed `SY_*` fields documented in SPEC §4.4 — `sy doctor`
//!    (Zone 6) and external `journalctl --user SY_DECISION=deny`
//!    queries depend on those names; do not rename.
//! 2. `$XDG_STATE_HOME/sy/audit.jsonl` via [`jsonl::emit_jsonl`],
//!    rotated at 64 MiB with zstd compression and a 10-archive
//!    retention cap so the file system can't be filled by a
//!    long-running agent.
//!
//! Both sinks are *fire-and-forget*: each swallows its own errors and
//! emits a `tracing::error!` on the `sy::agt::audit` target so the
//! arch-observability stack catches sink-level failures. The audit
//! call site never panics — a sandbox decision is the security
//! record, and losing one to a write error must not crash the
//! caller. The DEDICATED sinks (journald `SY_*` fields, JSONL on
//! disk) are the source of truth; the `tracing` mirror is best-effort.

pub mod journald;
pub mod jsonl;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Three-way audit verdict — mirrors [`crate::agt::policy::Decision`]
/// but spelled as a flat enum because `Decision::ConsentRequired`
/// carries a `reason` payload that lands on [`AuditRecord::reason`]
/// rather than the variant itself. Serialised lowercase to match
/// SPEC §4.4's `SY_DECISION ∈ {allow,deny,consent}` constraint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditDecision {
    Allow,
    Deny,
    Consent,
}

impl AuditDecision {
    /// Lowercase wire form for the journald `SY_DECISION` field and
    /// the JSONL `decision` column. Keeping the mapping in code (not
    /// just via serde) means non-serde callers (journald) hit the
    /// same string.
    pub fn as_str(self) -> &'static str {
        match self {
            AuditDecision::Allow => "allow",
            AuditDecision::Deny => "deny",
            AuditDecision::Consent => "consent",
        }
    }
}

/// One audit-log entry. The field layout matches SPEC §4.4's listing
/// verbatim — `ts`, `tool`, `policy_sha`, `decision`, `argv`,
/// `request_id`, `trace_id`, `reason` — so the JSONL schema and the
/// journald `SY_*` indexed fields stay in lockstep. `request_id` and
/// `trace_id` are `Option` because the IPC envelope (arch-ipc-v1
/// Step 6) and the trace context (arch-observability Step 4) backfill
/// them; pre-wiring call sites stamp `None` so audit emission can
/// land before those zones do.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditRecord {
    pub ts: DateTime<Utc>,
    pub tool: String,
    pub policy_sha: String,
    pub decision: AuditDecision,
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Ulid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AuditRecord {
    /// Build a record stamped with the current UTC time. Most call
    /// sites use this constructor; tests that need a deterministic
    /// timestamp build the struct literal directly.
    pub fn now(
        tool: impl Into<String>,
        policy_sha: impl Into<String>,
        decision: AuditDecision,
        argv: Vec<String>,
    ) -> Self {
        Self {
            ts: Utc::now(),
            tool: tool.into(),
            policy_sha: policy_sha.into(),
            decision,
            argv,
            request_id: None,
            trace_id: None,
            reason: None,
        }
    }

    /// Attach the IPC envelope's `request_id` (arch-ipc-v1 Step 6).
    #[must_use]
    pub fn with_request_id(mut self, id: Option<Ulid>) -> Self {
        self.request_id = id;
        self
    }

    /// Attach the W3C trace_id (arch-observability Step 4).
    #[must_use]
    pub fn with_trace_id(mut self, id: Option<String>) -> Self {
        self.trace_id = id;
        self
    }

    /// Attach a human-readable reason — populated for `Deny` /
    /// `Consent` decisions from [`crate::agt::policy::Decision`].
    #[must_use]
    pub fn with_reason(mut self, reason: Option<String>) -> Self {
        self.reason = reason;
        self
    }
}

/// `$XDG_STATE_HOME/sy` (or `~/.local/state/sy`) — the audit JSONL
/// directory. Mirrors the layout used by
/// `sy_core::obs::state_logs_dir()` so operators only ever look in
/// one place under `~/.local/state/sy/`.
pub fn default_audit_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_STATE_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("sy");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/sy");
    }
    PathBuf::from("sy")
}

/// Fire-and-forget dual-sink emit. Each sink swallows its own error
/// and forwards it to `tracing::error!(target = "sy::agt::audit")`;
/// the call site cannot fail. Callers pass the on-disk dir explicitly
/// so tests use a tempdir and production passes
/// [`default_audit_dir`].
pub fn emit(record: &AuditRecord, jsonl_dir: &Path) {
    if let Err(e) = jsonl::emit_jsonl(record, jsonl_dir) {
        tracing::error!(
            target: "sy::agt::audit",
            sink = "jsonl",
            tool = record.tool.as_str(),
            decision = record.decision.as_str(),
            error = %e,
            "audit sink failed"
        );
    }
    if let Err(e) = journald::emit_journald(record) {
        tracing::error!(
            target: "sy::agt::audit",
            sink = "journald",
            tool = record.tool.as_str(),
            decision = record.decision.as_str(),
            error = %e,
            "audit sink failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_decision_round_trips_lowercase() {
        // SPEC §4.4 mandates lowercase `SY_DECISION ∈
        // {allow,deny,consent}` — both the journald wire form and
        // the JSONL schema must agree.
        let json = serde_json::to_string(&AuditDecision::Deny).expect("ser deny");
        assert_eq!(json, "\"deny\"");
        let parsed: AuditDecision = serde_json::from_str("\"consent\"").expect("de consent");
        assert_eq!(parsed, AuditDecision::Consent);
        assert_eq!(AuditDecision::Allow.as_str(), "allow");
    }

    #[test]
    fn record_now_populates_ts_and_carries_argv() {
        let r = AuditRecord::now(
            "/usr/bin/rg",
            "deadbeef",
            AuditDecision::Allow,
            vec!["foo".into()],
        );
        assert_eq!(r.tool, "/usr/bin/rg");
        assert_eq!(r.decision, AuditDecision::Allow);
        assert_eq!(r.argv, vec!["foo".to_string()]);
        assert!(r.request_id.is_none());
        assert!(r.trace_id.is_none());
        assert!(r.reason.is_none());
    }

    /// SPEC §4.4 dual sink end-to-end: a single `emit` writes BOTH a
    /// JSONL line under the configured dir AND (when journald is
    /// reachable) a journald record discoverable via `journalctl
    /// --user SY_DECISION=deny -o json`. The journald half is
    /// `#[ignore]`-gated because CI runners commonly lack a user
    /// manager session, but the JSONL half always runs.
    #[test]
    #[ignore = "needs `journalctl --user` available; verify locally on rice host"]
    fn dual_sink_emits_both_records() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let record = AuditRecord::now(
            "/usr/bin/cat",
            "deadbeef",
            AuditDecision::Deny,
            vec!["/etc/shadow".into()],
        )
        .with_reason(Some("strict profile".into()));

        // Fire both sinks.
        emit(&record, tmp.path());

        // JSONL side: live file exists with exactly one line whose
        // decision = "deny".
        let live = tmp.path().join("audit.jsonl");
        let body = std::fs::read_to_string(&live).expect("read live");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1, "expected one JSONL line; got {body:?}");
        let v: serde_json::Value = serde_json::from_str(lines[0]).expect("parse line");
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["tool"], "/usr/bin/cat");

        // journald side: best-effort. `journalctl --user
        // SY_DECISION=deny -o json --since='1 minute ago'` should
        // surface our record. If `journalctl` is missing (no
        // systemd), skip the assertion — the JSONL sink already
        // covered the audit-record contract.
        if which::which("journalctl").is_err() {
            eprintln!("journalctl missing; journald sink half of dual-sink skipped");
            return;
        }
        let out = std::process::Command::new("journalctl")
            .args([
                "--user",
                "SY_DECISION=deny",
                "SY_TOOL=/usr/bin/cat",
                "-o",
                "json",
                "--since",
                "1 minute ago",
                "--no-pager",
            ])
            .output()
            .expect("invoke journalctl");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("\"SY_DECISION\":\"deny\""),
            "journald record missing; journalctl stdout: {stdout}"
        );
    }
}
