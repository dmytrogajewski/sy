//! Deterministic CPU-only `Workload` used by tests to exercise the
//! daemon, IPC, and registry plumbing without `/dev/accel/accel0`.
//!
//! Behaviour is keyed on the input text/bytes hash so the same input
//! always produces the same output — integration tests can assert
//! exact equality.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::super::registry::{
    Workload, WorkloadHealth, WorkloadInput, WorkloadKind, WorkloadOutput,
};
use super::super::session::SessionPool;
use super::VECTOR_DIM;

/// Polling granularity used by the cancellable sleep in
/// [`FakeWorkload`]. Small enough that the cancel test reliably
/// observes interruption within its 1 s deadline.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub struct FakeWorkload {
    /// We pretend to be whatever kind the test wants.
    kind: WorkloadKind,
    loaded: Mutex<bool>,
    /// Per-call cooperative sleep (ms). Zero means "no sleep". Used by
    /// the worker-cancel tests to keep a single inference inflight
    /// long enough for a Cancel to arrive.
    sleep_ms: u64,
    /// Cooperative cancel flag. [`Workload::try_cancel`] sets it; the
    /// `sleep_ms` polling loop in [`Workload::run`] observes it and
    /// returns `Err(anyhow!("cancelled"))` early.
    cancel: AtomicBool,
}

impl FakeWorkload {
    pub fn new(kind: WorkloadKind) -> Self {
        Self {
            kind,
            loaded: Mutex::new(false),
            sleep_ms: 0,
            cancel: AtomicBool::new(false),
        }
    }

    /// Embed-flavoured fake: returns a deterministic VECTOR_DIM-length
    /// vector derived from the bytes of `text`. Useful as the only
    /// `Embed` workload registered in tests.
    pub fn embed() -> Self {
        Self::new(WorkloadKind::Embed)
    }

    /// Build a cancellable fake whose `run` sleeps for `sleep_ms`
    /// before returning. Used by worker / supervisor cancel tests.
    pub fn with_sleep_ms(kind: WorkloadKind, sleep_ms: u64) -> Self {
        Self {
            kind,
            loaded: Mutex::new(false),
            sleep_ms,
            cancel: AtomicBool::new(false),
        }
    }
}

impl Workload for FakeWorkload {
    fn kind(&self) -> WorkloadKind {
        self.kind
    }

    fn model_stem(&self) -> &'static str {
        "fake"
    }

    fn load(&self, _pool: &SessionPool) -> Result<()> {
        *self.loaded.lock().expect("fake loaded poisoned") = true;
        Ok(())
    }

    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput> {
        if self.sleep_ms > 0 {
            let deadline = Instant::now() + Duration::from_millis(self.sleep_ms);
            while Instant::now() < deadline {
                if self.cancel.load(Ordering::SeqCst) {
                    self.cancel.store(false, Ordering::SeqCst);
                    anyhow::bail!("cancelled");
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(CANCEL_POLL_INTERVAL));
            }
        }
        let bytes: Vec<u8> = match &input {
            WorkloadInput::Text { text } => text.as_bytes().to_vec(),
            WorkloadInput::TextPair { a, b } => {
                let mut v = a.as_bytes().to_vec();
                v.push(0);
                v.extend_from_slice(b.as_bytes());
                v
            }
            WorkloadInput::Audio { pcm, .. } => pcm.iter().flat_map(|s| s.to_le_bytes()).collect(),
            WorkloadInput::Image { bytes } => bytes.clone(),
        };
        Ok(match self.kind {
            WorkloadKind::Embed | WorkloadKind::Clip => WorkloadOutput::Vector {
                vector: fake_vector(&bytes, VECTOR_DIM),
            },
            WorkloadKind::Rerank => WorkloadOutput::Score {
                score: fake_score(&bytes),
            },
            WorkloadKind::Stt | WorkloadKind::Ocr => WorkloadOutput::Text {
                text: format!("fake-output ({} bytes)", bytes.len()),
            },
            WorkloadKind::Tts | WorkloadKind::Denoise => WorkloadOutput::Bytes { bytes },
            WorkloadKind::Vad => WorkloadOutput::Spans { spans: Vec::new() },
            WorkloadKind::EyeTrack => WorkloadOutput::Vector {
                vector: vec![0.5, 0.5],
            },
        })
    }

    fn unload(&self) {
        *self.loaded.lock().expect("fake loaded poisoned") = false;
    }

    fn try_cancel(&self) -> bool {
        self.cancel.store(true, Ordering::SeqCst);
        true
    }

    fn health(&self) -> WorkloadHealth {
        let loaded = *self.loaded.lock().expect("fake loaded poisoned");
        WorkloadHealth {
            state: if loaded {
                super::super::registry::WorkloadState::Ready {
                    backend: "fake".to_string(),
                }
            } else {
                super::super::registry::WorkloadState::NotPrepared
            },
            loaded,
            backend: "fake".to_string(),
            ..Default::default()
        }
    }
}

