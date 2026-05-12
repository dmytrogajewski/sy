//! Speech-to-text. Target model: `novasr` (locally available at
//! `~/sources/novasr-output/data/model/novasr.onnx`) with fallback to
//! `whisper-tiny` or `parakeet-tdt`. Input is 16 kHz mono PCM; output
//! is decoded text.
//!
//! Whisper-class encoders are heavy enough to want NPU; the decoder
//! is sequence-dependent and may stay on CPU. The artifact layout
//! splits these: `<stem>.encoder.bf16.onnx` + `<stem>.decoder.onnx`.
//!
//! Status: scaffolded. See `/workload` skill.

use std::sync::Mutex;

use anyhow::Result;

use super::super::registry::{
    cache_root, Workload, WorkloadHealth, WorkloadInput, WorkloadKind, WorkloadOutput,
};
use super::super::session::SessionPool;

const MODEL_STEM: &str = "novasr";

pub struct SttWorkload {
    loaded: Mutex<bool>,
}

impl SttWorkload {
    pub fn new() -> Self {
        Self {
            loaded: Mutex::new(false),
        }
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
        let dir = cache_root().join(MODEL_STEM);
        let model_path = dir.join(format!("{MODEL_STEM}.bf16.onnx"));
        if !model_path.is_file() {
            anyhow::bail!(
                "STT model not prepared at {}\nBuild it with:\n  \
                 source /opt/AMD/ryzenai/venv/bin/activate && \
                 python scripts/prep_npu_workload.py --workload stt",
                model_path.display()
            );
        }
        *self.loaded.lock().expect("stt loaded poisoned") = true;
        Ok(())
    }

    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput> {
        let (pcm, sr) = match input {
            WorkloadInput::Audio { pcm, sr } => (pcm, sr),
            other => anyhow::bail!("stt: expected Audio input, got {other:?}"),
        };
        if sr != 16_000 {
            anyhow::bail!("stt: expected 16 kHz input, got {sr}");
        }
        let _ = pcm.len();
        anyhow::bail!(
            "stt: ONNX session not yet implemented; \
             prep the model with `python scripts/prep_npu_workload.py --workload stt` \
             then fill in `run()` per the /workload skill."
        )
    }

    fn unload(&self) {
        *self.loaded.lock().expect("stt loaded poisoned") = false;
    }

    fn health(&self) -> WorkloadHealth {
        WorkloadHealth {
            loaded: *self.loaded.lock().expect("stt loaded poisoned"),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stt_advertises_correct_kind() {
        let w = SttWorkload::new();
        assert_eq!(w.kind(), WorkloadKind::Stt);
    }

    #[test]
    fn stt_rejects_text_input() {
        let w = SttWorkload::new();
        let res = w.run(WorkloadInput::Text {
            text: "not audio".into(),
        });
        assert!(res.is_err());
    }
}
