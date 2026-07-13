//! Step 30 integration test — knowledge backend stub round-trip.
//!
//! Pins SPEC §3.3 item 10 + the roadmap Step 30 DoD bullet
//! `end_to_end_with_stubbed_qdrant`: a [`KnowledgeBackend`] stub
//! returning canned hits drives [`query`] through its full pipeline
//! (spawn_blocking + timeout + `HitRow` → `(PathBuf, f32)`
//! conversion) without standing up `sy-knowledge.service`. The chip
//! flips to `Reachable`; the merge orders qdrant entries above the
//! filename hits the e2e plants.
//!
//! Lives at the test-crate root (rather than as a unit test in
//! `src/file/search/knowledge.rs`) because it exercises the trait
//! object surface the production reducer drives, which the
//! `KnowledgeBackend` SPEC §6 risk-mitigation row pins on.

// `crate::knowledge::ipc::HitRow` + `crate::knowledge::cli::search_hits`
// live behind the `gui-iced` feature in production via the bin's
// `mod knowledge` declaration. The integration-test build doesn't
// require gui-iced because the knowledge surface itself is headless
// (SPEC §3.3 item 10) — same rationale as `src/file/search/mod.rs`'s
// "NOT `#[cfg(feature = "gui-iced")]` gated" docstring.

#[path = "../src/file/search/knowledge.rs"]
mod file_search_knowledge;

/// Step 30 — minimal `crate::knowledge` shim. The `#[path]`-imported
/// `file_search_knowledge` mirror references
/// `crate::knowledge::ipc::HitRow` + `crate::knowledge::cli::search_hits`
/// inside `RealKnowledgeBackend::search`. The e2e never drives the
/// real backend (it injects a `StubKnowledgeBackend`), but the
/// compile-time references still have to resolve; the shim mirrors
/// the wire shape — same fields as `src/aiplane/ipc.rs::HitRow`.
pub mod knowledge {
    pub mod ipc {
        #[derive(Debug, Clone)]
        pub struct HitRow {
            pub score: f32,
            pub chunk_id: String,
            pub file_path: String,
            pub chunk_index: u32,
            pub chunk_text: String,
            pub embed_score: Option<f32>,
        }
    }
    pub mod cli {
        use super::ipc::HitRow;
        use anyhow::Result;
        pub fn search_hits(_q: &str, _k: usize, _prefix: Option<&str>) -> Result<Vec<HitRow>> {
            anyhow::bail!("knowledge shim: e2e injects a stub instead")
        }
    }
}

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::file_search_knowledge::{
    merge, query, KnowledgeBackend, KnowledgeStatus, RealKnowledgeBackend, KNOWLEDGE_QUERY_BUDGET,
};
use crate::knowledge::ipc::HitRow;

/// Touch the [`RealKnowledgeBackend`] type-of so the production
/// surface is reachable from the integration-test crate's dead-code
/// pass even when no test calls into the real daemon (the e2e stubs
/// the backend instead).
#[allow(dead_code)]
fn _touch_real_backend() {
    let _ = std::marker::PhantomData::<RealKnowledgeBackend>;
}

