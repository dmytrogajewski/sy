//! Workload registry: enumerates every NPU-eligible workload sy can
//! host inside the `sy-aiplane.service` daemon and dispatches typed
//! input/output between the IPC layer and concrete `Workload` impls.
//!
//! The registry is **the** generalisation point of the aiplane crate:
//! adding a new workload is a Workload-trait impl + one line in
//! `workloads::register_all()` + (optionally) new variants in
//! `WorkloadInput`/`WorkloadOutput`. Everything else — IPC ser/de,
//! session pool, status snapshot, CLI dispatch — picks it up by
//! enumerating `WorkloadKind`.

use std::{fmt, path::PathBuf, sync::Mutex, time::Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::session::SessionPool;

/// Every workload class sy can host on the NPU plane. Stable wire
/// identifiers — adding a variant is allowed; renaming or removing
/// one is a breaking change for clients and qdrant/state migrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadKind {
    /// Text → fixed-dim sentence embedding.
    Embed,
    /// (query, doc) text pair → relevance score in [0,1].
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

impl std::str::FromStr for WorkloadKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
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

/// Anything that can serve an NPU-eligible workload through the
/// shared `SessionPool`. The trait is intentionally non-async:
/// concurrency is handled by the registry's worker thread which
/// dispatches one request at a time per `WorkloadKind`. NPU
/// serialisation is enforced by the `SessionPool`'s NPU mutex.
pub trait Workload: Send + Sync {
    fn kind(&self) -> WorkloadKind;

    /// Human-readable model identifier surfaced to status / logs
    /// (e.g. `"multilingual-e5-base"`). Used as the on-disk
    /// directory name under `~/.cache/sy/aiplane/<stem>/`.
    fn model_stem(&self) -> &'static str;

    /// Idempotent. The pool calls this before the first `run()`.
    /// Cached state lives behind `&self` (a `Mutex<Option<...>>`
    /// inside the impl) so subsequent loads are cheap no-ops.
    fn load(&self, pool: &SessionPool) -> Result<()>;

    /// Run one inference. Implementations validate the input
    /// variant matches what they expect; mismatched variants
    /// return a clear error rather than panicking.
    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput>;

    /// Run a batched inference. Default impl loops over `run`; the
    /// supervisor calls this from `run_batch` so a workload that
    /// supports session-level batching (e.g. a rerank model exported
    /// at `(B, 512)`) can override and turn N kernel launches into
    /// one. Returns one output per input, in order, or `Err` on the
    /// first failure.
    fn run_batch(&self, inputs: Vec<WorkloadInput>) -> Result<Vec<WorkloadOutput>> {
        let mut out = Vec::with_capacity(inputs.len());
        for input in inputs {
            out.push(self.run(input)?);
        }
        Ok(out)
    }

    /// Best-effort release of the loaded ORT session. Called by the
    /// pool's LRU eviction when memory pressure forces it. Workloads
    /// that hold extra state (tokenizers, image preprocessors) drop
    /// them here too.
    fn unload(&self);

    fn health(&self) -> WorkloadHealth;
}

/// The registry the daemon's req worker dispatches through. Owns one
/// boxed `Workload` per kind plus the shared `SessionPool` they share.
pub struct Registry {
    pub pool: std::sync::Arc<SessionPool>,
    workloads: std::collections::HashMap<WorkloadKind, std::sync::Arc<dyn Workload>>,
    stats: Mutex<std::collections::HashMap<WorkloadKind, WorkloadHealth>>,
}

impl Registry {
    pub fn new(pool: std::sync::Arc<SessionPool>) -> Self {
        Self {
            pool,
            workloads: std::collections::HashMap::new(),
            stats: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Register one `Workload`. Panics if the kind is already
    /// registered — registration happens once at daemon startup,
    /// double-registration is a bug.
    pub fn register(&mut self, w: std::sync::Arc<dyn Workload>) {
        let k = w.kind();
        if self.workloads.contains_key(&k) {
            panic!("workload {k} registered twice");
        }
        self.workloads.insert(k, w);
    }

    pub fn kinds(&self) -> Vec<WorkloadKind> {
        let mut v: Vec<_> = self.workloads.keys().copied().collect();
        v.sort_by_key(|k| k.as_str());
        v
    }

    pub fn run(&self, kind: WorkloadKind, input: WorkloadInput) -> Result<WorkloadOutput> {
        let w = self
            .workloads
            .get(&kind)
            .ok_or_else(|| anyhow::anyhow!("workload {kind} not registered"))?
            .clone();
        // Lazy load on first call.
        w.load(&self.pool)?;
        let t0 = Instant::now();
        let res = w.run(input);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut stats = self.stats.lock().expect("stats poisoned");
        let entry = stats.entry(kind).or_default();
        match &res {
            Ok(_) => {
                entry.calls += 1;
                entry.last_call_unix = unix_now();
                // EMA with alpha=0.2.
                entry.ema_ms = if entry.ema_ms == 0.0 {
                    elapsed_ms
                } else {
                    0.2 * elapsed_ms + 0.8 * entry.ema_ms
                };
            }
            Err(_) => {
                entry.errors += 1;
            }
        }
        res
    }

    pub fn health(&self, kind: WorkloadKind) -> WorkloadHealth {
        let mut h = self
            .stats
            .lock()
            .expect("stats poisoned")
            .get(&kind)
            .cloned()
            .unwrap_or_default();
        if let Some(w) = self.workloads.get(&kind) {
            let from_workload = w.health();
            h.state = from_workload.state;
            h.loaded = from_workload.loaded;
            h.backend = from_workload.backend;
        }
        h
    }

    /// Snapshot of every registered workload's health, keyed by
    /// `WorkloadKind::as_str()`. Used by the daemon's status writer
    /// to enumerate *all* registered kinds, not just the ones that
    /// have been called. Unregistered kinds are omitted.
    pub fn all_health(&self) -> std::collections::HashMap<String, WorkloadHealth> {
        self.kinds()
            .into_iter()
            .map(|k| (k.as_str().to_string(), self.health(k)))
            .collect()
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `~/.cache/sy/aiplane/` (overridable via `SY_AIPLANE_CACHE_DIR` for
/// tests). All workload caches live under this.
pub fn cache_root() -> PathBuf {
    if let Some(v) = std::env::var_os("SY_AIPLANE_CACHE_DIR") {
        return PathBuf::from(v);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/sy/aiplane")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip_via_str() {
        for k in WorkloadKind::ALL {
            assert_eq!(k.as_str().parse::<WorkloadKind>().unwrap(), k);
        }
    }

    #[test]
    fn kind_rejects_unknown_string() {
        assert!("nonsense".parse::<WorkloadKind>().is_err());
    }

    #[test]
    fn cache_root_respects_override() {
        std::env::set_var("SY_AIPLANE_CACHE_DIR", "/tmp/sy-test-cache");
        assert_eq!(cache_root(), PathBuf::from("/tmp/sy-test-cache"));
        std::env::remove_var("SY_AIPLANE_CACHE_DIR");
    }

    #[test]
    fn registry_dispatches_to_registered_workload_via_trait_object() {
        // Exercises the full path: register `dyn Workload`, dispatch
        // through Registry::run, observe the trait's run/load are
        // both invoked. Also pulls WorkloadHealth + the EMA counter
        // through enough code that the compiler stops calling them
        // dead.
        use super::super::session::SessionPool;
        use super::super::workloads::fake::FakeWorkload;
        use std::sync::Arc;

        let pool = Arc::new(SessionPool::new());
        let mut reg = Registry::new(pool);
        reg.register(Arc::new(FakeWorkload::embed()));

        assert_eq!(reg.kinds(), vec![WorkloadKind::Embed]);

        let out = reg
            .run(
                WorkloadKind::Embed,
                WorkloadInput::Text {
                    text: "hello".into(),
                },
            )
            .expect("dispatch");
        match out {
            WorkloadOutput::Vector { vector } => assert!(!vector.is_empty()),
            _ => panic!("expected Vector"),
        }

        let h = reg.health(WorkloadKind::Embed);
        assert!(h.loaded);
        assert!(h.calls >= 1);
        assert_eq!(h.backend, "fake");
    }

    #[test]
    fn all_health_enumerates_every_registered_kind() {
        // The status snapshot must list a row for every kind in
        // the registry, even those that have never been called —
        // that's how `NotPrepared` and `Failed` states become
        // visible to the user without first triggering a request.
        use super::super::session::SessionPool;
        use super::super::workloads::fake::FakeWorkload;
        use std::sync::Arc;
        let pool = Arc::new(SessionPool::new());
        let mut reg = Registry::new(pool);
        reg.register(Arc::new(FakeWorkload::new(WorkloadKind::Embed)));
        reg.register(Arc::new(FakeWorkload::new(WorkloadKind::Rerank)));
        let all = reg.all_health();
        assert_eq!(all.len(), 2);
        assert!(all.contains_key("embed"));
        assert!(all.contains_key("rerank"));
        // Both unloaded → NotPrepared, not Ready.
        for (_, h) in &all {
            assert_eq!(h.state, WorkloadState::NotPrepared);
            assert!(!h.loaded);
        }
    }

    #[test]
    fn workload_state_ready_serializes_with_backend() {
        let s = WorkloadState::Ready {
            backend: "vitisai".into(),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"state\":\"ready\""));
        assert!(j.contains("\"backend\":\"vitisai\""));
        let back: WorkloadState = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn workload_state_default_is_not_prepared() {
        assert_eq!(WorkloadState::default(), WorkloadState::NotPrepared);
        assert!(!WorkloadState::default().is_ready());
    }

    #[test]
    fn registry_rejects_unregistered_kind() {
        use super::super::session::SessionPool;
        use std::sync::Arc;
        let reg = Registry::new(Arc::new(SessionPool::new()));
        let res = reg.run(
            WorkloadKind::Vad,
            WorkloadInput::Audio {
                pcm: vec![],
                sr: 16_000,
            },
        );
        assert!(res.is_err());
    }
}
