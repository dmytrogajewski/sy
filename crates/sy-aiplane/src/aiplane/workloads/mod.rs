//! All concrete `Workload` impls + the `register_all` boot helper.
//!
//! Adding a new workload: write `workloads/<name>.rs` with a `pub
//! struct <Name>Workload` implementing `Workload`, then add one line
//! to `register_all`. The Workload skill (`.claude/commands/workload.md`)
//! walks the full 8-artefact checklist.

use std::path::PathBuf;
use std::sync::Arc;

use super::registry::{Registry, WorkloadKind};
use super::session::SessionPool;

pub mod embed;
/// Deterministic Workload impl used by aiplane / ipc / worker tests
/// only. Gated out of non-test builds so the scaffolded helpers it
/// exposes (`fake_vector`, `fake_score`, alternate constructors)
/// do not register as dead code in release / clippy.
#[cfg(test)]
pub mod fake;
pub mod ocr;
pub mod rerank;
pub mod stt;
pub mod vad;
/// Whisper log-mel feature extractor — see module docs. Used by the
/// Stt workload to turn 16 kHz PCM into the encoder's `[1,80,3000]`
/// input tensor.
pub mod whisper_mel;

/// Sentence-embedding vector dim. e5-base is 768-dim. The qdrant
/// collection schema is keyed on this — changing the constant requires
/// `sy knowledge sync --yes` to recreate the collection at the new dim.
pub const VECTOR_DIM: usize = 768;

/// Default cap for ONNX Runtime's CPU execution-provider intra-op thread
/// pool on every NPU workload session.
///
/// The VitisAI EP runs the heavy matmul subgraph on the AIE; ORT's CPU EP
/// only handles the fallback glue ops (embedding `Gather`, partition-boundary
/// casts). Left uncapped, that pool defaults to *every logical core* and pins
/// the whole machine during a catch-up pass even though the NPU is the actual
/// throughput bottleneck. A small pool keeps the glue parallel without the
/// load-average blowup. Override with `SY_NPU_INTRA_THREADS` (clamped to >= 1).
pub const DEFAULT_NPU_INTRA_THREADS: usize = 4;

/// Resolve the NPU session intra-op thread cap, honouring the
/// `SY_NPU_INTRA_THREADS` env override (flags > env > default, per CLIG).
pub fn npu_intra_threads() -> usize {
    parse_intra_threads(std::env::var("SY_NPU_INTRA_THREADS").ok().as_deref())
}

/// Pure core of [`npu_intra_threads`]: parse the override, clamp to >= 1, and
/// fall back to [`DEFAULT_NPU_INTRA_THREADS`] when unset or unparseable.
fn parse_intra_threads(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.max(1))
        .unwrap_or(DEFAULT_NPU_INTRA_THREADS)
}

/// Boot the workload registry with every kind sy supports. Called once
/// from `daemon::run` before the req worker starts.
pub fn register_all(pool: Arc<SessionPool>) -> Registry {
    let mut reg = Registry::new(pool);
    reg.register(Arc::new(embed::EmbedWorkload::new()));
    reg.register(Arc::new(rerank::RerankWorkload::new()));
    reg.register(Arc::new(vad::VadWorkload::new()));
    reg.register(Arc::new(stt::SttWorkload::new()));
    reg.register(Arc::new(ocr::OcrWorkload::new()));
    reg
}

/// Best-effort CPU model name from `/proc/cpuinfo`. Used in hardware
/// labels surfaced to `sy aiplane status` and the waybar tooltip.
pub fn detect_cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_else(|| "CPU".to_string())
}

/// SKU-stable NPU label. The lspci vendor string is `Strix/Krackan/Strix
/// Halo Neural Processing Unit` — useless across SKUs. The CPU model name
/// pins it down (`AMD Ryzen AI 9 HX 370` → Strix Point, etc.).
pub fn detect_npu_label() -> String {
    let cpu = detect_cpu_model();
    let short = cpu
        .strip_prefix("AMD Ryzen AI ")
        .map(|s| {
            s.split_once(" w/ ")
                .map(|(left, _)| left.to_string())
                .unwrap_or_else(|| s.to_string())
        })
        .unwrap_or(cpu);
    if short.trim().is_empty() {
        "AMD NPU".to_string()
    } else {
        format!("AMD NPU on {short}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intra_threads_defaults_when_unset() {
        assert_eq!(parse_intra_threads(None), DEFAULT_NPU_INTRA_THREADS);
    }

    #[test]
    fn intra_threads_honours_valid_override() {
        assert_eq!(parse_intra_threads(Some("8")), 8);
        assert_eq!(parse_intra_threads(Some("  2 ")), 2);
    }

    #[test]
    fn intra_threads_clamps_zero_to_one() {
        // A 0-thread pool is invalid for ORT; clamp up so the cap never
        // accidentally disables the CPU EP entirely.
        assert_eq!(parse_intra_threads(Some("0")), 1);
    }

    #[test]
    fn intra_threads_falls_back_on_garbage() {
        assert_eq!(parse_intra_threads(Some("")), DEFAULT_NPU_INTRA_THREADS);
        assert_eq!(parse_intra_threads(Some("lots")), DEFAULT_NPU_INTRA_THREADS);
        assert_eq!(parse_intra_threads(Some("-3")), DEFAULT_NPU_INTRA_THREADS);
    }
}

/// On-disk artifacts a workload's `load()` looks for, so `sy doctor` can
/// pre-flight them instead of waiting for the daemon to crash-loop on a
/// wiped `~/.cache` (BUG-20260927-0119). Stems and layouts come from the
/// workloads themselves — this module never re-derives a path.
#[derive(Debug, Clone)]
pub struct ArtifactSet {
    pub kind: WorkloadKind,
    /// Preferred ONNX (NPU-compiled / quantised).
    pub model: PathBuf,
    /// ONNX that keeps the workload loadable on the CPU EP when `model`
    /// is gone. `None` when the workload has no CPU path at all.
    pub fallback_model: Option<PathBuf>,
    pub tokenizer: PathBuf,
    /// Command that rebuilds the set.
    pub prep: String,
    /// False = losing this set costs quality, not service. `rerank` is the
    /// case: it re-scores search hits, so `sy doctor` must not shout
    /// `fail` (nor the daemon suspend indexing) over a missing reranker.
    pub indexing_critical: bool,
}

/// Every workload the knowledge daemon raises at start-up. Ordered as
/// `knowledge::daemon::REQUIRED_WORKLOADS` so `sy doctor --json` is stable.
pub fn artifact_sets() -> Vec<ArtifactSet> {
    let (embed_model, embed_tokenizer) = embed::required_paths();
    let (rerank_model, rerank_tokenizer) = rerank::required_paths();
    vec![
        ArtifactSet {
            kind: WorkloadKind::Embed,
            model: embed_model,
            fallback_model: Some(embed::fallback_model_path()),
            tokenizer: embed_tokenizer,
            prep: "python ~/sources/sy/scripts/prep_npu_workload.py --workload embed".into(),
            indexing_critical: true,
        },
        ArtifactSet {
            kind: WorkloadKind::Rerank,
            model: rerank_model,
            fallback_model: None,
            tokenizer: rerank_tokenizer,
            prep: "python ~/sources/sy/scripts/prep_npu_workload.py --workload rerank".into(),
            indexing_critical: false,
        },
    ]
}
