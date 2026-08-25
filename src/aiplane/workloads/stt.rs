//! Speech-to-text: `whisper-medium`, the AMD NPU ONNX split
//! (encoder + decoder), both driven over the VitisAI EP on the NPU.
//! Strict NPU: a VitisAI failure is a hard error, never a silent CPU
//! fallback. Input is 16 kHz mono PCM; output is decoded text.
//!
//! The encoder (`encoder_model.onnx`, input `x` f32 `[1,80,3000]` →
//! `layer_norm_48` f32 `[1,1500,1024]`) and decoder
//! (`decoder_model.onnx` + `.data`, inputs `x` int64 `[1,128]` ids +
//! `xa` f32 `[1,1500,1024]` → `matmul` f32 `[1,128,51865]` logits) run
//! as two `Session`s sharing one VAIP cache dir. Greedy decode follows
//! `RyzenAI-SW/Demos/ASR/Whisper/run_whisper.py`'s `decode`: fixed
//! length-128 `input_ids` filled with EOS, `tokens[0] = SOT`, argmax
//! over `logits[0, len-1]`, stop on EOS or length.
//!
//! Model artefacts (produced by
//! `prep_npu_workload.py --workload stt`) live at:
//!
//! ```text
//! ~/.cache/sy/aiplane/whisper-medium/
//!   amd-src/{encoder_model.onnx, decoder_model.onnx, decoder_model.onnx.data}
//!   tokenizer/{tokenizer.json, ..., preprocessor_config.json}
//!   vitisai_config_whisper_encoder.json
//!   vitisai_config_whisper_decoder.json
//! ```

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
use super::npu_intra_threads;
use super::whisper_mel::{MelExtractor, N_FRAMES, N_MELS, SAMPLE_RATE};

const MODEL_STEM: &str = "whisper-medium";
/// Encoder hidden dim (medium). The decoder's `xa` cross-attention
/// input is `[1, ENC_FRAMES, ENC_DIM]`.
const ENC_DIM: usize = 1024;
/// Encoder output time steps (`layer_norm_48` dim 1).
const ENC_FRAMES: usize = 1500;
/// Fixed decoder `input_ids` length. The decoder ONNX is exported at a
/// static `[1, 128]`; unused slots are padded with EOS.
const MAX_DECODE: usize = 128;
/// Logits vocabulary size (`matmul` dim 2).
const VOCAB: usize = 51865;
const SOT_TOKEN: &str = "<|startoftranscript|>";
const EOT_TOKEN: &str = "<|endoftext|>";
/// Force the *transcribe* task (verbatim, in the audio's own language) and
/// suppress timestamps. Without these, Whisper is free to pick the
/// *translate* task and emit English for non-English audio — wrong for a
/// Russian corpus, which must be transcribed verbatim so Russian queries
/// match. The language token itself is auto-detected (predicted after SOT).
const TRANSCRIBE_TOKEN: &str = "<|transcribe|>";
const NOTIMESTAMPS_TOKEN: &str = "<|notimestamps|>";

struct LoadedStt {
    encoder: Session,
    decoder: Session,
    /// `true` when the decoder's first input is `int64` (`x` = ids).
    /// `false` means the export bound (xa, ids) instead, so we swap.
    decoder_ids_first: bool,
    tokenizer: Tokenizer,
    mel: MelExtractor,
    sot: u32,
    eot: u32,
    transcribe_id: u32,
    notimestamps_id: u32,
    backend: &'static str,
}

// `Session` is `Send` in ort 2.0 but the bound isn't propagated through
// the wrapper struct behind a Mutex; we only touch it under the lock.
unsafe impl Send for LoadedStt {}

pub struct SttWorkload {
    state: Mutex<Option<LoadedStt>>,
}

impl SttWorkload {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    fn cache_dir() -> PathBuf {
        if let Some(v) = std::env::var_os("SY_STT_MODEL_DIR") {
            return PathBuf::from(v);
        }
        cache_root().join(MODEL_STEM)
    }
}

impl Default for SttWorkload {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for SttWorkload {
    fn kind(&self) -> WorkloadKind {
        WorkloadKind::Stt
    }

    fn model_stem(&self) -> &'static str {
        MODEL_STEM
    }

