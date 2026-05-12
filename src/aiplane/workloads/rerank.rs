//! Cross-encoder reranker. Target model: `BAAI/bge-reranker-v2-m3`
//! (XLM-RoBERTa pair classifier). Static-shape (1, 512) for q+d concat,
//! sigmoid baked into the ONNX graph so the output is a scalar
//! relevance score in [0, 1].
//!
//! Wiring: `aiplane::workloads::register_all` registers this workload
//! so the daemon will dispatch `Req::Run { Rerank, TextPair }`. The
//! prep script (`scripts/prep_npu_workload.py --workload rerank`) is
//! the path that produces the model artifact at
//! `~/.cache/sy/aiplane/bge-reranker-v2-m3/`.
//!
//! Status: scaffolded — the `Workload` impl, CLI, and MCP plumbing
//! are in place; running it without a prepared model returns a clear
//! `not prepared` error pointing at the prep script. See the
//! `/workload` skill for the full registration checklist.

use std::sync::Mutex;

use anyhow::Result;

use super::super::registry::{
    cache_root, Workload, WorkloadHealth, WorkloadInput, WorkloadKind, WorkloadOutput,
};
use super::super::session::SessionPool;

const MODEL_STEM: &str = "bge-reranker-v2-m3";

pub struct RerankWorkload {
    loaded: Mutex<bool>,
}

impl RerankWorkload {
    pub fn new() -> Self {
        Self {
            loaded: Mutex::new(false),
        }
    }
}

impl Default for RerankWorkload {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for RerankWorkload {
    fn kind(&self) -> WorkloadKind {
        WorkloadKind::Rerank
    }

    fn model_stem(&self) -> &'static str {
        MODEL_STEM
    }

    fn load(&self, _pool: &SessionPool) -> Result<()> {
        let dir = cache_root().join(MODEL_STEM);
        let model_path = dir.join(format!("{MODEL_STEM}.bf16.onnx"));
        if !model_path.is_file() {
            anyhow::bail!(
                "rerank model not prepared at {}\nBuild it with:\n  \
                 source /opt/AMD/ryzenai/venv/bin/activate && \
                 python scripts/prep_npu_workload.py --workload rerank",
                model_path.display()
            );
        }
        *self.loaded.lock().expect("rerank loaded poisoned") = true;
        Ok(())
    }

    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput> {
        match input {
            WorkloadInput::TextPair { .. } => {}
            other => anyhow::bail!("rerank: expected TextPair input, got {other:?}"),
        }
        anyhow::bail!(
            "rerank: ONNX session not yet implemented; \
             prep the model with `python scripts/prep_npu_workload.py --workload rerank` \
             then fill in `run()` per the /workload skill."
        )
    }

    fn unload(&self) {
        *self.loaded.lock().expect("rerank loaded poisoned") = false;
    }

    fn health(&self) -> WorkloadHealth {
        WorkloadHealth {
            loaded: *self.loaded.lock().expect("rerank loaded poisoned"),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rerank_advertises_correct_kind() {
        let w = RerankWorkload::new();
        assert_eq!(w.kind(), WorkloadKind::Rerank);
        assert_eq!(w.model_stem(), MODEL_STEM);
    }

    #[test]
    fn rerank_rejects_non_pair_input() {
        let w = RerankWorkload::new();
        let res = w.run(WorkloadInput::Text {
            text: "single".into(),
        });
        assert!(res.is_err());
    }
}
