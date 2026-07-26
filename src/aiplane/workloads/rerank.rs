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
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ort::{
    ep::Vitis,
    inputs,
    session::{builder::GraphOptimizationLevel, RunOptions, Session},
    value::Tensor,
};
use tokenizers::Tokenizer;

use super::super::reexec;
use super::super::registry::{
    cache_root, Workload, WorkloadHealth, WorkloadInput, WorkloadKind, WorkloadOutput,
};
use super::super::session::SessionPool;
use super::{detect_npu_label, npu_intra_threads};

const MODEL_STEM: &str = "bge-reranker-v2-m3";
const SEQ_LEN: usize = 512;
/// Static batch dim baked into the prep-time ONNX export + the VAIP
/// partition cache. Pinned at 1 for `bge-reranker-v2-m3`
/// (xlm-roberta-large) — every attempt to lift it hits one of:
///
/// 1. **BF16 + batch>1**: VAIP's `ModelProto::SerializeToString`
///    inlines the Quark QDQ-annotated graph; even after
///    `_strip_value_info` + `_shrink_fp32_initializers_to_bf16` the
///    serialized proto lands at ~2.29 GiB, over libprotobuf's hard
///    2 GiB cap.
///
/// 2. **INT8 + batch=8**: Quark's `INT8_TRANSFORMER_DEFAULT`
///    produces a graph whose QDQ patterns do not match VAIP's
///    `fuse_MatMulNBits` / `vaip-pass_ssmlp` fusion rules — VAIP
///    emits an empty `vaiml_partition_fe.flexml/`, no `.rai`
///    artifact, and the worker falls back to CPU (~11 s per
///    dispatch).
///
/// The path to true batched rerank is therefore a smaller
/// multilingual backbone (e.g. `jinaai/jina-reranker-v2-base-
/// multilingual` at ~278M params) or a custom Quark recipe that
/// emits VAIP-friendly INT8 QDQ patterns. Until then, `run_batch`
/// chunks pairs into singletons.
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
    /// `RunOptions` clone reachable from outside the state lock so
    /// [`Workload::try_cancel`] can call `terminate()` while
    /// `Workload::run_batch` is mid-session. See `EmbedWorkload` for
    /// the same pattern + rationale (SPEC §4.2 cancel is best-effort
    /// and idempotent).
    run_options: Mutex<Option<Arc<RunOptions>>>,
}

impl RerankWorkload {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
            run_options: Mutex::new(None),
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
        tracing::info!(
            target: "sy::aiplane::workloads::rerank",
            hardware = %hw,
            model = MODEL_STEM,
            batch_size = BATCH_SIZE,
            backend = "vitisai",
            "NPU active"
        );
        *guard = Some(LoadedReranker {
            session,
            tokenizer,
            backend: "vitisai",
            batch_size: BATCH_SIZE,
        });
        let opts = RunOptions::new().map_err(|e| anyhow::anyhow!("create run_options: {e}"))?;
        *self
            .run_options
            .lock()
            .expect("rerank run_options poisoned") = Some(Arc::new(opts));
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

        let run_options = self
            .run_options
            .lock()
            .expect("rerank run_options poisoned")
            .clone()
            .ok_or_else(|| anyhow::anyhow!("rerank: load() not called"))?;
        let mut guard = self.state.lock().expect("rerank state poisoned");
        let r = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("rerank: load() not called"))?;
        let scores = run_pairs(r, &run_options, &pairs)?;
        Ok(scores
            .into_iter()
            .map(|s| WorkloadOutput::Score { score: s })
            .collect())
    }

    fn unload(&self) {
        *self.state.lock().expect("rerank state poisoned") = None;
        *self
            .run_options
            .lock()
            .expect("rerank run_options poisoned") = None;
    }

    fn try_cancel(&self) -> bool {
        let Some(opts) = self
            .run_options
            .lock()
            .expect("rerank run_options poisoned")
            .clone()
        else {
            return false;
        };
        opts.terminate().is_ok()
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

    // Cache key follows the prep script's `compiled_<stem>_<suffix>_seq<N>_b<B>`
    // convention. BF16 + batch=1 is the only stable configuration; see
    // the BATCH_SIZE doc comment above for why batch>1 isn't viable yet.
    let cache_key = format!("compiled_{MODEL_STEM}_bf16_seq{SEQ_LEN}_b{BATCH_SIZE}");
    let vitis = Vitis::default()
        .with_config_file(vaip_config.to_string_lossy())
        .with_cache_dir(cache_dir.to_string_lossy())
        .with_cache_key(cache_key);

    Session::builder()
        .map_err(|e| anyhow::anyhow!("session builder: {e}"))?
        // Cap the CPU-EP intra-op pool: VitisAI runs the matmuls on the
        // AIE, ORT's CPU EP only handles fallback glue. Uncapped it grabs
        // every core and pins the box.
        .with_intra_threads(npu_intra_threads())
        .map_err(|e| anyhow::anyhow!("intra-op thread cap: {e}"))?
        // Level1 produces a larger serialized graph (post-fusion
        // attribute bloat). For the xlm-roberta-large backbone that
        // already sits at ~2.27 GB on disk, ORT's pre-pass pushes
        // VAIP's `ModelProto::SerializeToString` over libprotobuf's
        // 2 GB hard cap and the partition pass aborts mid-compile.
        // Keep this on Disable until either (a) a smaller reranker
        // backbone, or (b) the same value_info-strip we'd need for
        // batched export lands.
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
/// Session::run per chunk; with the current `batch_size = 1` cap
/// (libprotobuf 2 GB block) each pair is its own NPU dispatch at
/// ~350 ms.
fn run_pairs(
    r: &mut LoadedReranker,
    run_options: &RunOptions,
    pairs: &[(String, String)],
) -> Result<Vec<f32>> {
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
            ids_flat.extend(std::iter::repeat_n(pad_id, SEQ_LEN));
            mask_flat.extend(std::iter::repeat_n(0i64, SEQ_LEN));
        }
        let shape: [i64; 2] = [batch as i64, SEQ_LEN as i64];
        let ids_t = Tensor::from_array((shape, ids_flat))
            .map_err(|e| anyhow::anyhow!("tensor ids: {e}"))?;
        let mask_t = Tensor::from_array((shape, mask_flat))
            .map_err(|e| anyhow::anyhow!("tensor mask: {e}"))?;
        run_options
            .unterminate()
            .map_err(|e| anyhow::anyhow!("unterminate: {e}"))?;
        let outputs = r
            .session
            .run_with_options(
                inputs![
                    "input_ids" => ids_t,
                    "attention_mask" => mask_t,
                ],
                run_options,
            )
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
    fn try_cancel_before_load_returns_false_without_panicking() {
        // SPEC §4.2 cancel is best-effort. Mirror of the EmbedWorkload
        // test — pre-load cancel must safely no-op so the supervisor's
        // 500 ms SIGKILL guard takes over.
        let w = RerankWorkload::new();
        assert!(!w.try_cancel());
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
