//! All concrete `Workload` impls + the `register_all` boot helper.
//!
//! Adding a new workload: write `workloads/<name>.rs` with a `pub
//! struct <Name>Workload` implementing `Workload`, then add one line
//! to `register_all`. The Workload skill (`.claude/commands/workload.md`)
//! walks the full 8-artefact checklist.

use std::sync::Arc;

use super::registry::Registry;
use super::session::SessionPool;

pub mod embed;
pub mod fake;
pub mod ocr;
pub mod rerank;
pub mod stt;
pub mod vad;

/// Sentence-embedding vector dim. e5-base is 768-dim. The qdrant
/// collection schema is keyed on this — changing the constant requires
/// `sy knowledge sync --yes` to recreate the collection at the new dim.
pub const VECTOR_DIM: usize = 768;

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
