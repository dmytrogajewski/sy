//! `multilingual-e5-base` sentence embedder. Static-shape (1, 512)
//! ONNX with mean-pool + L2-normalize baked into the graph, BF16-
//! quantised for the AMD Ryzen AI NPU via the VitisAI EP. Falls back
//! to CPU when the daemon's NPU attach fails (re-exec didn't fire,
//! cap missing, /dev/accel busy).
//!
//! Model artefacts live at:
//!
//! ```text
//! ~/.cache/sy/aiplane/multilingual-e5-base/
//!   multilingual-e5-base.bf16.onnx        (model + external .data)
//!   multilingual-e5-base.tokenizer/       (XLM-RoBERTa BPE)
//!   compiled_multilingual-e5-base_bf16_seq512/   (VAIP partition cache)
//! ```
//!
//! Produced once by `scripts/prep_npu_workload.py --workload embed`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
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
use super::{detect_cpu_model, detect_npu_label, VECTOR_DIM};

const MODEL_STEM: &str = "multilingual-e5-base";
const SEQ_LEN: usize = 512;
const QUERY_PREFIX: &str = "query: ";
const PASSAGE_PREFIX: &str = "passage: ";

struct LoadedEmbedder {
    session: Session,
    tokenizer: Tokenizer,
    backend: &'static str,
}

// `Session` is `Send + Sync` in ort 2.0, but the bound isn't propagated
// through our wrapper struct's auto-derived markers when behind a Mutex.
// We always touch it under the lock so manual Send is sound.
unsafe impl Send for LoadedEmbedder {}

pub struct EmbedWorkload {
    state: Mutex<Option<LoadedEmbedder>>,
    /// `RunOptions` clone reachable from outside the state lock so
    /// [`Workload::try_cancel`] can call `terminate()` while
    /// `Workload::run` is still holding `state` mid-session. Per
    /// SPEC §4.2 the cancel must be best-effort and idempotent;
    /// `terminate()` returns immediately and the in-flight
    /// `session.run_with_options` unwinds with an ORT error.
    run_options: Mutex<Option<Arc<RunOptions>>>,
}

impl EmbedWorkload {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
            run_options: Mutex::new(None),
        }
    }

    fn cache_dir() -> PathBuf {
        if let Some(v) = std::env::var_os("SY_EMBED_MODEL_DIR") {
            return PathBuf::from(v);
        }
        cache_root().join(MODEL_STEM)
    }
}

impl Default for EmbedWorkload {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for EmbedWorkload {
    fn kind(&self) -> WorkloadKind {
        WorkloadKind::Embed
    }

    fn model_stem(&self) -> &'static str {
        MODEL_STEM
    }

