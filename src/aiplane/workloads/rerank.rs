//! Cross-encoder reranker: `BAAI/bge-reranker-v2-m3` (XLM-RoBERTa-large
//! pair classifier, ~568M params, multilingual). Static-shape
//! `(1, 512)` ONNX export with `sigmoid(logits[..., 0])` baked into the
//! graph so the output is a single scalar relevance score in `[0, 1]`.
//!
//! Mirrors `EmbedWorkload`:
//!   - Tries the VitisAI EP first (NPU via the daemon's re-exec), falls
//!     back to CPU if the AMD venv / re-exec wasn't set up.
//!   - Holds the loaded session behind `Mutex<Option<...>>` so the
//!     trait stays `&self` and a single shared instance services every
//!     `run()` call.
//!   - XLM-RoBERTa pair tokenisation (`<s> q </s></s> d </s>`) with
//!     pad_id=1; truncation defaults to `only_second` so the query
//!     survives long docs.
//!
//! Model artefacts live at `~/.cache/sy/aiplane/bge-reranker-v2-m3/`,
//! produced once by `scripts/prep_npu_workload.py --workload rerank`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use ort::{
    ep::Vitis,
    inputs,
    session::{builder::GraphOptimizationLevel, Session},
    value::Tensor,
};
use tokenizers::Tokenizer;

use super::super::reexec;
use super::super::registry::{
    cache_root, Workload, WorkloadHealth, WorkloadInput, WorkloadKind, WorkloadOutput,
};
use super::super::session::SessionPool;
use super::detect_npu_label;

const MODEL_STEM: &str = "bge-reranker-v2-m3";
const SEQ_LEN: usize = 512;
/// Static batch dim baked into the prep-time ONNX export + the VAIP
/// partition cache. Held at 1 because VAIP's partition pass
/// internally calls `SerializeToString` on the model graph, and at
/// batch > 1 the xlm-roberta-large graph (with extra activation
/// `value_info` metadata) overflows libprotobuf's 2 GB hard cap.
/// Session-level batching is parked until a smaller backbone or a
/// patched VAIP load path lands.
///
/// The worker's `run_batch` still receives N pairs at once and
/// dispatches them sequentially through one Session — each call is
/// ~40 ms on NPU vs ~2 s on CPU, so the IPC batching wins still hold
/// even without graph-level batching.
const BATCH_SIZE: usize = 1;

struct LoadedReranker {
    session: Session,
    tokenizer: Tokenizer,
    backend: &'static str,
    /// Static batch dim baked into the ONNX export + VAIP cache.
    /// 1 for the legacy single-pair export; ≥ 2 for batched exports
    /// produced by `prep_npu_workload.py --batch-size N`. The
    /// `run_batch` override uses this to decide between per-pair
    /// dispatch and one batched Session::run.
    batch_size: usize,
}

// Same rationale as `EmbedWorkload`: `Session` is `Send + Sync` in ort
// 2.0, but the bound isn't propagated through the wrapper auto-derived
// markers. Access is always under the `Mutex<Option<...>>` so manual
// `Send` is sound.
unsafe impl Send for LoadedReranker {}

pub struct RerankWorkload {
    state: Mutex<Option<LoadedReranker>>,
}

impl RerankWorkload {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    fn cache_dir() -> PathBuf {
        if let Some(v) = std::env::var_os("SY_RERANK_MODEL_DIR") {
            return PathBuf::from(v);
        }
        cache_root().join(MODEL_STEM)
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
        let mut guard = self.state.lock().expect("rerank state poisoned");
        if guard.is_some() {
            return Ok(());
        }
        let dir = Self::cache_dir();
        let model_path = dir.join(format!("{MODEL_STEM}.bf16.onnx"));
        let tokenizer_path = dir.join(format!("{MODEL_STEM}.tokenizer/tokenizer.json"));

