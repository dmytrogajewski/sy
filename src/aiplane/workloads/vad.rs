//! Voice-activity detection. Target model: `snakers4/silero-vad`
//! (~2 MB ONNX). Input is 16 kHz mono PCM in 32 ms frames (512
//! samples); output is speech probability per frame, post-processed
//! into `SpeechSpan` ranges with hysteresis.
//!
//! silero-vad is small enough that the NPU dispatch overhead exceeds
//! the inference cost — this Workload declares CPU as its preferred
//! EP (skips the NPU mutex acquisition in `SessionPool::with_npu`).
//!
//! Status: scaffolded. See `/workload` skill for the registration
//! checklist + `/npu-prep` for the artifact prep.

use std::sync::Mutex;

use anyhow::Result;

use super::super::registry::{
    cache_root, Workload, WorkloadHealth, WorkloadInput, WorkloadKind, WorkloadOutput,
};
use super::super::session::SessionPool;

const MODEL_STEM: &str = "silero-vad";

pub struct VadWorkload {
    loaded: Mutex<bool>,
}

impl VadWorkload {
    pub fn new() -> Self {
        Self {
            loaded: Mutex::new(false),
        }
    }
}

impl Default for VadWorkload {
    fn default() -> Self {
        Self::new()
    }
}

impl Workload for VadWorkload {
    fn kind(&self) -> WorkloadKind {
        WorkloadKind::Vad
    }

    fn model_stem(&self) -> &'static str {
        MODEL_STEM
    }

    fn load(&self, _pool: &SessionPool) -> Result<()> {
        let dir = cache_root().join(MODEL_STEM);
        let model_path = dir.join(format!("{MODEL_STEM}.onnx"));
        if !model_path.is_file() {
            anyhow::bail!(
                "VAD model not prepared at {}\nBuild it with:\n  \
                 source /opt/AMD/ryzenai/venv/bin/activate && \
                 python scripts/prep_npu_workload.py --workload vad",
                model_path.display()
            );
        }
        *self.loaded.lock().expect("vad loaded poisoned") = true;
        Ok(())
    }

    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput> {
        let (pcm, sr) = match input {
            WorkloadInput::Audio { pcm, sr } => (pcm, sr),
            other => anyhow::bail!("vad: expected Audio input, got {other:?}"),
        };
        if sr != 16_000 {
            anyhow::bail!("vad: expected 16 kHz input, got {sr}");
        }
        if pcm.is_empty() {
            return Ok(WorkloadOutput::Spans { spans: Vec::new() });
        }
        anyhow::bail!(
            "vad: ONNX session not yet implemented; \
             prep the model with `python scripts/prep_npu_workload.py --workload vad` \
             then fill in `run()` per the /workload skill."
        )
    }

    fn unload(&self) {
        *self.loaded.lock().expect("vad loaded poisoned") = false;
    }

    fn health(&self) -> WorkloadHealth {
        let loaded = *self.loaded.lock().expect("vad loaded poisoned");
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
    fn vad_advertises_correct_kind() {
        let w = VadWorkload::new();
        assert_eq!(w.kind(), WorkloadKind::Vad);
    }

    #[test]
    fn vad_rejects_wrong_sample_rate() {
        let w = VadWorkload::new();
        let res = w.run(WorkloadInput::Audio {
            pcm: vec![0; 100],
            sr: 44_100,
        });
        assert!(res.is_err());
    }

    #[test]
    fn vad_accepts_empty_audio_silently() {
        let w = VadWorkload::new();
        let out = w
            .run(WorkloadInput::Audio {
                pcm: vec![],
                sr: 16_000,
            })
            .unwrap();
        match out {
            WorkloadOutput::Spans { spans } => assert!(spans.is_empty()),
            _ => panic!("expected empty Spans"),
        }
    }
}
