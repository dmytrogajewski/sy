//! Knowledge-plane slice of the [`super::State`]. Roadmap Step 30
//! (SPEC §3.3 item 10 + journey J4).
//!
//! The reducer in [`crate::file::app::update`] writes this slice when:
//!
//! * the operator fires `:k <query>` → palette path queues a
//!   [`crate::file::app::Message::KnowledgeQuery`]; the reducer
//!   stamps [`KnowledgeState::last_query`].
//! * the async [`crate::file::search::knowledge::query`] task
//!   resolves → the reducer stamps [`KnowledgeState::last_hits`].
//! * the dial fails (daemon unreachable) → the reducer flips
//!   [`KnowledgeState::status`] to
//!   [`crate::file::search::knowledge::KnowledgeStatus::Unreachable`]
//!   so the statusbar chip dim-greys and the commandbar paints the
//!   SPEC §6 `:index .` hint.
//!
//! Pure data — no I/O. Tests over this module exercise the reducer
//! arms without standing up the knowledge daemon.

use std::path::PathBuf;

use crate::file::search::knowledge::KnowledgeStatus;

/// In-memory slice owned by [`super::State`]. Default = `Reachable`
/// with no prior query so the chip paints in the "ready" colour on
/// first paint (journey J2 → J4 hand-off).
#[derive(Debug, Default, Clone)]
pub struct KnowledgeState {
    /// Current daemon reachability — read by
    /// [`crate::file::view::statusbar::knowledge_chip`].
    pub status: KnowledgeStatus,
    /// The query string the operator last fired via `:k`. `None`
    /// until the first query lands; surviving across queries lets
    /// the chip tooltip surface "last: tuned override".
    pub last_query: Option<String>,
    /// Merged hit list from the last successful query. Ordered
    /// qdrant-first, filename-second per
    /// [`crate::file::search::knowledge::merge`]. Empty until the
    /// first response lands; empty after an `Unreachable` /
    /// `Timeout` reducer arm.
    pub last_hits: Vec<(PathBuf, f32)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 30 invariant: the default slice paints the "ready" chip
    /// without any prior query / hits. Pinning the default value
    /// keeps the journey-J2 first-paint shape stable.
    #[test]
    fn default_is_reachable_with_no_history() {
        let s = KnowledgeState::default();
        assert_eq!(s.status, KnowledgeStatus::Reachable);
        assert!(s.last_query.is_none());
        assert!(s.last_hits.is_empty());
    }
}
