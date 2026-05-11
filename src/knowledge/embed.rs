//! Embedding pipeline: AMD Ryzen AI NPU via ORT's VitisAI EP, with
//! CUDA and CPU fallbacks. Model is `intfloat/multilingual-e5-base`
//!
//! ## Re-exec dance
//!
//! VitisAI EP's plugin (`libonnxruntime_vitisai_ep.so`) dlopens a dozen
//! sibling libs by SONAME (`libxcompiler-core-without-symbol.so`,
//! `libvart-*.so.3`, `libglog.so.1`, …) — all of which live in the
//! Ryzen AI venv's `voe/lib`, `flexml*/lib`, etc. Setting
//! `LD_LIBRARY_PATH` via `set_var` after process start is too late:
//! glibc has already snapshotted its lib search path. So if we detect
//! `/opt/AMD/ryzenai/venv` and we haven't already, we re-exec
//! ourselves with the env baked in. The `SY_AMD_REEXECED` env-var
//! sentinel prevents infinite loops.
//! (768-dim, BF16-quantised) pre-compiled to a `.rai` NPU artifact by
//! `scripts/prep_npu_embed.py`. Compile cache lives under
//! `~/.cache/sy/npu-embed/` (override with `SY_EMBED_MODEL_DIR`).
//!
//! E5 task prefixes (`query: ` for searches, `passage: ` for indexed
//! documents) are added by `embed_one`/`embed_batch`. Mean-pool +
//! L2-normalise happen inside the ONNX graph, so the runtime gets a
//! single normalised 1×VECTOR_DIM tensor per call — no post-processing
//! on the Rust side.
//!
//! Migration note: before this commit sy used `multilingual-e5-large`
//! (1024-dim) via fastembed. The new dim (768) is incompatible with the
//! old qdrant collection, so on first start after upgrading run:
//!
//!   sy knowledge drop && sy knowledge resync
//!
//! The compiled NPU artifact is produced once by:
//!
//!   source /opt/AMD/ryzenai/venv/bin/activate
//!   python ~/sources/sy/scripts/prep_npu_embed.py

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use ort::{
    ep::{CUDA, Vitis},
    inputs,
    session::{builder::GraphOptimizationLevel, Session},
    value::Tensor,
};
use tokenizers::Tokenizer;

use super::{exit, VECTOR_DIM};

const MODEL_STEM: &str = "multilingual-e5-base";
const SEQ_LEN: usize = 512;
const QUERY_PREFIX: &str = "query: ";
const PASSAGE_PREFIX: &str = "passage: ";

struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
}

// `Session` is `Send + Sync` in ort 2.0, but the bound isn't propagated
// through our wrapper struct's auto-derived markers when behind a Mutex.
// We always touch it under the lock so manual Send is sound.
unsafe impl Send for Embedder {}

static EMBEDDER: OnceLock<Mutex<Embedder>> = OnceLock::new();
static BACKEND: OnceLock<&'static str> = OnceLock::new();
static HARDWARE: OnceLock<String> = OnceLock::new();

/// `"vitisai"`, `"cuda"`, `"cpu"`, or `"unloaded"` if the model hasn't
/// been touched yet. Surfaced to the status snapshot so the waybar
/// tooltip + `sy knowledge status` can show which backend is engaged.
pub fn current_backend() -> &'static str {
    BACKEND.get().copied().unwrap_or("unloaded")
}

/// Human-readable label for the actual hardware doing inference, e.g.
/// `"AMD NPU on 9 HX 370"`, `"NVIDIA GeForce RTX 5090 Laptop GPU"`,
/// `"AMD Ryzen AI 9 HX 370 (CPU)"`. Empty if `current_backend() ==
/// "unloaded"`.
pub fn current_hardware() -> String {
    HARDWARE.get().cloned().unwrap_or_default()
}

fn detect_cpu_model() -> String {
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

fn detect_nvidia_label() -> String {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = s.lines().next() {
                return line.trim().to_string();
            }
        }
    }
    "NVIDIA GPU".to_string()
}

