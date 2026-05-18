//! In-daemon consent store — SPEC §4.4 "Consent UX" / arch-agent-sandbox
//! Step 6. Holds the pending `ConsentRequired` tool calls keyed by a
//! freshly-minted UUID token; a separate `sy approve <token>` IPC call
//! (or the existing `sy policy grant` overlay) resolves the entry,
//! waking the caller waiting on the oneshot receiver.
//!
//! Lifecycle of a single consent request:
//!
//! 1. The daemon's permission handler routes `Decision::ConsentRequired`
//!    through [`ConsentStore::issue`], obtaining a `(token, rx)` pair.
//!    The handler replies to the original IPC call with
//!    `ErrorCode::ConsentRequired { token, expires_at, policy_diff }`
//!    and awaits `rx`.
//! 2. The operator runs `sy approve <token>` from a TTY (or pre-issues
//!    a `sy policy grant`). The CLI hits the `agt.approve` IPC method,
//!    which calls [`ConsentStore::decide`] with the token and
//!    `Decision::Allow`.
//! 3. The original handler wakes on `rx`, audits the consent decision
//!    (carrying the original IPC `request_id` and the approver's
//!    `pid`/`uid` from `SO_PEERCRED`), and resumes the tool call.
//!
//! Expiry is checked on every `decide` call; expired entries return
//! `ConsentError::Expired` rather than `NotFound` so the caller can
//! surface a helpful message ("token expired; rerun the original tool
//! call"). [`ConsentStore::cleanup_expired`] sweeps the map and drops
//! stale receivers (their senders are dropped, so the original handler
//! sees `RecvError` and treats the request as denied).
//!
//! Auto-approval is intentionally absent — no LLM-driven heuristics ever
//! decide consent (SPEC §3.4 anti-goal). The only ways to flip a token
//! to `Allow` are an interactive `sy approve` from a TTY or a pre-issued
//! `sy policy grant` overlay.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use tokio::sync::oneshot;
use uuid::Uuid;

/// What [`ConsentStore::decide`] delivers to the waiting handler.
/// SPEC §4.4 only models the approve path on the wire (`sy approve
/// <token>`); explicit reject is surfaced indirectly — the operator
/// either lets the token expire (yielding `RecvError` on the
/// receiver, which the daemon maps to `Decision::Deny`) or kills the
/// pending tool call via `sy agt stop`. Keeping the enum as a single
/// `Allow` variant makes that contract explicit on the type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsentDecision {
    Allow,
}

/// Errors surfaced by [`ConsentStore::decide`]. Mapped to
/// `ErrorCode::BadRequest` on the IPC reply so the operator's
/// `sy approve` invocation sees a structured failure.
#[derive(Debug, PartialEq, Eq)]
pub enum ConsentError {
    /// The token isn't in the store — never issued, already decided,
    /// or already swept by [`ConsentStore::cleanup_expired`].
    NotFound,
    /// The token was found but past its TTL.
    Expired,
}

impl std::fmt::Display for ConsentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsentError::NotFound => {
                f.write_str("consent token not found (already approved, denied, or never issued)")
            }
            ConsentError::Expired => f.write_str(
                "consent token expired (rerun the original tool call to mint a fresh one)",
            ),
        }
    }
}

impl std::error::Error for ConsentError {}

/// One pending consent slot. Lives in the store's `Mutex<HashMap>`
/// keyed by the consent token until the operator decides or the
/// entry expires.
#[derive(Debug)]
pub struct PendingConsent {
    pub tool: String,
    pub argv: Vec<String>,
    pub policy_diff: String,
    pub expires_at: Instant,
    decided: oneshot::Sender<ConsentDecision>,
}

/// Per-daemon registry of outstanding consent requests. Multiple
/// `PendingConsent` entries coexist (each tool call gets its own UUID),
/// so approving one token never collides with another.
#[derive(Debug, Default)]
pub struct ConsentStore {
    inner: Mutex<HashMap<Uuid, PendingConsent>>,
}

impl ConsentStore {
    /// Fresh store with no pending entries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Park a new consent request. Returns the freshly-minted token to
    /// surface in the IPC reply and the receiver the handler awaits.
    /// Also sweeps expired entries inline so a long-running daemon
    /// can't accumulate them under a flood of un-approved requests.
    pub fn issue(
        &self,
        tool: &str,
        argv: &[String],
        policy_diff: String,
        ttl: Duration,
    ) -> (Uuid, oneshot::Receiver<ConsentDecision>) {
        self.cleanup_expired();
        let token = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        let entry = PendingConsent {
            tool: tool.to_string(),
            argv: argv.to_vec(),
            policy_diff,
            expires_at: Instant::now() + ttl,
            decided: tx,
        };
        if let Ok(mut map) = self.inner.lock() {
            map.insert(token, entry);
        }
        (token, rx)
    }

    /// Resolve a pending consent. Looks up the entry, removes it from
    /// the store, and sends on the oneshot. Returns
    /// `ConsentError::NotFound` if the token isn't known and
    /// `ConsentError::Expired` if the entry was still in the map but
    /// its `expires_at` is in the past.
    pub fn decide(&self, token: Uuid, decision: ConsentDecision) -> Result<(), ConsentError> {
        let mut map = self.inner.lock().map_err(|_| ConsentError::NotFound)?;
        let entry = map.remove(&token).ok_or(ConsentError::NotFound)?;
        if entry.expires_at <= Instant::now() {
            return Err(ConsentError::Expired);
        }
        // The receiver may have been dropped (caller timed out). That's
        // a benign race — silently swallow the send error.
        let _ = entry.decided.send(decision);
        Ok(())
    }