    fn load(&self, _pool: &SessionPool) -> Result<()> {
        let mut guard = self.state.lock().expect("stt state poisoned");
        if guard.is_some() {
            return Ok(());
        }
        let dir = Self::cache_dir();
        let encoder_path = dir.join("amd-src/encoder_model.onnx");
        let decoder_path = dir.join("amd-src/decoder_model.onnx");
        let tokenizer_path = dir.join("tokenizer/tokenizer.json");
        let preproc_path = dir.join("tokenizer/preprocessor_config.json");
        let enc_cfg = dir.join("vitisai_config_whisper_encoder.json");
        let dec_cfg = dir.join("vitisai_config_whisper_decoder.json");

        for (label, p) in [
            ("encoder_model.onnx", &encoder_path),
            ("decoder_model.onnx", &decoder_path),
            ("tokenizer/tokenizer.json", &tokenizer_path),
            ("tokenizer/preprocessor_config.json", &preproc_path),
            ("vitisai_config_whisper_encoder.json", &enc_cfg),
            ("vitisai_config_whisper_decoder.json", &dec_cfg),
        ] {
            if !p.is_file() {
                anyhow::bail!(
                    "STT artefact {label} missing at {}\nBuild it with:\n  \
                     source /opt/AMD/ryzenai/venv/bin/activate && \
                     python ~/sources/sy/scripts/prep_npu_workload.py --workload stt",
                    p.display()
                );
            }
        }

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer.json: {e}"))?;
        let sot = tokenizer
            .token_to_id(SOT_TOKEN)
            .ok_or_else(|| anyhow::anyhow!("tokenizer missing {SOT_TOKEN}"))?;
        let eot = tokenizer
            .token_to_id(EOT_TOKEN)
            .ok_or_else(|| anyhow::anyhow!("tokenizer missing {EOT_TOKEN}"))?;
        let transcribe_id = tokenizer
            .token_to_id(TRANSCRIBE_TOKEN)
            .ok_or_else(|| anyhow::anyhow!("tokenizer missing {TRANSCRIBE_TOKEN}"))?;
        let notimestamps_id = tokenizer
            .token_to_id(NOTIMESTAMPS_TOKEN)
            .ok_or_else(|| anyhow::anyhow!("tokenizer missing {NOTIMESTAMPS_TOKEN}"))?;
        let mel = MelExtractor::from_preprocessor_config(&preproc_path)?;

        // Both encoder and decoder run on the NPU (VitisAI), each as its own
        // session with a distinct VitisAI config + cache_key — exactly how
        // AMD's `RyzenAI-SW/Demos/ASR/Whisper/run_whisper.py` drives two
        // VitisAI `InferenceSession`s in one process. Strict NPU: a VitisAI
        // failure is a hard error, never a silent CPU fallback.
        let encoder = vitisai_session(&encoder_path, &dir, &enc_cfg, "whisper_medium_encoder")?;
        let decoder = vitisai_session(&decoder_path, &dir, &dec_cfg, "whisper_medium_decoder")?;
        // Determine ids/encoder binding order from the decoder's first input
        // type (run_whisper.py's swap guard).
        let decoder_ids_first = decoder_first_input_is_int64(&decoder);

        let backend = "vitisai";
        *guard = Some(LoadedStt {
            encoder,
            decoder,
            decoder_ids_first,
            tokenizer,
            mel,
            sot,
            eot,
            transcribe_id,
            notimestamps_id,
            backend,
        });
        Ok(())
    }

    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput> {
        let (pcm, sr) = match input {
            WorkloadInput::Audio { pcm, sr } => (pcm, sr),
            other => anyhow::bail!("stt: expected Audio input, got {other:?}"),
        };
        if sr != SAMPLE_RATE {
            anyhow::bail!("stt: expected {SAMPLE_RATE} Hz input, got {sr}");
        }
        let mut guard = self.state.lock().expect("stt state poisoned");
        let stt = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stt: load() not called"))?;
        let text = transcribe(stt, &pcm)?;
        Ok(WorkloadOutput::Text { text })
    }

    fn unload(&self) {
        *self.state.lock().expect("stt state poisoned") = None;
    }