    fn load(&self, _pool: &SessionPool) -> Result<()> {
        let mut guard = self.state.lock().expect("embed state poisoned");
        if guard.is_some() {
            return Ok(());
        }
        let dir = Self::cache_dir();
        let model_path = dir.join(format!("{MODEL_STEM}.bf16.onnx"));
        let tokenizer_path = dir.join(format!("{MODEL_STEM}.tokenizer/tokenizer.json"));

        if !model_path.is_file() {
            anyhow::bail!(
                "embed model not found at {}\nBuild it with:\n  \
                 source /opt/AMD/ryzenai/venv/bin/activate && \
                 python ~/sources/sy/scripts/prep_npu_workload.py --workload embed",
                model_path.display()
            );
        }
        if !tokenizer_path.is_file() {
            anyhow::bail!(
                "tokenizer.json not found at {}\nRe-run prep_npu_workload.py --workload embed.",
                tokenizer_path.display()
            );
        }

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer.json: {e}"))?;

        let (session, backend) = match try_vitisai(&model_path, &dir) {
            Ok(s) => {
                let hw = detect_npu_label();
                tracing::info!(
                    target: "sy::aiplane::workloads::embed",
                    hardware = %hw,
                    model = MODEL_STEM,
                    backend = "vitisai",
                    "NPU active"
                );
                (s, "vitisai")
            }
            Err(vitis_err) => {
                tracing::warn!(
                    target: "sy::aiplane::workloads::embed",
                    error = %format!("{vitis_err:#}"),
                    "VitisAI unavailable; falling back to CPU"
                );
                let s = try_cpu(&model_path)?;
                let hw = format!("{} (CPU)", detect_cpu_model());
                tracing::info!(
                    target: "sy::aiplane::workloads::embed",
                    hardware = %hw,
                    backend = "cpu",
                    "CPU EP active"
                );
                (s, "cpu")
            }
        };
        *guard = Some(LoadedEmbedder {
            session,
            tokenizer,
            backend,
        });
        // Build the per-workload `RunOptions` once the ORT runtime is
        // live (Session::builder above initialises it). Shared across
        // every `run()` call so `try_cancel` from another thread can
        // call `terminate()` on the same handle.
        let opts = RunOptions::new().map_err(|e| anyhow::anyhow!("create run_options: {e}"))?;
        *self.run_options.lock().expect("embed run_options poisoned") = Some(Arc::new(opts));
        Ok(())
    }

    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput> {
        let text = match input {
            WorkloadInput::Text { text } => text,
            WorkloadInput::TextPair { a, .. } => {
                // E5 doesn't have a pair mode — embed `a` (the query
                // side) and ignore `b`. Pair-mode belongs to the
                // Rerank workload.
                a
            }
            other => anyhow::bail!("embed: expected Text input, got {other:?}"),
        };
        // Auto-prefix: callers send raw text; we add the E5 task
        // prefix here. A heuristic that mimics what `embed_one` used
        // to do — incoming text is a query unless explicitly tagged.
        let prefixed = if text.starts_with(PASSAGE_PREFIX) || text.starts_with(QUERY_PREFIX) {
            text
        } else {
            format!("{QUERY_PREFIX}{text}")
        };
        // Snapshot `run_options` first so we don't hold the state
        // lock across the brief run_options lock; this ordering keeps
        // `try_cancel` (which only touches run_options) deadlock-free
        // against a concurrent `run()` that is mid-session.
        let run_options = self
            .run_options
            .lock()
            .expect("embed run_options poisoned")
            .clone()
            .ok_or_else(|| anyhow::anyhow!("embed: load() not called"))?;
        let mut guard = self.state.lock().expect("embed state poisoned");
        let emb = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("embed: load() not called"))?;
        let v = run_one(emb, &run_options, &prefixed)?;
        Ok(WorkloadOutput::Vector { vector: v })
    }

    fn unload(&self) {
        *self.state.lock().expect("embed state poisoned") = None;
        *self.run_options.lock().expect("embed run_options poisoned") = None;
    }

    fn try_cancel(&self) -> bool {
        let Some(opts) = self
            .run_options
            .lock()
            .expect("embed run_options poisoned")
            .clone()
        else {
            return false;
        };
        opts.terminate().is_ok()
    }

    fn health(&self) -> WorkloadHealth {
        let guard = self.state.lock().expect("embed state poisoned");
        match guard.as_ref() {
            Some(e) => WorkloadHealth {
                state: super::super::registry::WorkloadState::Ready {
                    backend: e.backend.to_string(),
                },
                loaded: true,
                backend: e.backend.to_string(),
                ..Default::default()
            },
            None => WorkloadHealth::default(),
        }
    }
}

/// Probe AMD Ryzen AI's venv, register the VitisAI EP with the cached
/// NPU partition artifact.
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

    // Versioned with `_o1` because the cached .rai is tied to the
    // exact node names VAIP saw at compile time. Switching ORT's
    // pre-pass optimization level changes node names (DQ/Cast
    // elimination, fusion). Loading the old .rai with a new graph
    // crashes inside libvaip-core (`check failure: node != nullptr,
    // cannot find producer`). Bump the suffix any time the
    // optimization level or pre-pass shape changes.
    let cache_key = format!("compiled_{MODEL_STEM}_bf16_seq{SEQ_LEN}_o1");
    let vitis = Vitis::default()
        .with_config_file(vaip_config.to_string_lossy())
        .with_cache_dir(cache_dir.to_string_lossy())
        .with_cache_key(cache_key);

    Session::builder()
        .map_err(|e| anyhow::anyhow!("session builder: {e}"))?
        // BASIC runs ORT's safe, partition-friendly transforms
        // (constant folding, redundant Cast elimination, trivial
        // fusions) before VAIP sees the graph. Measured on Strix:
        // BASIC = 63 ms / inference vs DISABLE = 208 ms (3.3×). The
        // earlier "let quark's output through verbatim" comment was
        // wrong — VAIP partitions a cleaner graph more aggressively.
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(|e| anyhow::anyhow!("optimisation level: {e}"))?
        .with_execution_providers([vitis.build()])
        .map_err(|e| anyhow::anyhow!("register vitisai ep: {e}"))?
        .commit_from_file(model)
        .map_err(|e| anyhow::anyhow!("vitisai session: {e}"))
}

