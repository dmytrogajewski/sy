//! Knowledge-plane integration. Roadmap Step 30 (SPEC §3.3 item 10).
//!
//! The `sy file` plane reaches for `sy-knowledge.service` via the
//! [`crate::knowledge::cli::search_hits`] entry point and merges the
//! returned qdrant scores into the current pane's order. The trait
//! [`KnowledgeBackend`] sits between this module and `search_hits` so
//! the integration test (Step 30 DoD `end_to_end_with_stubbed_qdrant`)
//! can drive the merge path without a live daemon — the same shape
//! Step 21's [`crate::file::mcp::FileDaemonClient`] uses.
//!
//! All blocking dials are wrapped in [`tokio::task::spawn_blocking`]
//! and bounded by a 250 ms [`tokio::time::timeout`]; if the daemon
//! drops the connection or simply takes too long, [`query`] returns
//! `Ok(vec![])` so the palette path collapses cleanly to a
//! filename-only ranking (SPEC §6 risk-mitigation row 3 + the journey
//! J4 fallthrough beat).
//!
//! [`merge`] is a pure function over score vectors so the
//! `merge_orders_qdrant_first_then_filename` unit test pins the
//! ordering contract without any I/O.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::knowledge::ipc::HitRow;

/// SPEC §3.3 item 10 + SPEC §6 risk row 3 — the knowledge query is
/// bounded at 250 ms so a hung daemon never blocks the UI thread. On
/// timeout the call returns `Ok(vec![])` and the statusbar chip flips
/// to [`KnowledgeStatus::Timeout`] / [`KnowledgeStatus::Unreachable`].
pub const KNOWLEDGE_QUERY_BUDGET: Duration = Duration::from_millis(250);

/// Statusbar chip discriminator. Surfaces in
/// [`crate::file::view::statusbar::knowledge_chip`] so the operator
/// can see whether `sy-knowledge.service` is currently reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KnowledgeStatus {
    /// Daemon reachable on the most recent call (default).
    #[default]
    Reachable,
    /// Daemon socket disappeared / dial returned an error. The view
    /// layer paints the chip in dim-grey + the commandbar offers the
    /// `:index .` hint per SPEC §6.
    Unreachable,
    /// Daemon was reachable but the 250 ms budget elapsed before the
    /// response landed. Distinguished from `Unreachable` so the
    /// operator can tell "indexing is slow" from "daemon is down".
    Timeout,
}

/// Production-injection trait. The `search_hits` entry point is sync
/// (it builds its own runtime per call); routing it through this
/// trait lets the Step 30 integration test pin canned responses
/// without standing up the qdrant daemon.
pub trait KnowledgeBackend: Send + Sync {
    /// Perform a search; `prefix` scopes to a cwd-substring (per the
    /// `search_hits` contract). Returns the raw hits so [`query`] can
    /// post-process scores + path conversion.
    fn search(&self, query: &str, limit: usize, prefix: Option<&str>) -> Result<Vec<HitRow>>;
}

/// Production [`KnowledgeBackend`] — dials the live `sy-knowledge`
/// daemon socket via [`crate::knowledge::cli::search_hits`]. The
/// liveness probe + ipc dial are bundled inside `search_hits`, so
/// this impl is a single forwarding call.
pub struct RealKnowledgeBackend;

impl KnowledgeBackend for RealKnowledgeBackend {
    fn search(&self, query: &str, limit: usize, prefix: Option<&str>) -> Result<Vec<HitRow>> {
        crate::knowledge::cli::search_hits(query, limit, prefix)
    }
}

/// Outcome of a single [`query`] call. Pairs the (possibly empty)
/// hit list with the [`KnowledgeStatus`] the chip should flip to —
/// callers planted on the same async task so the chip + the hits
/// land in the same reducer turn (no race between two messages).
#[derive(Debug, Clone)]
pub struct QueryOutcome {
    /// `(absolute_path, score)` pairs ranked best-first. Empty when
    /// the backend errored, the budget timed out, or the worker
    /// panicked.
    pub hits: Vec<(PathBuf, f32)>,
    /// Chip status the reducer should write back. `Reachable` only
    /// on a successful response; `Unreachable` on backend error /
    /// worker panic; `Timeout` on the 250 ms budget firing.
    pub status: KnowledgeStatus,
}

