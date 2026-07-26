//! Workload vocabulary shared across the aiplane, knowledge, and IPC
//! surfaces. The `Workload` trait stays in `sy::aiplane::registry`
//! because it depends on `SessionPool`; only pure ser/de data shapes
//! live here so they can be referenced without pulling ORT into the
//! build tree.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Every workload class sy can host on the NPU plane. Stable wire
/// identifiers — adding a variant is allowed; renaming or removing
/// one is a breaking change for clients and qdrant/state migrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadKind {
    /// Text → fixed-dim sentence embedding.
    Embed,
    /// (query, doc) text pair → relevance score in `[0,1]`.
    Rerank,
    /// 16 kHz mono audio → speech/silence span list.
    Vad,
    /// 16 kHz mono audio → transcribed text.
    Stt,
    /// Text → WAV bytes.
    Tts,
    /// Image bytes → extracted text.
    Ocr,
    /// (image | text) → joint embedding.
    Clip,
    /// 48 kHz mono audio → denoised audio.
    Denoise,
    /// Image bytes → (x, y) gaze coordinate.
    EyeTrack,
}

impl WorkloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadKind::Embed => "embed",
            WorkloadKind::Rerank => "rerank",
            WorkloadKind::Vad => "vad",
            WorkloadKind::Stt => "stt",
            WorkloadKind::Tts => "tts",
            WorkloadKind::Ocr => "ocr",
            WorkloadKind::Clip => "clip",
            WorkloadKind::Denoise => "denoise",
            WorkloadKind::EyeTrack => "eye-track",
        }
    }

    pub const ALL: [WorkloadKind; 9] = [
        WorkloadKind::Embed,
        WorkloadKind::Rerank,
        WorkloadKind::Vad,
        WorkloadKind::Stt,
        WorkloadKind::Tts,
        WorkloadKind::Ocr,
        WorkloadKind::Clip,
        WorkloadKind::Denoise,
        WorkloadKind::EyeTrack,
    ];
}

impl fmt::Display for WorkloadKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkloadKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        for k in WorkloadKind::ALL {
            if s == k.as_str() {
                return Ok(k);
            }
        }
        anyhow::bail!(
            "unknown workload {s:?}; one of {:?}",
            WorkloadKind::ALL.map(|k| k.as_str())
        )
    }
}

