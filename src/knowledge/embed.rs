//! Thin façade over `aiplane::workloads::embed`. Keeps the
//! pre-aiplane public API (`embed_one`, `embed_batch`,
//! `current_backend`, `current_hardware`,
//! `maybe_reexec_with_amd_env`) so `knowledge::daemon`,
//! `knowledge::cli`, and the MCP server compile unchanged while the
//! real implementation moved to `crate::aiplane`.
//!
//! Why the façade and not a re-export: the daemon and CLI manage a
//! process-global embedder (a `OnceLock<EmbedWorkload>`); the
//! Workload trait, by design, doesn't carry global state. The
//! singleton lives here so `aiplane::workloads::embed::EmbedWorkload`
//! stays trait-pure and tests can spin up their own instances.

use std::sync::OnceLock;

use anyhow::Result;

use crate::aiplane::registry::{Workload, WorkloadInput, WorkloadOutput};
use crate::aiplane::session::SessionPool;
use crate::aiplane::workloads::embed::{self as aip_embed, EmbedWorkload};

use super::{exit, KnowledgeError};

static WORKLOAD: OnceLock<EmbedWorkload> = OnceLock::new();
static POOL: OnceLock<SessionPool> = OnceLock::new();

fn workload() -> &'static EmbedWorkload {
    WORKLOAD.get_or_init(EmbedWorkload::new)
}

fn pool() -> &'static SessionPool {
    POOL.get_or_init(SessionPool::new)
}

/// `"vitisai"`, `"cpu"`, or `"unloaded"` if the model hasn't been
/// touched yet. Surfaced to the status snapshot so the waybar
/// tooltip + `sy knowledge status` can show which backend is engaged.
pub fn current_backend() -> &'static str {
    aip_embed::current_backend(workload())
}

/// Human-readable label for the actual hardware doing inference, e.g.
/// `"AMD NPU on 9 HX 370"`, `"AMD Ryzen AI 9 HX 370 (CPU)"`. Empty if
/// the embedder hasn't loaded yet.
pub fn current_hardware() -> String {
    aip_embed::current_hardware(workload())
}

/// Embed a batch of indexed passages. Each output vector is L2-normalised.
/// Adds the E5 `passage: ` prefix.
pub fn embed_batch(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    aip_embed::embed_passages(workload(), texts).map_err(|e| {
        KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("embed: {e}"),
        }
        .into()
    })
}

/// Embed a single search query (used by `sy knowledge search` and the
/// MCP server). Adds the E5 `query: ` prefix.
pub fn embed_one(text: &str) -> Result<Vec<f32>> {
    let w = workload();
    w.load(pool()).map_err(|e| KnowledgeError {
        code: exit::EMBEDDING_FAILED,
        msg: format!("embed load: {e}"),
    })?;
    let out = w
        .run(WorkloadInput::Text {
            text: text.to_string(),
        })
        .map_err(|e| KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("embed: {e}"),
        })?;
    match out {
        WorkloadOutput::Vector { vector } => Ok(vector),
        other => Err(KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("embed: unexpected output variant {other:?}"),
        }
        .into()),
    }
}