    fn health(&self) -> WorkloadHealth {
        let guard = self.state.lock().expect("stt state poisoned");
        match guard.as_ref() {
            Some(s) => WorkloadHealth {
                state: super::super::registry::WorkloadState::Ready {
                    backend: s.backend.to_string(),
                },
                loaded: true,
                backend: s.backend.to_string(),
                ..Default::default()
            },
            None => WorkloadHealth::default(),
        }
    }
}

/// Build one strict-NPU VitisAI session for `model`. `config_file` points
/// at the per-model whisper VitisAI JSON vendored in the cache dir (not the
/// venv's `vaip_config.json`); `cache_key` keeps the encoder's and decoder's
/// compiled `.rai` partitions distinct in the shared `cache_dir`. There is
/// no CPU fallback — if the VitisAI EP is unavailable this returns `Err` so
/// the daemon never silently serves CPU inference.
fn vitisai_session(
    model: &Path,
    cache_dir: &Path,
    config_file: &Path,
    cache_key: &str,
) -> Result<Session> {
    let amd_venv = reexec::amd_venv_dir();
    if !amd_venv.is_dir() {
        anyhow::bail!("AMD venv missing at {}", amd_venv.display());
    }
    if !reexec::reexec_fired() {
        anyhow::bail!("VitisAI re-exec did not fire; refusing to load EP in-process");
    }
    if !config_file.is_file() {
        anyhow::bail!(
            "whisper VitisAI config missing at {}",
            config_file.display()
        );
    }
    let vitis = Vitis::default()
        .with_config_file(config_file.to_string_lossy())
        .with_cache_dir(cache_dir.to_string_lossy())
        .with_cache_key(cache_key);
    Session::builder()
        .map_err(|e| anyhow::anyhow!("session builder: {e}"))?
        // Cap the CPU-EP intra-op pool: the encoder/decoder matmuls run on
        // the AIE, ORT's CPU EP only handles fallback glue. Uncapped it
        // grabs every core and pins the box.
        .with_intra_threads(npu_intra_threads())
        .map_err(|e| anyhow::anyhow!("intra-op thread cap: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(|e| anyhow::anyhow!("optimisation level: {e}"))?
        .with_execution_providers([vitis.build()])
        .map_err(|e| anyhow::anyhow!("register vitisai ep: {e}"))?
        .commit_from_file(model)
        .map_err(|e| anyhow::anyhow!("vitisai session: {e}"))
}

/// run_whisper.py's swap guard: the decoder takes (`input_ids`,
/// `encoder_out`) but the export's input order isn't guaranteed. If the
/// first input isn't int64 it's the encoder tensor, so we feed in the
/// other order. Defaults to ids-first when the type can't be read.
fn decoder_first_input_is_int64(decoder: &Session) -> bool {
    use ort::value::{TensorElementType, ValueType};
    match decoder.inputs().first().map(|i| i.dtype()) {
        Some(ValueType::Tensor { ty, .. }) => *ty == TensorElementType::Int64,
        // Unknown / non-tensor: assume the documented ids-first order.
        _ => true,
    }
}

/// Full mel → encode → greedy-decode → detokenize for one ≤30 s clip.
fn transcribe(stt: &mut LoadedStt, pcm: &[i16]) -> Result<String> {
    let mel = stt.mel.log_mel(pcm);
    let encoder_out = run_encoder(&mut stt.encoder, &mel)?;
    let ids_first = stt.decoder_ids_first;
    let sot = stt.sot;
    let eot = stt.eot;
    let transcribe_id = stt.transcribe_id;
    let notimestamps_id = stt.notimestamps_id;
    let decoder = &mut stt.decoder;
    let mut step =
        |ids: &[i64], pos: usize| run_decoder(decoder, ids, pos, &encoder_out, ids_first);
    // Auto-detect the language: the first token Whisper emits after SOT is
    // the language token. Then force the transcribe task + no-timestamps so
    // the model transcribes verbatim in that language (never translates).
    let mut probe = vec![eot as i64; MAX_DECODE];
    probe[0] = sot as i64;
    let lang = argmax(&step(&probe, 1)?) as u32;
    let prefix = [sot, lang, transcribe_id, notimestamps_id];
    let tokens = greedy_decode(&prefix, eot, step)?;
    // `skip_special_tokens` strips SOT / language / transcribe / no-timestamps.
    let text = stt
        .tokenizer
        .decode(&tokens, true)
        .map_err(|e| anyhow::anyhow!("detokenize: {e}"))?;
    Ok(text.trim().to_string())
}

/// Feed the `[1,80,3000]` log-mel into the encoder, returning the
/// flattened `[1500*1024]` `layer_norm_48` output.
fn run_encoder(encoder: &mut Session, mel: &[Vec<f32>]) -> Result<Vec<f32>> {
    let mut flat = Vec::with_capacity(N_MELS * N_FRAMES);
    for row in mel {
        flat.extend_from_slice(row);
    }
    let shape: [i64; 3] = [1, N_MELS as i64, N_FRAMES as i64];
    let x = Tensor::from_array((shape, flat)).map_err(|e| anyhow::anyhow!("tensor mel: {e}"))?;
    let outputs = encoder
        .run(inputs!["x" => x])
        .map_err(|e| anyhow::anyhow!("encoder run: {e}"))?;
    let view = outputs[0]
        .try_extract_array::<f32>()
        .map_err(|e| anyhow::anyhow!("extract encoder output: {e}"))?;
    let v: Vec<f32> = view.iter().copied().collect();
    let want = ENC_FRAMES * ENC_DIM;
    if v.len() != want {
        anyhow::bail!("encoder output len {} != {want}", v.len());
    }
    Ok(v)
}

/// One decoder forward pass: bind the length-`MAX_DECODE` int64 ids and
/// the encoder output, return the `[len-1]`-position logits row (the
/// next-token distribution for the last real token).
fn run_decoder(
    decoder: &mut Session,
    ids: &[i64],
    pos: usize,
    encoder_out: &[f32],
    ids_first: bool,
) -> Result<Vec<f32>> {
    let id_shape: [i64; 2] = [1, MAX_DECODE as i64];
    let ids_t = Tensor::from_array((id_shape, ids.to_vec()))
        .map_err(|e| anyhow::anyhow!("tensor ids: {e}"))?;
    let xa_shape: [i64; 3] = [1, ENC_FRAMES as i64, ENC_DIM as i64];
    let xa_t = Tensor::from_array((xa_shape, encoder_out.to_vec()))
        .map_err(|e| anyhow::anyhow!("tensor xa: {e}"))?;
    let outputs = if ids_first {
        decoder.run(inputs!["x" => ids_t, "xa" => xa_t])
    } else {
        decoder.run(inputs!["xa" => xa_t, "x" => ids_t])
    }
    .map_err(|e| anyhow::anyhow!("decoder run: {e}"))?;
    let view = outputs[0]
        .try_extract_array::<f32>()
        .map_err(|e| anyhow::anyhow!("extract logits: {e}"))?;
    let logits: Vec<f32> = view.iter().copied().collect();
    if logits.len() != MAX_DECODE * VOCAB {
        anyhow::bail!("logits len {} != {}", logits.len(), MAX_DECODE * VOCAB);
    }
    let row = pos.saturating_sub(1).min(MAX_DECODE - 1);
    let start = row * VOCAB;
    Ok(logits[start..start + VOCAB].to_vec())
}

/// Greedy decode over a decoder step. `step` receives the length-128
/// int64 ids buffer (EOS-padded) and the current live-token count
/// `pos`, and returns the next-token logits row for the last real
/// token (`logits[0, pos-1, :]`). Stops on EOS or `MAX_DECODE`.
fn greedy_decode(
    initial: &[u32],
    eot: u32,
    mut step: impl FnMut(&[i64], usize) -> Result<Vec<f32>>,
) -> Result<Vec<u32>> {
    let mut tokens: Vec<u32> = initial.to_vec();
    while tokens.len() < MAX_DECODE {
        let mut ids = vec![eot as i64; MAX_DECODE];
        for (i, &t) in tokens.iter().enumerate() {
            ids[i] = t as i64;
        }
        let logits = step(&ids, tokens.len())?;
        let next = argmax(&logits) as u32;
        if next == eot {
            break;
        }
        tokens.push(next);
    }
    Ok(tokens)
}

fn argmax(v: &[f32]) -> usize {
    let mut best = 0usize;
    let mut best_v = f32::NEG_INFINITY;
    for (i, &x) in v.iter().enumerate() {
        if x > best_v {
            best_v = x;
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const EOT: u32 = 50257;
    const SOT: u32 = 50258;

    #[test]
    fn stt_advertises_correct_kind() {
        let w = SttWorkload::new();
        assert_eq!(w.kind(), WorkloadKind::Stt);
        assert_eq!(w.model_stem(), MODEL_STEM);
    }

    #[test]
    fn stt_rejects_text_input() {
        let w = SttWorkload::new();
        let res = w.run(WorkloadInput::Text {
            text: "not audio".into(),
        });
        assert!(res.is_err());
    }

    #[test]
    fn stt_rejects_wrong_sample_rate() {
        let w = SttWorkload::new();
        // load() never succeeds without the model, but the SR guard
        // fires before the state lookup, so it must error clearly.
        let res = w.run(WorkloadInput::Audio {
            pcm: vec![0; 16],
            sr: 8_000,
        });
        assert!(res.is_err());
    }

    #[test]
    fn stt_health_starts_unloaded() {
        let w = SttWorkload::new();
        let h = w.health();
        assert!(!h.loaded);
        assert_eq!(h.backend, "");
    }

    /// Greedy decode stops at EOS and yields the expected token run.
    /// The stub decoder emits a scripted sequence then EOS, keyed off
    /// the number of real tokens already in the buffer.
    #[test]
    fn greedy_decode_stops_at_eos_with_expected_tokens() {
        // Script: after [SOT] → 100, after [SOT,100] → 200, then EOS.
        let scripted = [100u32, 200u32, EOT];
        let step = |_ids: &[i64], pos: usize| -> Result<Vec<f32>> {
            let next = scripted[pos - 1];
            let mut logits = vec![0.0f32; VOCAB];
            logits[next as usize] = 10.0;
            Ok(logits)
        };
        let tokens = greedy_decode(&[SOT], EOT, step).expect("decode");
        assert_eq!(tokens, vec![SOT, 100, 200]);
    }

    /// The fixed-length ids buffer is EOS-padded and carries the live
    /// tokens in the leading slots — the contract `run_decoder` and the
    /// stub both rely on.
    #[test]
    fn greedy_decode_builds_eos_padded_fixed_buffer() {
        let mut seen_len = 0usize;
        let step = |ids: &[i64], pos: usize| -> Result<Vec<f32>> {
            assert_eq!(ids.len(), MAX_DECODE);
            assert_eq!(pos, 1);
            assert_eq!(ids[0], SOT as i64);
            // All trailing slots are EOS padding.
            assert_eq!(ids[MAX_DECODE - 1], EOT as i64);
            seen_len += 1;
            let mut logits = vec![0.0f32; VOCAB];
            logits[EOT as usize] = 1.0; // stop immediately
            Ok(logits)
        };
        let tokens = greedy_decode(&[SOT], EOT, step).expect("decode");
        assert_eq!(tokens, vec![SOT]);
        assert_eq!(seen_len, 1);
    }

    #[test]
    fn argmax_picks_largest_index() {
        assert_eq!(argmax(&[0.1, 0.9, 0.3]), 1);
        assert_eq!(argmax(&[5.0, 1.0, 2.0]), 0);
    }

    /// Detokenization of a known whisper id sequence via the loaded
    /// tokenizer matches the reference text. Skips when the model cache
    /// is absent (hermetic CI).
    #[test]
    fn tokenizer_decodes_known_ids_to_reference_text() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let path =
            PathBuf::from(home).join(".cache/sy/aiplane/whisper-medium/tokenizer/tokenizer.json");
        if !path.is_file() {
            eprintln!("skip: whisper-medium tokenizer.json not in cache");
            return;
        }
        let tk = Tokenizer::from_file(&path).expect("load tokenizer");
        // ids for "He hoped there would be stew" (no special tokens).
        let ids = [5205u32, 19737, 456, 576, 312, 22654];
        let text = tk.decode(&ids, true).expect("decode");
        assert_eq!(text.trim(), "He hoped there would be stew");
        assert_eq!(tk.token_to_id(SOT_TOKEN), Some(SOT));
        assert_eq!(tk.token_to_id(EOT_TOKEN), Some(EOT));
    }
}