        if !model_path.is_file() {
            anyhow::bail!(
                "rerank model not found at {}\nBuild it with:\n  \
                 source /opt/AMD/ryzenai/venv/bin/activate && \
                 python ~/sources/sy/scripts/prep_npu_workload.py --workload rerank",
                model_path.display()
            );
        }
        if !tokenizer_path.is_file() {
            anyhow::bail!(
                "tokenizer.json not found at {}\nRe-run prep_npu_workload.py --workload rerank.",
                tokenizer_path.display()
            );
        }

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer.json: {e}"))?;

        let session = try_vitisai(&model_path, &dir).with_context(|| {
            "rerank worker requires NPU — re-run prep_npu_workload.py and check XRT setup"
        })?;
        let hw = detect_npu_label();
        eprintln!("sy aiplane[rerank]: NPU via VitisAI on {hw} ({MODEL_STEM}, batch={BATCH_SIZE})");
        *guard = Some(LoadedReranker {
            session,
            tokenizer,
            backend: "vitisai",
            batch_size: BATCH_SIZE,
        });
        Ok(())
    }

    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput> {
        // Single-input path: delegate through `run_batch` so a model
        // compiled at batch=N still pads correctly on a one-shot call.
        // Avoids a fast-path divergence between `run` and `run_batch`.
        let outputs = self.run_batch(vec![input])?;
        outputs
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("rerank: run_batch returned empty"))
    }

    fn run_batch(&self, inputs: Vec<WorkloadInput>) -> Result<Vec<WorkloadOutput>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        // Validate every input is a TextPair before doing any work.
        let pairs: Vec<(String, String)> = inputs
            .into_iter()
            .map(|i| match i {
                WorkloadInput::TextPair { a, b } => Ok((a, b)),
                other => Err(anyhow::anyhow!(
                    "rerank: expected TextPair input, got {other:?}"
                )),
            })
            .collect::<Result<Vec<_>>>()?;

        let mut guard = self.state.lock().expect("rerank state poisoned");
        let r = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("rerank: load() not called"))?;
        let scores = run_pairs(r, &pairs)?;
        Ok(scores
            .into_iter()
            .map(|s| WorkloadOutput::Score { score: s })
            .collect())
    }

    fn unload(&self) {
        *self.state.lock().expect("rerank state poisoned") = None;
    }

    fn health(&self) -> WorkloadHealth {
        let guard = self.state.lock().expect("rerank state poisoned");
        match guard.as_ref() {
            Some(r) => WorkloadHealth {
                state: super::super::registry::WorkloadState::Ready {
                    backend: r.backend.to_string(),
                },
                loaded: true,
                backend: r.backend.to_string(),
                ..Default::default()
            },
            None => WorkloadHealth::default(),
        }
    }
}