/// Typed input variants. Each concrete `Workload` accepts a specific
/// variant; the registry validates the variant matches the requested
/// `WorkloadKind` before dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WorkloadInput {
    Text { text: String },
    TextPair { a: String, b: String },
    Audio { pcm: Vec<i16>, sr: u32 },
    Image { bytes: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WorkloadOutput {
    Vector { vector: Vec<f32> },
    Score { score: f32 },
    Text { text: String },
    Spans { spans: Vec<SpeechSpan> },
    Bytes { bytes: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechSpan {
    pub start_ms: u32,
    pub end_ms: u32,
    pub prob: f32,
}

/// Per-workload runtime state surfaced to `sy aiplane status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkloadHealth {
    /// Coarse lifecycle phase. Drives CLI / waybar messaging and
    /// short-circuits dispatch (a `Loading` worker returns "not
    /// ready" rather than blocking the request path).
    #[serde(default)]
    pub state: WorkloadState,
    /// Legacy: `state == Ready{..}` for any backend. Kept for
    /// backwards-compat with pre-supervisor status consumers.
    pub loaded: bool,
    /// Wall-clock seconds of the most recent successful `run()`.
    pub last_call_unix: u64,
    /// Exponential moving average of run latency in ms.
    pub ema_ms: f64,
    /// Total successful invocations since daemon start.
    pub calls: u64,
    /// Total failed invocations since daemon start.
    pub errors: u64,
    /// Effective execution provider after `load()` succeeded.
    /// `"vitisai"` / `"cpu"` / `""` (unloaded).
    pub backend: String,
}

/// Coarse lifecycle phase for a registered workload. Read by status
/// writers, the supervisor's child manager, and the dispatch path
/// (which short-circuits if a workload isn't `Ready`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum WorkloadState {
    /// Model artefact missing from `cache_dir()`. The user hasn't
    /// run `prep_npu_workload.py --workload <kind>` yet.
    #[default]
    NotPrepared,
    /// Background load thread is running (ONNX → VAIP partition →
    /// Session::commit). `dispatch` returns "not ready" while in
    /// this state rather than blocking the req worker.
    Loading,
    /// Session attached, serving requests. `backend` carries the
    /// effective execution provider for status display.
    Ready { backend: String },
    /// Load attempted and failed. The daemon won't auto-retry; the
    /// user must either fix the cause (re-prep the model, free the
    /// HW context) and restart the worker, or accept the degraded
    /// state. `reason` is the underlying error chain rendered.
    Failed { reason: String },
    /// Explicitly disabled in sy.toml `[aiplane] enabled_workloads`.
    Unavailable,
}

impl WorkloadState {
    pub fn is_ready(&self) -> bool {
        matches!(self, WorkloadState::Ready { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the wire schema. Every variant must round-trip; the
    /// kebab-case discriminator is what aiplane IPC + qdrant
    /// migrations rely on.
    #[test]
    fn workload_kind_round_trip() {
        for k in WorkloadKind::ALL {
            let j = serde_json::to_string(&k).expect("serialize");
            let back: WorkloadKind = serde_json::from_str(&j).expect("deserialize");
            assert_eq!(back, k);
        }
    }

    #[test]
    fn workload_kind_kebab_case_on_wire() {
        let j = serde_json::to_string(&WorkloadKind::EyeTrack).expect("serialize");
        assert_eq!(j, "\"eye-track\"");
    }

    #[test]
    fn workload_kind_from_str_round_trips_via_as_str() {
        for k in WorkloadKind::ALL {
            assert_eq!(k.as_str().parse::<WorkloadKind>().expect("parse"), k);
        }
    }

    #[test]
    fn workload_kind_from_str_rejects_unknown() {
        assert!("nonsense".parse::<WorkloadKind>().is_err());
    }

    /// Pins every input variant's tagged-union shape on the wire.
    #[test]
    fn workload_input_tagged_union_round_trip() {
        let cases = [
            WorkloadInput::Text {
                text: "hello".into(),
            },
            WorkloadInput::TextPair {
                a: "q".into(),
                b: "d".into(),
            },
            WorkloadInput::Audio {
                pcm: vec![1, -1, 2, -2],
                sr: 16_000,
            },
            WorkloadInput::Image {
                bytes: vec![0xff, 0x00, 0xaa],
            },
        ];
        for c in cases {
            let j = serde_json::to_string(&c).expect("serialize");
            let back: WorkloadInput = serde_json::from_str(&j).expect("deserialize");
            // Compare via re-serialise since WorkloadInput has no Eq.
            assert_eq!(
                serde_json::to_string(&back).expect("re-serialize"),
                serde_json::to_string(&c).expect("serialize"),
            );
        }
    }

    #[test]
    fn workload_output_tagged_union_round_trip() {
        let span = SpeechSpan {
            start_ms: 0,
            end_ms: 480,
            prob: 0.97,
        };
        let cases = [
            WorkloadOutput::Vector {
                vector: vec![0.1, 0.2, 0.3],
            },
            WorkloadOutput::Score { score: 0.42 },
            WorkloadOutput::Text {
                text: "transcript".into(),
            },
            WorkloadOutput::Spans {
                spans: vec![span.clone()],
            },
            WorkloadOutput::Bytes {
                bytes: vec![1, 2, 3, 4],
            },
        ];
        for c in cases {
            let j = serde_json::to_string(&c).expect("serialize");
            let back: WorkloadOutput = serde_json::from_str(&j).expect("deserialize");
            assert_eq!(
                serde_json::to_string(&back).expect("re-serialize"),
                serde_json::to_string(&c).expect("serialize"),
            );
        }
    }

    #[test]
    fn workload_state_ready_serializes_with_backend() {
        let s = WorkloadState::Ready {
            backend: "vitisai".into(),
        };
        let j = serde_json::to_string(&s).expect("serialize");
        assert!(j.contains("\"state\":\"ready\""));
        assert!(j.contains("\"backend\":\"vitisai\""));
        let back: WorkloadState = serde_json::from_str(&j).expect("deserialize");
        assert_eq!(back, s);
    }

    #[test]
    fn workload_state_default_is_not_prepared() {
        assert_eq!(WorkloadState::default(), WorkloadState::NotPrepared);
        assert!(!WorkloadState::default().is_ready());
    }

    #[test]
    fn workload_health_default_serializes_with_not_prepared() {
        let h = WorkloadHealth::default();
        let j = serde_json::to_string(&h).expect("serialize");
        assert!(j.contains("\"state\":\"not-prepared\""));
        assert!(j.contains("\"loaded\":false"));
    }
}
