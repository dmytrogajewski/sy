//! Shared legacy aiplane and knowledge IPC payloads.
//!
//! These values use `sy-core` workload vocabulary and are independent of
//! the aiplane runtime and its Unix socket implementation.

pub use crate::knowledge::HitRow;
use serde::{Deserialize, Serialize};
use sy_core::{
    Priority,
    workload::{WorkloadInput, WorkloadKind, WorkloadOutput},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// Re-read sy.toml `[knowledge]` sources + schedule.
    RefreshSources,
    /// Run an incremental index pass right now.
    IndexNow,
    /// Drop the qdrant collection and re-embed everything.
    FullResync,
    /// Re-read schedule from sy.toml (subset of RefreshSources).
    ReloadSchedule,
    /// Re-walk discover roots + shallow-home for `qdr.toml` manifests; diff
    /// the active set, register/unregister watchers + qdrant points.
    RescanDiscovery,
    /// Stop firing scheduled / FS-tickle / IPC-IndexNow passes until
    /// `Resume`. User-driven `sy knowledge index/sync/search` calls
    /// (which run in the CLI process, not the daemon) bypass this.
    Pause,
    /// Resume from a paused state. Triggers a single catch-up pass.
    Resume,
    /// Idempotent toggle (used by the waybar middle-click handler).
    TogglePause,
    /// Cooperatively cancel any in-flight pass. Daemon stays paused if it
    /// was paused before. Files already embedded keep their qdrant points.
    Cancel,
    /// Graceful shutdown.
    Shutdown,
}

/// Pre-search payload filter (REQ-2). Compiled into a qdrant `Filter`
/// downstream (Step 7); here it is pure wire format. Every field is
/// optional/empty by default so an absent filter serializes compactly
/// and pre-Step-6 payloads still deserialize.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchFilter {
    /// Inclusive lower bound, ISO-8601 / RFC-3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_from: Option<String>,
    /// Inclusive upper bound, ISO-8601 / RFC-3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_to: Option<String>,
    /// Sender match set (any-of).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<String>,
    /// `SourceKind` kebab strings (any-of).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kind: Vec<String>,
    /// Source names that must be present (must).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_sources: Vec<String>,
    /// Source names that must be absent (must-not).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_sources: Vec<String>,
    /// `SourceKind` kebab strings that must be absent (must-not). Carries
    /// the REQ-1 default-exclude of `claude-transcripts`, injected by the
    /// CLI/MCP boundary unless the caller opts that kind in. Additive +
    /// defaulted so pre-Step-11 payloads still deserialize.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_kinds: Vec<String>,
}

/// Request-response op. Distinct from `Op` because callers wait for
/// the daemon's reply on the same UnixStream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "kebab-case")]
pub enum Req {
    /// Generic workload dispatch. The daemon validates the input
    /// variant matches the workload's expected shape and returns
    /// `Resp::Run { output }` or `Resp::Error`.
    Run {
        workload: WorkloadKind,
        input: WorkloadInput,
    },
    /// Composite: embed `query` via the Embed workload, then
    /// qdrant top-k cosine search. Kept as a single round-trip so
    /// search consumers don't pay 2× IPC.
    Search {
        query: String,
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// SPEC §4.3 scheduler class for the embed step. Older
        /// callers default to `Interactive`. The bridge stamps this
        /// from the v1 envelope's `priority` field at translation
        /// time.
        #[serde(default = "default_search_priority")]
        priority: Priority,
        /// REQ-2 pre-search filter. `None` ⇒ unfiltered (back-compat).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<SearchFilter>,
        /// REQ-6 abstain cutoff in `[0,1]`. `None` ⇒ server default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abstain_threshold: Option<f32>,
    },
    /// Two-stage retrieval: embed → qdrant top-`candidates` →
    /// bge-reranker cross-encoder scores every (query, doc) pair →
    /// truncate to `limit`. Done daemon-side so the client doesn't
    /// pay one IPC per pair, and so the NPU mutex is held across the
    /// whole rerank pass (no re-entry cost between pairs).
    SearchRerank {
        query: String,
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// Top-N pulled from qdrant before reranking. Default 30 in
        /// the CLI / MCP surfaces; tune up for higher recall on long
        /// tails, down for tighter latency.
        candidates: usize,
        /// SPEC §4.3 scheduler class for the embed step. Same
        /// semantics as on [`Req::Search`].
        #[serde(default = "default_search_priority")]
        priority: Priority,
        /// REQ-2 pre-search filter. `None` ⇒ unfiltered (back-compat).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<SearchFilter>,
        /// REQ-6 abstain cutoff in `[0,1]`. `None` ⇒ server default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abstain_threshold: Option<f32>,
    },
    /// REQ-10 fetch-by-id: resolve a single chunk's full (uncapped)
    /// text + payload by the stable `chunk_id` a bounded search result
    /// carries. `chunk_id` is **qdrant's own point id**, passed through from
    /// the search response — callers must not re-derive it from
    /// `file_path`/`chunk_index`, because pipelines hash their own domain
    /// keys (`chunk::point_id` is one such producer, not the contract);
    /// the daemon answers with [`Resp::Chunk`].
    GetChunk { chunk_id: String },
}

/// Default priority for legacy [`Req::Search`] / [`Req::SearchRerank`]
/// callers that don't stamp the field. The bridge overrides this
/// from the v1 envelope at translation time; only direct legacy-Req
/// constructors (CLI pre-Step-10) land on this default.
pub fn default_search_priority() -> Priority {
    Priority::Interactive
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "kebab-case")]
pub enum Resp {
    Run {
        output: WorkloadOutput,
    },
    Search {
        hits: Vec<HitRow>,
        /// REQ-6 calibrated confidence in `[0,1]`. Computed in Step 12;
        /// here it is plumbing-only. Legacy payloads without the field
        /// deserialize to the neutral default `1.0` (uncalibrated /
        /// not-abstained) so a Step-6 daemon and an older CLI interop.
        #[serde(default = "default_search_confidence")]
        confidence: f32,
        /// REQ-6 abstain flag. `false` ⇒ a normal (non-abstained)
        /// response; legacy payloads default to `false`.
        #[serde(default)]
        abstained: bool,
    },
    /// REQ-10 fetch-by-id reply: the full (uncapped) chunk for a
    /// `Req::GetChunk`. `chunk` is `None` when no point matched the id.
    Chunk {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chunk: Option<ChunkRow>,
    },
    Error {
        msg: String,
    },
}

/// Full (uncapped) chunk returned by [`Resp::Chunk`] for a fetch-by-id.
/// Mirrors the SPEC §4 `knowledge_get_chunk` shape — the `payload` carries
/// the source-kind metadata, `text` is the complete chunk text (never the
/// MCP-capped form).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRow {
    pub chunk_id: String,
    pub file_path: String,
    pub chunk_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    pub text: String,
}

/// Neutral confidence for [`Resp::Search`] payloads that predate the
/// Step-6 wire format (and for producers that don't calibrate yet).
pub fn default_search_confidence() -> f32 {
    1.0
}