fn try_vitisai(model: &Path, cache_dir: &Path) -> Result<Session> {
    let amd_venv = reexec::amd_venv_dir();
    if !amd_venv.is_dir() {
        anyhow::bail!("AMD venv missing at {}", amd_venv.display());
    }
    if !reexec::reexec_fired() {
        anyhow::bail!("VitisAI re-exec did not fire; refusing to load EP in-process");
    }
    let vaip_config = amd_venv.join("voe-4.0-linux_x86_64/vaip_config.json");
    if !vaip_config.is_file() {
        anyhow::bail!("vaip_config.json missing at {}", vaip_config.display());
    }

    let cache_key = format!("compiled_{MODEL_STEM}_bf16_seq{SEQ_LEN}_b{BATCH_SIZE}");
    let vitis = Vitis::default()
        .with_config_file(vaip_config.to_string_lossy())
        .with_cache_dir(cache_dir.to_string_lossy())
        .with_cache_key(cache_key);

    Session::builder()
        .map_err(|e| anyhow::anyhow!("session builder: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Disable)
        .map_err(|e| anyhow::anyhow!("optimisation level: {e}"))?
        .with_execution_providers([vitis.build()])
        .map_err(|e| anyhow::anyhow!("register vitisai ep: {e}"))?
        .commit_from_file(model)
        .map_err(|e| anyhow::anyhow!("vitisai session: {e}"))
}

/// XLM-RoBERTa pair encoding via the HF tokenizers `EncodeInput::Dual`
/// API. Pads/truncates to SEQ_LEN (pad_id=1). Truncation strategy
/// defaults to whatever the tokenizer.json prescribes — `only_second`
/// for bge-reranker — which keeps the query intact and chops the doc
/// tail when the pair overflows.
fn encode_pair(tokenizer: &Tokenizer, q: &str, d: &str) -> Result<(Vec<i64>, Vec<i64>)> {
    let enc = tokenizer
        .encode((q, d), true)
        .map_err(|e| anyhow::anyhow!("tokenize pair: {e}"))?;
    let mut ids: Vec<i64> = enc.get_ids().iter().map(|&x| x as i64).collect();
    let mut mask: Vec<i64> = enc.get_attention_mask().iter().map(|&x| x as i64).collect();
    if ids.len() > SEQ_LEN {
        ids.truncate(SEQ_LEN);
        mask.truncate(SEQ_LEN);
    } else if ids.len() < SEQ_LEN {
        let pad_id = tokenizer
            .get_padding()
            .map(|p| p.pad_id as i64)
            .unwrap_or(1);
        ids.resize(SEQ_LEN, pad_id);
        mask.resize(SEQ_LEN, 0);
    }
    Ok((ids, mask))
}

/// Batched inference. Splits `pairs` into chunks of size `batch_size`
/// (matching the prep-time static export shape), pads the trailing
/// chunk with empty rows so every Session::run sees the same shape,
/// and stitches the scores back together in input order. One
/// Session::run per chunk; for the common path (30 candidates,
/// compiled batch=32) this is one inference per query.
fn run_pairs(r: &mut LoadedReranker, pairs: &[(String, String)]) -> Result<Vec<f32>> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let batch = r.batch_size.max(1);
    let pad_id = r
        .tokenizer
        .get_padding()
        .map(|p| p.pad_id as i64)
        .unwrap_or(1);
    let mut scores = Vec::with_capacity(pairs.len());
    for chunk in pairs.chunks(batch) {
        let real_n = chunk.len();
        // Encode each real pair; pad the trailing rows with all-pad
        // tokens + zero mask so the compiled (B, 512) graph runs but
        // those rows contribute nothing.
        let mut ids_flat: Vec<i64> = Vec::with_capacity(batch * SEQ_LEN);
        let mut mask_flat: Vec<i64> = Vec::with_capacity(batch * SEQ_LEN);
        for (a, b) in chunk {
            let (ids, mask) = encode_pair(&r.tokenizer, a, b)?;
            ids_flat.extend(ids);
            mask_flat.extend(mask);
        }
        for _ in real_n..batch {
            ids_flat.extend(std::iter::repeat(pad_id).take(SEQ_LEN));
            mask_flat.extend(std::iter::repeat(0i64).take(SEQ_LEN));
        }
        let shape: [i64; 2] = [batch as i64, SEQ_LEN as i64];
        let ids_t = Tensor::from_array((shape, ids_flat))
            .map_err(|e| anyhow::anyhow!("tensor ids: {e}"))?;
        let mask_t = Tensor::from_array((shape, mask_flat))
            .map_err(|e| anyhow::anyhow!("tensor mask: {e}"))?;
        let outputs = r
            .session
            .run(inputs![
                "input_ids" => ids_t,
                "attention_mask" => mask_t,
            ])
            .map_err(|e| anyhow::anyhow!("session run: {e}"))?;
        let view = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|e| anyhow::anyhow!("extract score: {e}"))?;
        let row_scores: Vec<f32> = view.iter().copied().take(real_n).collect();
        if row_scores.len() != real_n {
            anyhow::bail!(
                "rerank: expected {real_n} scores from batch of {batch}, got {}",
                row_scores.len()
            );
        }
        scores.extend(row_scores);
    }
    Ok(scores)
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

    #[test]
    fn rerank_health_starts_unloaded() {
        let w = RerankWorkload::new();
        let h = w.health();
        assert!(!h.loaded);
        assert_eq!(h.backend, "");
    }
}