fn detect_npu_label() -> String {
    // The lspci PCI vendor string is `Strix/Krackan/Strix Halo
    // Neural Processing Unit` — useless across SKUs. The CPU model
    // name pins it down.
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

/// If `/opt/AMD/ryzenai/venv` is present and we haven't already done so,
/// re-exec the current binary with `LD_LIBRARY_PATH`, `ORT_DYLIB_PATH`,
/// and the Ryzen AI activate env vars baked in. Called from `main()`
/// before any threads spawn so the re-exec is safe.
pub fn maybe_reexec_with_amd_env() {
    if std::env::var_os("SY_AMD_REEXECED").is_some() {
        return;
    }
    let amd_venv = Path::new("/opt/AMD/ryzenai/venv/lib/python3.12/site-packages");
    if !amd_venv.is_dir() {
        return;
    }
    let Ok(ort_so) = pick_ort_so(&amd_venv.join("onnxruntime/capi")) else {
        return;
    };

    // Put the canonical ORT dir FIRST so any dlopen-by-soname inside
    // VitisAI EP for `libonnxruntime.so.1` resolves to the same binary
    // that `ORT_DYLIB_PATH` points at — voe/lib also ships a copy of
    // ORT and loading both double-inits the C++ runtime singletons.
    let lib_dirs = [
        amd_venv.join("onnxruntime/capi"),
        amd_venv.join("flexml/flexml_extras/lib"),
        amd_venv.join("voe/lib"),
        amd_venv.join("vaimlpl_be/lib"),
        amd_venv.join("flexmlrt/lib"),
    ];
    let prev = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let mut ld_path: Vec<String> = lib_dirs
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    if !prev.is_empty() {
        ld_path.push(prev);
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let args: Vec<_> = std::env::args_os().skip(1).collect();

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&exe)
        .args(&args)
        .env("LD_LIBRARY_PATH", ld_path.join(":"))
        .env("ORT_DYLIB_PATH", &ort_so)
        .env("RYZEN_AI_INSTALLATION_PATH", "/opt/AMD/ryzenai/venv")
        .env("XILINX_VITIS", amd_venv)
        .env("XILINX_VITIS_AIETOOLS", amd_venv)
        .env("XILINX_XRT", "/opt/xilinx/xrt")
        .env("SY_AMD_REEXECED", "1")
        .exec();
    // exec() only returns on failure. Fall through to the in-process
    // path so the user at least gets a chance via CPU/CUDA fallback.
    eprintln!("sy knowledge: re-exec for VitisAI env failed: {err}; staying in-process");
}

fn model_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("SY_EMBED_MODEL_DIR") {
        return PathBuf::from(v);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/sy/npu-embed")
}

fn embedder() -> Result<&'static Mutex<Embedder>> {
    if let Some(m) = EMBEDDER.get() {
        return Ok(m);
    }
    let e = init_embedder().map_err(|e| super::KnowledgeError {
        code: exit::EMBEDDING_FAILED,
        msg: format!("embed init: {e}"),
    })?;
    let _ = EMBEDDER.set(Mutex::new(e));
    Ok(EMBEDDER.get().expect("just set"))
}

fn init_embedder() -> Result<Embedder> {
    let dir = model_dir();
    let model_path = dir.join(format!("{MODEL_STEM}.bf16.onnx"));
    let tokenizer_path = dir.join(format!("{MODEL_STEM}.tokenizer/tokenizer.json"));

    if !model_path.is_file() {
        anyhow::bail!(
            "embedding model not found at {}\n\
             Build it with:\n  \
             source /opt/AMD/ryzenai/venv/bin/activate && \
             python ~/sources/sy/scripts/prep_npu_embed.py",
            model_path.display()
        );
    }
    if !tokenizer_path.is_file() {
        anyhow::bail!(
            "tokenizer.json not found at {}\nRe-run prep_npu_embed.py to regenerate.",
            tokenizer_path.display()
        );
    }

    let tokenizer = Tokenizer::from_file(&tokenizer_path)
        .map_err(|e| anyhow::anyhow!("load tokenizer.json: {e}"))?;

    let session = match try_vitisai(&model_path, &dir) {
        Ok(s) => {
            let _ = BACKEND.set("vitisai");
            let hw = detect_npu_label();
            eprintln!("sy knowledge: embeddings on {hw} via VitisAI ({MODEL_STEM})");
            let _ = HARDWARE.set(hw);
            s
        }
        Err(vitis_err) => match try_cuda(&model_path) {
            Ok(s) => {
                let _ = BACKEND.set("cuda");
                let hw = detect_nvidia_label();
                eprintln!("sy knowledge: embeddings on {hw} via CUDA ({MODEL_STEM})");
                let _ = HARDWARE.set(hw);
                s
            }
            Err(cuda_err) => {
                eprintln!(
                    "sy knowledge: VitisAI unavailable ({vitis_err:#}); \
                     CUDA unavailable ({cuda_err:#}); falling back to CPU"
                );
                let s = try_cpu(&model_path)?;
                let _ = BACKEND.set("cpu");
                let hw = format!("{} (CPU)", detect_cpu_model());
                eprintln!("sy knowledge: embeddings on {hw} ({MODEL_STEM})");
                let _ = HARDWARE.set(hw);
                s
            }
        },
    };

    Ok(Embedder { session, tokenizer })
}

