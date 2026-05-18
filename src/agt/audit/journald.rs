//! journald sink — SPEC §4.4 "Audit log" first bullet.
//!
//! `libsystemd::logging::journal_send` writes the structured `SY_*`
//! fields directly to the journal datagram socket. We never panic:
//! on hosts without systemd / inside containers / under non-systemd
//! init (CI) the socket is missing and the call returns
//! [`AuditSinkError::NotAvailable`]; the parent `emit` swallows it
//! into a `tracing::error!` and falls through to the JSONL sink.
//!
//! Field names match SPEC §4.4 verbatim — `SY_TOOL`, `SY_POLICY_SHA`,
//! `SY_DECISION`, `SY_ARGV`, `SY_REQUEST_ID`, `SY_TRACE_ID`,
//! `MESSAGE_ID`. Renaming any of these breaks Zone 6's `sy doctor`
//! and external `journalctl --user SY_DECISION=deny -o json` queries
//! that consumers (operators + automation) are documented to run.

use libsystemd::logging::{journal_send, Priority};

use crate::agt::audit::AuditRecord;

/// Stable `MESSAGE_ID` for every sandbox audit record. journald
/// indexes `MESSAGE_ID` separately from free-text `MESSAGE`, so a
/// fixed UUID lets external tooling subscribe to
/// `MESSAGE_ID=<this>` and stream every audit event without
/// false-positive matches against other journal lines that happen to
/// mention "sandbox".
///
/// Generated once via `journalctl --new-id128` (offline) and pinned
/// here so journald's catalog can attach docs later. Format follows
/// journald's MESSAGE_ID rule: 32 lowercase hex chars, no dashes.
const SY_AUDIT_MESSAGE_ID: &str = "7a1c5b3e9d4f4c2db8e6f1a290c4d51e";

/// Error returned by [`emit_journald`] when the journal datagram
/// socket is unreachable (no systemd, container, CI). The audit
/// dispatcher in `agt::audit::emit` converts this to a
/// `tracing::error!` and continues; tests assert on the variant.
#[derive(Debug)]
pub enum AuditSinkError {
    /// systemd-journald is not reachable on this host — typically
    /// missing `/run/systemd/journal/socket`. Defense-in-depth per
    /// SPEC §2.3 deep dive on the `tracing-journald` silent-drop
    /// bug; we surface the missing-sink case as a structured error
    /// instead of silently dropping.
    NotAvailable,
    /// journal_send rejected the write — most commonly because the
    /// payload exceeded the datagram size and the memfd fallback
    /// also failed. Caller logs via `tracing::error!`.
    SendFailed(String),
}

impl std::fmt::Display for AuditSinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuditSinkError::NotAvailable => f.write_str("systemd-journald socket unavailable"),
            AuditSinkError::SendFailed(e) => write!(f, "journald send failed: {e}"),
        }
    }
}

impl std::error::Error for AuditSinkError {}

/// Send `record` to systemd-journald with the SPEC §4.4 indexed
/// fields. Returns `Err(NotAvailable)` on systemd-less hosts (the
/// JSONL sink still records the event); other write failures
/// surface as `Err(SendFailed)`.
pub fn emit_journald(record: &AuditRecord) -> Result<(), AuditSinkError> {
    if !journal_socket_present() {
        return Err(AuditSinkError::NotAvailable);
    }

    // Pre-format every field into owned `String` values so the
    // iterator's borrow lifetimes stay simple and the entire payload
    // is one allocation each.
    let ts = record.ts.to_rfc3339();
    let argv = record.argv.join(" ");
    let decision = record.decision.as_str().to_string();
    let request_id = record.request_id.map(|u| u.to_string()).unwrap_or_default();
    let trace_id = record.trace_id.clone().unwrap_or_default();
    let reason = record.reason.clone().unwrap_or_default();

    let fields: Vec<(&str, String)> = vec![
        ("MESSAGE_ID", SY_AUDIT_MESSAGE_ID.to_string()),
        ("SY_TOOL", record.tool.clone()),
        ("SY_POLICY_SHA", record.policy_sha.clone()),
        ("SY_DECISION", decision),
        ("SY_ARGV", argv),
        ("SY_REQUEST_ID", request_id),
        ("SY_TRACE_ID", trace_id),
        ("SY_REASON", reason),
        ("SY_TS", ts),
    ];

    let msg = format!(
        "sandbox {decision} {tool}",
        decision = record.decision.as_str(),
        tool = record.tool,
    );

    journal_send(
        Priority::Info,
        &msg,
        fields.iter().map(|(k, v)| (*k, v.as_str())),
    )
    .map_err(|e| AuditSinkError::SendFailed(e.to_string()))
}

/// Return `true` if the journald datagram socket exists. Avoids
/// opening + EWOULDBLOCK probing — a stat is enough; the socket path
/// is fixed by systemd. On Fedora 43 + systemd 258 it's
/// `/run/systemd/journal/socket`. Non-systemd hosts and stripped
/// containers don't ship it.
fn journal_socket_present() -> bool {
    std::path::Path::new("/run/systemd/journal/socket").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agt::audit::AuditDecision;

    /// Defence-in-depth per SPEC §2.3: if the journald socket isn't
    /// available, `emit_journald` returns `Err(NotAvailable)`
    /// without panicking. We can't reliably *remove* the socket in a
    /// unit test (it's a system resource), so we exercise the
    /// branch by checking that *one of* the two outcomes holds:
    ///
    /// - `Err(NotAvailable)` on hosts without `/run/systemd/journal/socket`
    /// - `Ok(())` on hosts with it (the rice / Fedora 43 CI).
    ///
    /// The test fails iff `emit_journald` panics, which is the
    /// regression we're guarding against.
    #[test]
    fn journald_emit_does_not_panic_when_missing() {
        let record = AuditRecord::now(
            "/usr/bin/cat",
            "deadbeef",
            AuditDecision::Deny,
            vec!["/etc/shadow".into()],
        );
        match emit_journald(&record) {
            Ok(()) => { /* journald present — emission succeeded */ }
            Err(AuditSinkError::NotAvailable) => { /* journald missing — graceful skip */ }
            Err(AuditSinkError::SendFailed(e)) => {
                // Send failures shouldn't panic either; surface them
                // so a hidden regression in libsystemd reaches CI.
                panic!("journald send failed unexpectedly: {e}");
            }
        }
    }
}
