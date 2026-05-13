//! OCR. Target: Nemotron OCR v2 (detector + recogniser two-stage).
//! Input is a PNG/JPEG image (decoded via the `image` crate inside
//! `run()`); output is the extracted text.
//!
//! Two-stage architecture means TWO ONNX models, with the detector's
//! output (text bounding boxes) feeding the recogniser. The session
//! pool's NPU mutex covers both — the detector and recogniser share
//! one NPU dispatch session.
//!
//! Artifact layout:
//! `~/.cache/sy/aiplane/nemotron-ocr-v2/{detector,recogniser}.bf16.onnx`.
//!
//! Status: scaffolded. RyzenAI-SW already ships export+compile scripts
//! for this model; the prep script wraps them. See `/workload`.

use std::sync::Mutex;

use anyhow::Result;

use super::super::registry::{
    cache_root, Workload, WorkloadHealth, WorkloadInput, WorkloadKind, WorkloadOutput,
};
use super::super::session::SessionPool;

const MODEL_STEM: &str = "nemotron-ocr-v2";

pub struct OcrWorkload {
    loaded: Mutex<bool>,
}

impl OcrWorkload {
    pub fn new() -> Self {
        Self {
            loaded: Mutex::new(false),
        }
    }
}

impl Default for OcrWorkload {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for OcrWorkload {
    fn kind(&self) -> WorkloadKind {
        WorkloadKind::Ocr
    }

    fn model_stem(&self) -> &'static str {
        MODEL_STEM
    }

    fn load(&self, _pool: &SessionPool) -> Result<()> {
        let dir = cache_root().join(MODEL_STEM);
        let det = dir.join("detector.bf16.onnx");
        let rec = dir.join("recogniser.bf16.onnx");
        for p in [&det, &rec] {
            if !p.is_file() {
                anyhow::bail!(
                    "OCR stage missing at {}\nBuild it with:\n  \
                     source /opt/AMD/ryzenai/venv/bin/activate && \
                     python scripts/prep_npu_workload.py --workload ocr",
                    p.display()
                );
            }
        }
        *self.loaded.lock().expect("ocr loaded poisoned") = true;
        Ok(())
    }

    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput> {
        let bytes = match input {
            WorkloadInput::Image { bytes } => bytes,
            other => anyhow::bail!("ocr: expected Image input, got {other:?}"),
        };
        let _ = bytes.len();
        anyhow::bail!(
            "ocr: ONNX session not yet implemented; \
             prep the model with `python scripts/prep_npu_workload.py --workload ocr` \
             then fill in `run()` per the /workload skill."
        )
    }

    fn unload(&self) {
        *self.loaded.lock().expect("ocr loaded poisoned") = false;
    }

    fn health(&self) -> WorkloadHealth {
        let loaded = *self.loaded.lock().expect("ocr loaded poisoned");
        WorkloadHealth {
            state: if loaded {
                super::super::registry::WorkloadState::Ready {
                    backend: String::new(),
                }
            } else {
                super::super::registry::WorkloadState::NotPrepared
            },
            loaded,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_advertises_correct_kind() {
        let w = OcrWorkload::new();
        assert_eq!(w.kind(), WorkloadKind::Ocr);
    }

    #[test]
    fn ocr_rejects_audio_input() {
        let w = OcrWorkload::new();
        let res = w.run(WorkloadInput::Audio {
            pcm: vec![],
            sr: 16_000,
        });
        assert!(res.is_err());
    }
}