/// Probe AMD Ryzen AI's venv, set ORT_DYLIB_PATH at its onnxruntime
/// (which has VitisAI EP built in), preload the voe/flexml deps, and
/// register the EP with the cached NPU partition artifact.
fn try_vitisai(model: &Path, cache_dir: &Path) -> Result<Session> {
    let amd_venv = Path::new("/opt/AMD/ryzenai/venv/lib/python3.12/site-packages");
    if !amd_venv.is_dir() {
        anyhow::bail!("AMD venv missing at {}", amd_venv.display());
    }

    // The actual env setup happens in `maybe_reexec_with_amd_env`
    // before main() does anything else. If we get here without the
    // SY_AMD_REEXECED sentinel, the re-exec didn't fire and VitisAI EP
    // will almost certainly fail to load its sibling .so deps. Bail
    // early so the caller falls back to CUDA/CPU.
    if std::env::var_os("SY_AMD_REEXECED").is_none() {
        anyhow::bail!(
            "VitisAI re-exec did not fire; refusing to load EP in-process"
        );
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
        // The VitisAI EP runs the partition decisions; disable ORT's own
        // graph optimisations so the partitioner sees the model exactly
        // as it was prepped by quark.
        .with_optimization_level(GraphOptimizationLevel::Disable)
        .map_err(|e| anyhow::anyhow!("optimisation level: {e}"))?
        .with_execution_providers([vitis.build()])
        .map_err(|e| anyhow::anyhow!("register vitisai ep: {e}"))?
        .commit_from_file(model)
        .map_err(|e| anyhow::anyhow!("vitisai session: {e}"))
}

fn pick_ort_so(dir: &Path) -> Result<PathBuf> {
    let pick = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("libonnxruntime.so.") {
                Some(e.path())
            } else {
                None
            }
        })
        .max()
        .with_context(|| format!("no libonnxruntime.so.* in {}", dir.display()))?;
    Ok(pick)
}

fn try_cuda(model: &Path) -> Result<Session> {
    // CUDA EP uses whatever ORT_DYLIB_PATH / LD_LIBRARY_PATH was set
    // up before us (or by the VitisAI probe attempt). If `cuda` feature
    // is built in and a CUDA-capable libonnxruntime is reachable, this
    // succeeds; otherwise commit_from_file returns the registration
    // error.
    Session::builder()
        .map_err(|e| anyhow::anyhow!("session builder: {e}"))?
        .with_execution_providers([CUDA::default().build()])
        .map_err(|e| anyhow::anyhow!("register cuda ep: {e}"))?
        .commit_from_file(model)
        .map_err(|e| anyhow::anyhow!("cuda session: {e}"))
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

fn run_one(emb: &mut Embedder, prefixed: &str) -> Result<Vec<f32>> {
    let (ids, mask) = encode(&emb.tokenizer, prefixed)?;
    let shape: [i64; 2] = [1, SEQ_LEN as i64];
    let ids_t = Tensor::from_array((shape, ids))
        .map_err(|e| anyhow::anyhow!("tensor ids: {e}"))?;
    let mask_t = Tensor::from_array((shape, mask))
        .map_err(|e| anyhow::anyhow!("tensor mask: {e}"))?;
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

/// Embed a batch of indexed passages. Each output vector is
/// `VECTOR_DIM`-long and L2-normalised. Adds the E5 `passage: ` prefix.
pub fn embed_batch(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let m = embedder()?;
    let mut guard = m.lock().expect("embed model mutex poisoned");
    let mut out = Vec::with_capacity(texts.len());
    for t in texts {
        let prefixed = format!("{PASSAGE_PREFIX}{t}");
        out.push(run_one(&mut guard, &prefixed).map_err(|e| super::KnowledgeError {
            code: exit::EMBEDDING_FAILED,
            msg: format!("embed: {e}"),
        })?);
    }
    Ok(out)
}

/// Embed a single search query (used by `sy knowledge search` and the
/// MCP server). Adds the E5 `query: ` prefix.
pub fn embed_one(text: &str) -> Result<Vec<f32>> {
    let m = embedder()?;
    let mut guard = m.lock().expect("embed model mutex poisoned");
    let prefixed = format!("{QUERY_PREFIX}{text}");
    run_one(&mut guard, &prefixed).map_err(|e| super::KnowledgeError {
        code: exit::EMBEDDING_FAILED,
        msg: format!("embed: {e}"),
    }
    .into())
}
