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
use std::sync::Mutex;

use anyhow::Result;
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
use super::{detect_cpu_model, detect_npu_label, VECTOR_DIM};

const MODEL_STEM: &str = "multilingual-e5-base";
const SEQ_LEN: usize = 512;
const QUERY_PREFIX: &str = "query: ";
const PASSAGE_PREFIX: &str = "passage: ";

struct LoadedEmbedder {
    session: Session,
    tokenizer: Tokenizer,
    backend: &'static str,
    hardware: String,
}

// `Session` is `Send + Sync` in ort 2.0, but the bound isn't propagated
// through our wrapper struct's auto-derived markers when behind a Mutex.
// We always touch it under the lock so manual Send is sound.
unsafe impl Send for LoadedEmbedder {}

pub struct EmbedWorkload {
    state: Mutex<Option<LoadedEmbedder>>,
}

impl EmbedWorkload {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
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

        let (session, backend, hardware) = match try_vitisai(&model_path, &dir) {
            Ok(s) => {
                let hw = detect_npu_label();
                eprintln!("sy aiplane[embed]: NPU via VitisAI on {hw} ({MODEL_STEM})");
                (s, "vitisai", hw)
            }
            Err(vitis_err) => {
                eprintln!(
                    "sy aiplane[embed]: VitisAI unavailable ({vitis_err:#}); falling back to CPU"
                );
                let s = try_cpu(&model_path)?;
                let hw = format!("{} (CPU)", detect_cpu_model());
                eprintln!("sy aiplane[embed]: CPU EP active on {hw}");
                (s, "cpu", hw)
            }
        };
        *guard = Some(LoadedEmbedder {
            session,
            tokenizer,
            backend,
            hardware,
        });
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
        let mut guard = self.state.lock().expect("embed state poisoned");
        let emb = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("embed: load() not called"))?;
        let v = run_one(emb, &prefixed)?;
        Ok(WorkloadOutput::Vector { vector: v })
    }

    fn unload(&self) {
        *self.state.lock().expect("embed state poisoned") = None;
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

/// Embed a batch of indexed passages. Each output vector is
/// `VECTOR_DIM`-long and L2-normalised. Adds the E5 `passage: ` prefix.
/// Used by the knowledge indexer; not part of the public Workload
/// trait because the batched API is hot-path-specific.
pub fn embed_passages(workload: &EmbedWorkload, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    workload.load(&SessionPool::new())?; // pool unused in CPU/VitisAI today
    let mut guard = workload.state.lock().expect("embed state poisoned");
    let emb = guard
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("embed: load() not called"))?;
    let mut out = Vec::with_capacity(texts.len());
    for t in texts {
        let prefixed = format!("{PASSAGE_PREFIX}{t}");
        out.push(run_one(emb, &prefixed)?);
    }
    Ok(out)
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

    let cache_key = format!("compiled_{MODEL_STEM}_bf16_seq{SEQ_LEN}");
    let vitis = Vitis::default()
        .with_config_file(vaip_config.to_string_lossy())
        .with_cache_dir(cache_dir.to_string_lossy())
        .with_cache_key(cache_key);

    Session::builder()
        .map_err(|e| anyhow::anyhow!("session builder: {e}"))?
        // The VitisAI EP runs the partition decisions; disable ORT's
        // own graph optimisations so the partitioner sees the model
        // exactly as it was prepped by quark.
        .with_optimization_level(GraphOptimizationLevel::Disable)
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

fn run_one(emb: &mut LoadedEmbedder, prefixed: &str) -> Result<Vec<f32>> {
    let (ids, mask) = encode(&emb.tokenizer, prefixed)?;
    let shape: [i64; 2] = [1, SEQ_LEN as i64];
    let ids_t = Tensor::from_array((shape, ids)).map_err(|e| anyhow::anyhow!("tensor ids: {e}"))?;
    let mask_t =
        Tensor::from_array((shape, mask)).map_err(|e| anyhow::anyhow!("tensor mask: {e}"))?;
    let outputs = emb
        .session
        .run(inputs![
            "input_ids" => ids_t,
            "attention_mask" => mask_t,
        ])
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

/// Public accessor for `current_hardware` — surfaced via
/// `sy aiplane status` and `sy knowledge status`.
pub fn current_hardware(workload: &EmbedWorkload) -> String {
    workload
        .state
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|e| e.hardware.clone()))
        .unwrap_or_default()
}

pub fn current_backend(workload: &EmbedWorkload) -> &'static str {
    workload
        .state
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|e| e.backend))
        .unwrap_or("unloaded")
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
