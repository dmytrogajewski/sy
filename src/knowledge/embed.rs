//! Thin façade over the aiplane embed worker. Exposes
//! `embed_one`, `embed_batch`, `current_backend`, and
//! `current_hardware` so `knowledge::daemon`, `knowledge::cli`, and
//! the MCP server route their embed traffic through one place. The
//! real ONNX session lives in `sy aiplane worker --kind embed`.
//!
//! There is exactly one path: callers route through the supervisor.
//! When the supervisor isn't running (e.g. a CLI process that hasn't
//! gone through the daemon), the helpers return an error rather than
//! silently spinning up an in-process ORT session — that mode caused
//! the multi-context-per-process collisions that the worker split was
//! designed to eliminate.

use anyhow::Result;

use crate::aiplane::registry::{WorkloadInput, WorkloadKind, WorkloadOutput, WorkloadState};
use crate::aiplane::supervisor;

use super::{exit, KnowledgeError};

/// `"vitisai"`, `"cpu"`, `"loading"`, `"failed"`, `"not-prepared"`,
/// `"unavailable"`, or `"unloaded"` if the supervisor hasn't reported
/// yet. Surfaced to the status snapshot so the waybar tooltip +
/// `sy knowledge status` reflect the embed worker's true state.
pub fn current_backend() -> &'static str {
    let Some(sup) = supervisor::current() else {
        return "unloaded";
    };
    match sup.all_health().get(&WorkloadKind::Embed) {
        Some(Some(h)) => match &h.state {
            WorkloadState::Ready { backend } => match backend.as_str() {
                "vitisai" => "vitisai",
                "cpu" => "cpu",
                _ => "vitisai",
            },
            WorkloadState::Loading => "loading",
            WorkloadState::Failed { .. } => "failed",
            WorkloadState::NotPrepared => "not-prepared",
            WorkloadState::Unavailable => "unavailable",
        },
        _ => "unloaded",
    }
}

/// Human-readable label for the actual hardware doing inference,
/// e.g. `"AMD NPU on 9 HX 370"`, `"AMD Ryzen AI 9 HX 370 (CPU)"`.
/// Synthesised from the worker's reported backend + the host CPU
/// model.
pub fn current_hardware() -> String {
    match current_backend() {
        "vitisai" => format!(
            "AMD NPU on {}",
            crate::aiplane::workloads::detect_cpu_model()
                .strip_prefix("AMD Ryzen AI ")
                .unwrap_or("?")
        ),
        "cpu" => format!("{} (CPU)", crate::aiplane::workloads::detect_cpu_model()),
        _ => String::new(),
    }
}

/// Embed a batch of indexed passages. Each output vector is
/// L2-normalised. Adds the E5 `passage: ` prefix; routes through the
/// embed worker via one batched IPC.
pub fn embed_batch(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let sup = require_supervisor("embed batch")?;
    let inputs: Vec<WorkloadInput> = texts
        .iter()
        .map(|t| WorkloadInput::Text {
            text: format!("passage: {t}"),
        })
        .collect();
    let outputs = sup
        .run_batch(WorkloadKind::Embed, inputs)
        .map_err(|e| KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("embed worker: {e:#}"),
        })?;
    let mut out = Vec::with_capacity(outputs.len());
    for o in outputs {
        match o {
            WorkloadOutput::Vector { vector } => out.push(vector),
            other => {
                return Err(KnowledgeError {
                    code: exit::EMBEDDING_FAILED,
                    msg: format!("embed: unexpected output {other:?}"),
                }
                .into());
            }
        }
    }
    Ok(out)
}

/// Embed a single search query (used by `sy knowledge search` and
/// the MCP server). The worker applies the E5 `query: ` prefix when
/// none is present in the caller's input.
pub fn embed_one(text: &str) -> Result<Vec<f32>> {
    let sup = require_supervisor("embed")?;
    let outputs = sup
        .run_batch(
            WorkloadKind::Embed,
            vec![WorkloadInput::Text {
                text: text.to_string(),
            }],
        )
        .map_err(|e| KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("embed worker: {e:#}"),
        })?;
    match outputs.into_iter().next() {
        Some(WorkloadOutput::Vector { vector }) => Ok(vector),
        Some(other) => Err(KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("embed: unexpected output variant {other:?}"),
        }
        .into()),
        None => Err(KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: "embed: worker returned empty batch".into(),
        }
        .into()),
    }
}

fn require_supervisor(call: &str) -> Result<std::sync::Arc<supervisor::Supervisor>> {
    supervisor::current().ok_or_else(|| {
        KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("{call}: aiplane supervisor not running"),
        }
        .into()
    })
}