fn fake_vector(seed: &[u8], dim: usize) -> Vec<f32> {
    // Tiny LCG seeded from the input — deterministic, no `rand`
    // dependency, and the resulting vector is L2-normalisable by
    // tests if they care. Output range is [-1, 1].
    let mut state: u64 = seed.iter().fold(0xCBF2_9CE4_8422_2325u64, |acc, &b| {
        (acc ^ b as u64).wrapping_mul(0x0100_0000_01B3)
    });
    let mut out = Vec::with_capacity(dim);
    let mut sumsq = 0.0_f64;
    for _ in 0..dim {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let v = (state >> 33) as i32 as f32 / (i32::MAX as f32);
        out.push(v);
        sumsq += (v as f64).powi(2);
    }
    let norm = sumsq.sqrt() as f32;
    if norm > 1e-9 {
        for v in &mut out {
            *v /= norm;
        }
    }
    out
}

fn fake_score(seed: &[u8]) -> f32 {
    let h = seed
        .iter()
        .fold(0u32, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u32));
    (h as f32 / u32::MAX as f32).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embed_returns_unit_norm_vector() {
        let w = FakeWorkload::embed();
        w.load(&SessionPool::new()).unwrap();
        let out = w
            .run(WorkloadInput::Text {
                text: "hello".into(),
            })
            .unwrap();
        let v = match out {
            WorkloadOutput::Vector { vector } => vector,
            _ => panic!("expected Vector"),
        };
        assert_eq!(v.len(), VECTOR_DIM);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "norm should be ~1, got {norm}");
    }

    #[test]
    fn fake_embed_is_deterministic() {
        let w = FakeWorkload::embed();
        w.load(&SessionPool::new()).unwrap();
        let v1 = w.run(WorkloadInput::Text { text: "x".into() }).unwrap();
        let v2 = w.run(WorkloadInput::Text { text: "x".into() }).unwrap();
        match (v1, v2) {
            (WorkloadOutput::Vector { vector: a }, WorkloadOutput::Vector { vector: b }) => {
                assert_eq!(a, b);
            }
            _ => panic!("expected Vector"),
        }
    }

    #[test]
    fn cancellable_sleep_returns_early_when_try_cancel_fires() {
        // SPEC §4.3 "Cancellation pattern" / ROADMAP Step 4: a
        // `with_sleep_ms` fake hits `try_cancel` from another thread
        // and surfaces the inflight `run` with an "cancelled" error
        // well before the sleep deadline. Without the cooperative
        // hook, this test would block for the full SLEEP_MS.
        use std::sync::Arc;
        use std::thread;
        const SLEEP_MS: u64 = 5_000;
        const CANCEL_AFTER_MS: u64 = 50;
        const DEADLINE_MS: u64 = 1_000;
        let w = Arc::new(FakeWorkload::with_sleep_ms(WorkloadKind::Embed, SLEEP_MS));
        w.load(&SessionPool::new()).unwrap();
        let w_cancel = Arc::clone(&w);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(CANCEL_AFTER_MS));
            assert!(w_cancel.try_cancel(), "cancellable fake claims cancel");
        });
        let t0 = Instant::now();
        let err = w
            .run(WorkloadInput::Text { text: "hi".into() })
            .expect_err("inflight run cancels");
        canceller.join().expect("canceller joins");
        let elapsed = t0.elapsed();
        assert!(
            err.to_string().contains("cancelled"),
            "error message should advertise the cancel: {err}"
        );
        assert!(
            elapsed < Duration::from_millis(DEADLINE_MS),
            "cancel must beat the {DEADLINE_MS}ms deadline; took {elapsed:?}"
        );
    }

    #[test]
    fn fake_rerank_score_in_range() {
        let w = FakeWorkload::new(WorkloadKind::Rerank);
        w.load(&SessionPool::new()).unwrap();
        let out = w
            .run(WorkloadInput::TextPair {
                a: "query".into(),
                b: "doc".into(),
            })
            .unwrap();
        match out {
            WorkloadOutput::Score { score } => {
                assert!((0.0..=1.0).contains(&score));
            }
            _ => panic!("expected Score"),
        }
    }
}