/// Step 30 DoD bullet `end_to_end_with_stubbed_qdrant`: a stub
/// backend returns canned hits and they appear in the resolved
/// `(PathBuf, f32)` list — the qdrant integration is reachable
/// end-to-end through the `KnowledgeBackend` trait. The
/// `Reachable` chip status is asserted alongside, mirroring the
/// SPEC §6 risk-mitigation row.
#[tokio::test]
async fn end_to_end_with_stubbed_qdrant() {
    /// Stub backend returning two canned `HitRow`s — what the
    /// real qdrant pipeline would emit for an embed → top-N pass
    /// over an indexed cwd.
    struct StubBackend {
        hits: Vec<HitRow>,
    }
    impl KnowledgeBackend for StubBackend {
        fn search(&self, _q: &str, _k: usize, _prefix: Option<&str>) -> Result<Vec<HitRow>> {
            Ok(self.hits.clone())
        }
    }

    let canned = vec![
        HitRow {
            score: 0.91,
            chunk_id: String::new(),
            file_path: "/sources/sy/src/aiplane/ipc.rs".to_owned(),
            chunk_index: 0,
            chunk_text: "tuned override carrier".to_owned(),
            embed_score: Some(0.88),
        },
        HitRow {
            score: 0.83,
            chunk_id: String::new(),
            file_path: "/sources/sy/src/file/app.rs".to_owned(),
            chunk_index: 0,
            chunk_text: "Step 30 reducer arm".to_owned(),
            embed_score: Some(0.81),
        },
    ];
    let backend: Arc<dyn KnowledgeBackend> = Arc::new(StubBackend {
        hits: canned.clone(),
    });

    let start = Instant::now();
    let outcome = query(
        backend,
        PathBuf::from("/sources/sy"),
        "tuned override".to_owned(),
        12,
    )
    .await
    .expect("stub backend must produce an Ok outcome");
    let elapsed = start.elapsed();

    // Step 30 timeout DoD — the call MUST complete inside the
    // 250 ms budget regardless of backend latency. A stub backend
    // is fast, so this assertion mostly defends against a future
    // pipeline regression that adds blocking work.
    assert!(
        elapsed < KNOWLEDGE_QUERY_BUDGET + Duration::from_millis(100),
        "stub-backed query must complete inside the 250 ms budget + slack, elapsed={elapsed:?}"
    );

    assert_eq!(
        outcome.status,
        KnowledgeStatus::Reachable,
        "stub backend (Ok) must flip the chip to Reachable, got {:?}",
        outcome.status
    );
    assert_eq!(
        outcome.hits.len(),
        2,
        "stub backend must surface both canned hits"
    );
    // First hit must be the top-scored canned entry — ordering
    // preserved by `query` (the inner ladder is a single-pass
    // map without reordering).
    let (top_path, top_score) = &outcome.hits[0];
    assert_eq!(
        top_path,
        &PathBuf::from("/sources/sy/src/aiplane/ipc.rs"),
        "top hit must match the canned 0.91 entry"
    );
    assert!(
        (*top_score - 0.91).abs() < f32::EPSILON,
        "top hit must preserve the canned score, got {top_score}"
    );

    // Pin the `merge` contract too — qdrant entries rank above
    // filename hits for paths the qdrant set doesn't already
    // cover, and duplicates collapse with the qdrant score.
    let filename_only = vec![
        (PathBuf::from("/sources/sy/Cargo.toml"), -0.4),
        // dup: same path as qdrant hit #1; the qdrant score must
        // win on collapse.
        (PathBuf::from("/sources/sy/src/aiplane/ipc.rs"), -0.7),
    ];
    let merged = merge(outcome.hits.clone(), filename_only);
    assert_eq!(merged.len(), 3, "merge must surface 3 unique paths");
    let merged_paths: Vec<&PathBuf> = merged.iter().map(|(p, _)| p).collect();
    assert_eq!(
        merged_paths[0],
        &PathBuf::from("/sources/sy/src/aiplane/ipc.rs"),
        "qdrant top hit must rank first"
    );
    assert_eq!(
        merged_paths[1],
        &PathBuf::from("/sources/sy/src/file/app.rs"),
        "second qdrant hit must rank second"
    );
    assert_eq!(
        merged_paths[2],
        &PathBuf::from("/sources/sy/Cargo.toml"),
        "filename-only entry must rank below all qdrant hits"
    );
    // The qdrant score must have won on the collapse.
    let dup_score = merged
        .iter()
        .find(|(p, _)| p == &PathBuf::from("/sources/sy/src/aiplane/ipc.rs"))
        .map(|(_, s)| *s)
        .expect("dup path present");
    assert!(
        (dup_score - 0.91).abs() < f32::EPSILON,
        "qdrant score (0.91) must win over filename score on collapse, got {dup_score}"
    );
}