fn try_cpu(model: &Path) -> Result<Session> {
    Session::builder()
        .map_err(|e| anyhow::anyhow!("session builder: {e}"))?
        .commit_from_file(model)
        .map_err(|e| anyhow::anyhow!("cpu session: {e}"))
}

fn encode(tokenizer: &Tokenizer, text: &str) -> Result<(Vec<i64>, Vec<i64>)> {
    let enc = tokenizer
        .encode(text, true)
        .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
    let mut ids: Vec<i64> = enc.get_ids().iter().map(|&x| x as i64).collect();
    let mut mask: Vec<i64> = enc.get_attention_mask().iter().map(|&x| x as i64).collect();
    if ids.len() > SEQ_LEN {
        ids.truncate(SEQ_LEN);
        mask.truncate(SEQ_LEN);
    } else if ids.len() < SEQ_LEN {
        // XLM-RoBERTa pads with id=1.
        let pad_id = tokenizer
            .get_padding()
            .map(|p| p.pad_id as i64)
            .unwrap_or(1);
        ids.resize(SEQ_LEN, pad_id);
        mask.resize(SEQ_LEN, 0);
    }
    Ok((ids, mask))
}

fn run_one(emb: &mut LoadedEmbedder, run_options: &RunOptions, prefixed: &str) -> Result<Vec<f32>> {
    let (ids, mask) = encode(&emb.tokenizer, prefixed)?;
    let shape: [i64; 2] = [1, SEQ_LEN as i64];
    let ids_t = Tensor::from_array((shape, ids)).map_err(|e| anyhow::anyhow!("tensor ids: {e}"))?;
    let mask_t =
        Tensor::from_array((shape, mask)).map_err(|e| anyhow::anyhow!("tensor mask: {e}"))?;
    // Reset a leftover `terminate()` flag from a previous cancelled
    // call so this fresh request isn't pre-poisoned (SPEC §4.2:
    // cancel is per-request, the RunOptions handle is per-workload).
    run_options
        .unterminate()
        .map_err(|e| anyhow::anyhow!("unterminate: {e}"))?;
    let outputs = emb
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
        .map_err(|e| anyhow::anyhow!("extract output: {e}"))?;
    let v: Vec<f32> = view.iter().copied().collect();
    if v.len() != VECTOR_DIM {
        anyhow::bail!("model output dim {} != VECTOR_DIM {VECTOR_DIM}", v.len());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_workload_advertises_correct_kind() {
        let w = EmbedWorkload::new();
        assert_eq!(w.kind(), WorkloadKind::Embed);
        assert_eq!(w.model_stem(), MODEL_STEM);
    }

    #[test]
    fn try_cancel_before_load_returns_false_without_panicking() {
        // SPEC §4.2 cancel is best-effort. Before `load()` builds the
        // RunOptions, there's nothing to terminate; the trait
        // contract is "return `false` so the supervisor escalates to
        // SIGKILL". The pre-load path must NOT panic — a worker that
        // crashes on a cancel of an un-ready workload would lose
        // every other queued request to the same kind.
        let w = EmbedWorkload::new();
        assert!(!w.try_cancel());
    }

    #[test]
    fn embed_run_without_load_errors_clearly() {
        let w = EmbedWorkload::new();
        let res = w.run(WorkloadInput::Text { text: "hi".into() });
        // Without the model on disk, load() bails before run() is
        // reachable; we just confirm `run` itself doesn't panic when
        // state is uninitialised.
        match res {
            Err(_) => {}
            Ok(_) => panic!("run without load must error"),
        }
    }

    #[test]
    fn embed_health_starts_unloaded() {
        let w = EmbedWorkload::new();
        let h = w.health();
        assert!(!h.loaded);
        assert_eq!(h.backend, "");
    }
}