    /// Inspect a pending entry without consuming it. Returns the
    /// tool + argv + policy_diff so the daemon can surface them on
    /// the IPC reply that announces the token. `None` when the
    /// entry has expired or never existed.
    pub fn snapshot(&self, token: Uuid) -> Option<PendingSnapshot> {
        let map = self.inner.lock().ok()?;
        let entry = map.get(&token)?;
        if entry.expires_at <= Instant::now() {
            return None;
        }
        Some(PendingSnapshot {
            tool: entry.tool.clone(),
            argv: entry.argv.clone(),
            policy_diff: entry.policy_diff.clone(),
            expires_at: entry.expires_at,
        })
    }

    /// Sweep expired entries. Their `decided` senders are dropped,
    /// which the waiting handlers observe as `RecvError` (treated as
    /// deny). Idempotent — safe to call from a periodic background
    /// task or from each `issue` call site.
    pub fn cleanup_expired(&self) {
        if let Ok(mut map) = self.inner.lock() {
            let now = Instant::now();
            map.retain(|_, entry| entry.expires_at > now);
        }
    }

    /// Test helper: how many entries currently sit in the store.
    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }
}

/// Read-only view of a pending consent for IPC reply payloads.
#[derive(Clone, Debug)]
pub struct PendingSnapshot {
    pub tool: String,
    pub argv: Vec<String>,
    pub policy_diff: String,
    pub expires_at: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT_TTL: Duration = Duration::from_millis(1);
    const LONG_TTL: Duration = Duration::from_secs(60);

    #[test]
    fn token_expires() {
        // SPEC §4.4 "Consent UX": an idle token must not stay alive
        // forever. After the TTL lapses, `decide` returns Expired
        // (not NotFound) so the operator sees a precise error rather
        // than a generic "unknown token".
        let store = ConsentStore::new();
        let (token, _rx) = store.issue("rg", &["foo".into()], "diff".into(), SHORT_TTL);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(
            store.decide(token, ConsentDecision::Allow),
            Err(ConsentError::Expired)
        );
    }

    #[test]
    fn two_simultaneous_consents_do_not_collide() {
        // Independent tool calls each get their own UUID, so approving
        // B leaves A pending. Regression guard for any future
        // "current consent" singleton refactor.
        let store = ConsentStore::new();
        let (token_a, _rx_a) = store.issue("rg", &["foo".into()], "diff a".into(), LONG_TTL);
        let (token_b, _rx_b) = store.issue("cat", &["bar".into()], "diff b".into(), LONG_TTL);
        assert_ne!(token_a, token_b);
        store
            .decide(token_b, ConsentDecision::Allow)
            .expect("approve B");
        // A is still around; decide(A) should still succeed.
        store
            .decide(token_a, ConsentDecision::Allow)
            .expect("approve A after B");
        assert_eq!(store.pending_count(), 0);
    }

    #[tokio::test]
    async fn e2e_strict_profile_issues_and_resumes() {
        // Pure-Rust stand-in for the cross-process e2e the SPEC sketches:
        // one task parks the consent and awaits the receiver, a second
        // task simulates `sy approve <token>` by calling `decide`. The
        // receiver wakes with `Allow`, mirroring what the daemon's
        // permission handler will observe once the consent flow lands.
        use std::sync::Arc;
        let store = Arc::new(ConsentStore::new());
        let (token, rx) = store.issue(
            "/usr/bin/cat",
            &["/etc/shadow".into()],
            "strict requires every-call consent".into(),
            LONG_TTL,
        );

        let approver = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                // The approver runs on a separate task; the small sleep
                // simulates the wall-clock between the IPC reply
                // landing on the operator's terminal and them typing
                // `sy approve …`.
                tokio::time::sleep(Duration::from_millis(5)).await;
                store
                    .decide(token, ConsentDecision::Allow)
                    .expect("approve");
            })
        };

        let decision = rx.await.expect("receiver");
        assert_eq!(decision, ConsentDecision::Allow);
        approver.await.expect("approver");
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn cleanup_drops_expired_entries() {
        // `cleanup_expired` is the periodic GC the daemon will call;
        // verify it actually removes stale rows so the store doesn't
        // grow unbounded under a flood of un-approved consents.
        let store = ConsentStore::new();
        let (_token, _rx) = store.issue("rg", &[], "diff".into(), SHORT_TTL);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(store.pending_count(), 1);
        store.cleanup_expired();
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn snapshot_returns_pending_metadata() {
        // The daemon's IPC reply needs tool + argv + policy_diff to
        // surface to the operator; `snapshot` is the read-only access
        // path that doesn't consume the entry.
        let store = ConsentStore::new();
        let (token, _rx) = store.issue(
            "/usr/bin/rg",
            &["foo".into(), "bar".into()],
            "diff".into(),
            LONG_TTL,
        );
        let snap = store.snapshot(token).expect("snapshot");
        assert_eq!(snap.tool, "/usr/bin/rg");
        assert_eq!(snap.argv, vec!["foo".to_string(), "bar".to_string()]);
        assert_eq!(snap.policy_diff, "diff");
    }

    #[test]
    fn unknown_token_returns_not_found() {
        // Distinct error variant from `Expired` so the CLI can give a
        // useful message ("did you typo the UUID?" vs "rerun the tool
        // call").
        let store = ConsentStore::new();
        let stranger = Uuid::new_v4();
        assert_eq!(
            store.decide(stranger, ConsentDecision::Allow),
            Err(ConsentError::NotFound)
        );
    }
}