/// Submit `q` to the knowledge backend scoped to `cwd`; returns a
/// [`QueryOutcome`] carrying the hit list + the chip status. The call
/// is bounded by [`KNOWLEDGE_QUERY_BUDGET`]; on timeout / backend
/// error / unreachable daemon, returns an empty hit list so the
/// caller can fall through to filename-only ranking (SPEC §6 risk
/// row 3).
///
/// `backend` is `'static` so the inner [`tokio::task::spawn_blocking`]
/// can move it into the worker thread without lifetime gymnastics —
/// production wraps [`RealKnowledgeBackend`] in an [`std::sync::Arc`];
/// tests inject a stub the same way.
pub async fn query(
    backend: std::sync::Arc<dyn KnowledgeBackend>,
    cwd: PathBuf,
    q: String,
    k: usize,
) -> Result<QueryOutcome> {
    // `search_hits` is sync (it opens its own runtime); push it onto a
    // blocking worker so the iced async runtime stays unblocked.
    let q_owned = q.clone();
    let cwd_str = cwd.to_string_lossy().into_owned();
    let join =
        tokio::task::spawn_blocking(move || backend.search(&q_owned, k, Some(cwd_str.as_str())));
    let bounded = tokio::time::timeout(KNOWLEDGE_QUERY_BUDGET, join).await;
    match bounded {
        Ok(Ok(Ok(hits))) => Ok(QueryOutcome {
            hits: hits
                .into_iter()
                .map(|h| (PathBuf::from(h.file_path), h.score))
                .collect(),
            status: KnowledgeStatus::Reachable,
        }),
        // Backend returned an error (daemon unreachable, wire error,
        // …) — collapse to empty so the caller falls through to
        // filename ranking. SPEC §6 risk row 3 / journey J4 fallback.
        Ok(Ok(Err(_))) => Ok(QueryOutcome {
            hits: Vec::new(),
            status: KnowledgeStatus::Unreachable,
        }),
        // spawn_blocking JoinError — the worker panicked. Treat as
        // unreachable so a single bad call doesn't poison the palette.
        Ok(Err(_)) => Ok(QueryOutcome {
            hits: Vec::new(),
            status: KnowledgeStatus::Unreachable,
        }),
        // Budget elapsed — surface the `Timeout` chip so the operator
        // can tell "indexing is slow" from "daemon is down".
        Err(_) => Ok(QueryOutcome {
            hits: Vec::new(),
            status: KnowledgeStatus::Timeout,
        }),
    }
}

/// Pure merge: qdrant-first (descending score), then filename-only
/// entries (descending score; nucleo emits negative-ish scores by
/// convention so they rank under qdrant's `[0,1]` band naturally).
/// Duplicates collapse with the qdrant score winning — the
/// `merge_orders_qdrant_first_then_filename` test pins this contract.
///
/// Stable: ties keep the input order so callers can pre-sort to
/// disambiguate.
pub fn merge(
    qdrant_hits: Vec<(PathBuf, f32)>,
    filename_hits: Vec<(PathBuf, f32)>,
) -> Vec<(PathBuf, f32)> {
    // Stable sort qdrant by descending score.
    let mut qdrant_sorted = qdrant_hits;
    qdrant_sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let qdrant_paths: std::collections::HashSet<PathBuf> =
        qdrant_sorted.iter().map(|(p, _)| p.clone()).collect();
    // Drop any filename hit whose path is already in the qdrant set
    // (qdrant score wins) — then stable-sort the remainder.
    let mut filename_sorted: Vec<(PathBuf, f32)> = filename_hits
        .into_iter()
        .filter(|(p, _)| !qdrant_paths.contains(p))
        .collect();
    filename_sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = qdrant_sorted;
    out.extend(filename_sorted);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    /// Roadmap Step 30 pin (DoD `merge_orders_qdrant_first_then_filename`):
    /// qdrant entries rank above filename-only entries; duplicates
    /// collapse with the qdrant score winning.
    #[test]
    fn merge_orders_qdrant_first_then_filename() {
        let p1 = PathBuf::from("/a.md");
        let p2 = PathBuf::from("/b.md");
        let p3 = PathBuf::from("/c.md");
        let qdrant = vec![(p1.clone(), 0.9), (p2.clone(), 0.8)];
        // filename hits include a dup (p1) + a unique entry (p3);
        // nucleo-style scores in the "boost" range (negative-ish).
        let filename = vec![(p3.clone(), -0.3), (p1.clone(), -0.5)];
        let merged = merge(qdrant, filename);
        let paths: Vec<PathBuf> = merged.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            paths,
            vec![p1.clone(), p2.clone(), p3.clone()],
            "qdrant entries must come first, then filename-only entries; \
             p1 must keep the qdrant score (collapsed dup)"
        );
        // Step 30 contract: the qdrant score wins on collapse.
        let p1_score = merged
            .iter()
            .find(|(p, _)| p == &p1)
            .map(|(_, s)| *s)
            .expect("p1 present");
        assert!(
            (p1_score - 0.9).abs() < f32::EPSILON,
            "p1 must keep the qdrant score 0.9, got {p1_score}"
        );
    }

    /// Stub backend that always returns `Err` — the unreachable-daemon
    /// case the DoD `daemon_unreachable_returns_empty_in_250ms`
    /// asserts.
    struct UnreachableBackend;
    impl KnowledgeBackend for UnreachableBackend {
        fn search(&self, _q: &str, _k: usize, _prefix: Option<&str>) -> Result<Vec<HitRow>> {
            anyhow::bail!("daemon unreachable (test stub)")
        }
    }

    /// Roadmap Step 30 pin (DoD `daemon_unreachable_returns_empty_in_250ms`):
    /// when the backend errors (mimics `sy-knowledge.service` being
    /// down), [`query`] returns an empty hit list AND completes
    /// inside the 250 ms budget AND flips the status to
    /// [`KnowledgeStatus::Unreachable`]. The journey-J4 fallback rides
    /// on all three — "no hits" must show up fast enough that the
    /// user isn't looking at a hung palette, and the chip must dim-
    /// grey so the operator knows why.
    #[tokio::test]
    async fn daemon_unreachable_returns_empty_in_250ms() {
        let backend: Arc<dyn KnowledgeBackend> = Arc::new(UnreachableBackend);
        let cwd = PathBuf::from("/tmp/step30");
        let start = Instant::now();
        let res = query(backend, cwd, "hello".to_string(), 12).await;
        let elapsed = start.elapsed();
        let outcome = res.expect("query must collapse to Ok on backend error");
        assert!(
            outcome.hits.is_empty(),
            "unreachable backend must produce an empty hit list, got {:?}",
            outcome.hits
        );
        assert_eq!(
            outcome.status,
            KnowledgeStatus::Unreachable,
            "backend error must flip the chip to Unreachable"
        );
        assert!(
            elapsed < KNOWLEDGE_QUERY_BUDGET + Duration::from_millis(100),
            "query must complete inside 250 ms + slack, elapsed={elapsed:?}"
        );
    }

    /// Stub backend that sleeps past the 250 ms ceiling. Lets us
    /// confirm the timeout arm fires instead of blocking forever.
    struct SlowBackend;
    impl KnowledgeBackend for SlowBackend {
        fn search(&self, _q: &str, _k: usize, _prefix: Option<&str>) -> Result<Vec<HitRow>> {
            std::thread::sleep(Duration::from_millis(500));
            Ok(vec![])
        }
    }

    /// The 250 ms budget fires even when the backend is alive but
    /// slow — defends against a backend that hangs but never errors.
    /// Status flips to [`KnowledgeStatus::Timeout`] so the chip can
    /// surface "indexing is slow" distinct from "daemon is down".
    #[tokio::test]
    async fn slow_backend_times_out_inside_budget() {
        let backend: Arc<dyn KnowledgeBackend> = Arc::new(SlowBackend);
        let cwd = PathBuf::from("/tmp/step30-slow");
        let start = Instant::now();
        let res = query(backend, cwd, "hello".to_string(), 12).await;
        let elapsed = start.elapsed();
        let outcome = res.expect("timeout must collapse to Ok");
        assert!(
            outcome.hits.is_empty(),
            "timeout must produce an empty hit list"
        );
        assert_eq!(
            outcome.status,
            KnowledgeStatus::Timeout,
            "slow backend must flip the chip to Timeout"
        );
        assert!(
            elapsed < KNOWLEDGE_QUERY_BUDGET + Duration::from_millis(100),
            "slow backend must be cut off at the 250 ms budget + slack, elapsed={elapsed:?}"
        );
    }
}
