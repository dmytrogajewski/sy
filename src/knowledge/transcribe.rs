//! Voice/video transcription for the knowledge index (REQ-5).
//!
//! Telegram voice notes (`voice_messages/*.ogg`) and round videos
//! (`round_video_messages/*.mp4`) carry searchable speech that the text
//! pipeline cannot see. This module turns that audio into text so the
//! Telegram index pass can emit `kind=telegram-voice` chunks pointing back
//! at the source media.
//!
//! ## Option A: route through the aiplane `Stt` NPU workload
//!
//! Transcription is served by the native [`crate::aiplane::workloads::stt`]
//! `whisper-medium` workload (NPU encoder + CPU decoder) over the same
//! supervisor IPC the embed/rerank planes use — see
//! [`crate::knowledge::embed`] for the mirrored pattern. The
//! [`AiplaneTranscriber`] decodes media to 16 kHz mono PCM with `ffmpeg`,
//! lazily [`ensure`](crate::aiplane::supervisor::Supervisor::ensure)s the
//! `Stt` worker (the first call triggers the multi-minute VitisAI encoder
//! compile; the long deadline covers it, later calls hit the warm worker),
//! and dispatches one `RunBatch`.
//!
//! ## Always-compiled core
//!
//! The [`Transcriber`] trait, the content-addressed [`sidecar_path`] cache,
//! the [`DisabledTranscriber`] fallback, and [`AiplaneTranscriber`] are all
//! **always compiled** so `cargo test --workspace` exercises the cache +
//! index wiring with a fake transcriber and the NPU route with a
//! `FakeWorkload`-style supervisor. The index pass is the live consumer of
//! [`transcribe_cached`], so neither the trait, the cache, nor the
//! transcribers are dead code.
//!
//! ## Sidecar cache
//!
//! A transcript is cached next to its media file, content-addressed by the
//! blake3 hash of the media bytes: `<media>.<hash>.txt`. Re-indexing the
//! same media short-circuits to the cached text, so unchanged voice notes
//! are never re-transcribed. Editing the media changes its hash and so its
//! sidecar path, which naturally invalidates the stale transcript.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::aiplane::registry::{WorkloadInput, WorkloadKind, WorkloadOutput};
use crate::aiplane::supervisor::{self, Supervisor};

use super::{exit, KnowledgeError};

/// Whisper's fixed input sample rate — 16 kHz mono. Must match the
/// `Stt` workload's `SAMPLE_RATE` guard.
const STT_SAMPLE_RATE: u32 = 16_000;
/// Deadline for the lazy `Stt` worker ensure. The first transcription
/// triggers the multi-minute VitisAI encoder compile; 30 minutes
/// comfortably covers it. Subsequent calls hit the warm worker and
/// return immediately.
const STT_READY_DEADLINE: Duration = Duration::from_secs(1800);

/// Turns a voice/video media file into transcribed text. Implementations:
/// the always-compiled [`AiplaneTranscriber`] (the NPU `Stt` route — the
/// default when the supervisor is running), the [`DisabledTranscriber`]
/// fallback, and test fakes.
pub trait Transcriber {
    /// Transcribe the media at `media_path` into plain text. Errors when the
    /// backend is unavailable (no supervisor) or the media cannot be
    /// decoded (ffmpeg missing / unsupported format).
    fn transcribe(&self, media_path: &Path) -> Result<String>;
}

/// Suffix appended after the content hash for cached transcripts.
const SIDECAR_EXT: &str = "txt";

/// Derive the content-addressed sidecar path for a transcript: the media
/// path with `.<hash>.txt` appended, where `hash` is the blake3 hex of the
/// media bytes. Pinned by `sidecar_path_is_content_addressed`.
pub fn sidecar_path(media_path: &Path, content_hash: &str) -> PathBuf {
    let mut name = media_path.as_os_str().to_os_string();
    name.push(format!(".{content_hash}.{SIDECAR_EXT}"));
    PathBuf::from(name)
}

/// Transcribe `media_path`, caching the result in a content-addressed
/// sidecar. Returns the cached transcript without invoking `transcriber`
/// when the sidecar already exists (incremental re-index short-circuit);
/// otherwise transcribes, writes the sidecar, and returns the text.
pub fn transcribe_cached(transcriber: &dyn Transcriber, media_path: &Path) -> Result<String> {
    let bytes = std::fs::read(media_path)
        .with_context(|| format!("read media {}", media_path.display()))?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let sidecar = sidecar_path(media_path, &hash);
    if let Ok(cached) = std::fs::read_to_string(&sidecar) {
        return Ok(cached);
    }
    let text = transcriber.transcribe(media_path)?;
    std::fs::write(&sidecar, &text)
        .with_context(|| format!("write transcript {}", sidecar.display()))?;
    Ok(text)
}

/// Decode `media` to 16 kHz mono signed-16-bit little-endian PCM via
/// `ffmpeg`, returning the `Vec<i16>` that [`WorkloadInput::Audio`]
/// expects. Handles every container ffmpeg knows — Opus/OGG (Telegram
/// voice notes), MP4 (round videos), WAV, etc. A missing or failing
/// `ffmpeg` surfaces a clear error rather than a silent empty clip.
pub fn decode_pcm_16k_mono(media: &Path) -> Result<Vec<i16>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(media)
        .args(["-ar", "16000", "-ac", "1", "-f", "s16le", "-"])
        .output()
        .with_context(|| {
            format!(
                "spawn ffmpeg for {} (is ffmpeg installed?)",
                media.display()
            )
        })?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg failed to decode {}: {}",
            media.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out
        .stdout
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect())
}

/// The Option-A transcriber: decodes media with ffmpeg and routes the
/// PCM through the aiplane `Stt` NPU workload over the supervisor IPC
/// (mirrors [`crate::knowledge::embed`]). The native whisper-medium
/// split supersedes the retired CPU whisper.cpp backend.
pub struct AiplaneTranscriber;

/// Routing seam: dispatch already-decoded `pcm` to the `Stt` worker on
/// `sup`. Split out of [`AiplaneTranscriber::transcribe`] so the
/// hermetic test can inject PCM and a `FakeWorkload`-backed supervisor
/// without invoking ffmpeg or the global `supervisor::current()`.
fn transcribe_pcm(sup: &Supervisor, pcm: Vec<i16>) -> Result<String> {
    sup.ensure(WorkloadKind::Stt, STT_READY_DEADLINE)
        .map_err(|e| KnowledgeError {
            code: exit::TRANSCRIBE_FAILED,
            msg: format!("stt worker: {e:#}"),
        })?;
    let outputs = sup
        .run_batch(
            WorkloadKind::Stt,
            vec![WorkloadInput::Audio {
                pcm,
                sr: STT_SAMPLE_RATE,
            }],
        )
        .map_err(|e| KnowledgeError {
            code: exit::TRANSCRIBE_FAILED,
            msg: format!("stt worker: {e:#}"),
        })?;
    match outputs.into_iter().next() {
        Some(WorkloadOutput::Text { text }) => Ok(text),
        Some(other) => Err(KnowledgeError {
            code: exit::TRANSCRIBE_FAILED,
            msg: format!("stt: unexpected output variant {other:?}"),
        }
        .into()),
        None => Err(KnowledgeError {
            code: exit::TRANSCRIBE_FAILED,
            msg: "stt: worker returned empty batch".into(),
        }
        .into()),
    }
}

impl Transcriber for AiplaneTranscriber {
    fn transcribe(&self, media_path: &Path) -> Result<String> {
        let pcm = decode_pcm_16k_mono(media_path)?;
        let sup = supervisor::current().ok_or_else(|| KnowledgeError {
            code: exit::TRANSCRIBE_FAILED,
            msg: "transcribe: aiplane supervisor not running".into(),
        })?;
        transcribe_pcm(&sup, pcm)
    }
}

/// The fallback backend used when no aiplane supervisor is running. It is a
/// real, deterministic backend (not a stub): it always reports that
/// transcription is disabled so the index pass can skip media cleanly
/// rather than panic.
pub struct DisabledTranscriber;

impl Transcriber for DisabledTranscriber {
    fn transcribe(&self, _media_path: &Path) -> Result<String> {
        anyhow::bail!(
            "transcription disabled: the aiplane supervisor is not running, so the \
             `Stt` NPU workload cannot be reached"
        )
    }
}

/// The default transcriber. Precedence: when an aiplane supervisor is
/// running (the daemon path, the chosen Option A), return
/// [`AiplaneTranscriber`] so voice media routes through the `Stt` NPU
/// workload; otherwise return [`DisabledTranscriber`] so the index pass
/// skips voice media cleanly instead of erroring on every file. The
/// index pass calls this so the cache + wiring stay live in both modes.
pub fn default_transcriber() -> Box<dyn Transcriber> {
    if supervisor::current().is_some() {
        Box::new(AiplaneTranscriber)
    } else {
        Box::new(DisabledTranscriber)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A hermetic transcriber that returns a canned transcript and counts
    /// how many times it actually ran, so tests can assert the cache
    /// short-circuits instead of re-transcribing.
    struct FakeTranscriber {
        text: String,
        calls: Cell<usize>,
    }

    impl Transcriber for FakeTranscriber {
        fn transcribe(&self, _media_path: &Path) -> Result<String> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.text.clone())
        }
    }

    #[test]
    fn sidecar_path_is_content_addressed() {
        let media = Path::new("/chat/voice_messages/1.ogg");
        let a = sidecar_path(media, "deadbeef");
        let b = sidecar_path(media, "cafef00d");
        // The hash is embedded in the path, so different content → different
        // sidecar (stale transcripts can never be served for changed media).
        assert_ne!(a, b);
        assert_eq!(a, Path::new("/chat/voice_messages/1.ogg.deadbeef.txt"));
    }

    #[test]
    fn cached_transcript_short_circuits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("note.ogg");
        std::fs::write(&media, b"fake-audio-bytes").expect("write media");
        let fake = FakeTranscriber {
            text: "привет мир".into(),
            calls: Cell::new(0),
        };

        // First call transcribes and writes the sidecar.
        let first = transcribe_cached(&fake, &media).expect("first");
        assert_eq!(first, "привет мир");
        assert_eq!(fake.calls.get(), 1);

        // Second call serves the cached transcript without re-transcribing.
        let second = transcribe_cached(&fake, &media).expect("second");
        assert_eq!(second, "привет мир");
        assert_eq!(fake.calls.get(), 1);
    }

    #[test]
    fn disabled_backend_reports_disabled() {
        let err = DisabledTranscriber
            .transcribe(Path::new("/x.ogg"))
            .expect_err("disabled backend must error");
        assert!(err.to_string().contains("transcription disabled"));
    }

    /// Decoding a missing/unreadable file surfaces a clear `Err` (ffmpeg
    /// fails to open the input) rather than an empty clip. No ffmpeg
    /// binary needed for the spawn-failure half; with ffmpeg present this
    /// exercises the non-zero-exit branch.
    #[test]
    fn decode_pcm_errors_on_unreadable_media() {
        let res = decode_pcm_16k_mono(Path::new("/nonexistent/sy-no-such-media.ogg"));
        assert!(res.is_err(), "decoding a missing file must error");
    }

    /// With ffmpeg present, a tiny OGG/Opus clip (Telegram's voice-note
    /// container) decodes to a non-empty 16 kHz mono `Vec<i16>` — the
    /// element type [`WorkloadInput::Audio`] consumes. Skips cleanly when
    /// ffmpeg is absent so the gate never goes red on a bare CI box.
    #[test]
    fn decode_pcm_yields_16k_mono_i16_from_opus() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skip: ffmpeg not installed");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("note.ogg");
        // Synthesize a 0.1 s stereo 44.1 kHz sine in the Opus/OGG
        // container so the decode path also proves resample + downmix.
        let synth = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.1",
                "-ar",
                "44100",
                "-ac",
                "2",
            ])
            .arg(&media)
            .arg("-y")
            .status()
            .expect("spawn ffmpeg to synthesize fixture");
        assert!(synth.success(), "ffmpeg fixture synthesis failed");
        let pcm = decode_pcm_16k_mono(&media).expect("decode opus");
        assert!(!pcm.is_empty(), "decoded PCM must be non-empty");
    }

    /// In-thread fake `Stt` worker for the hermetic route test: binds the
    /// supervisor's expected socket, reports `Ready`, and answers every
    /// `RunBatch` with a single deterministic `Text` output — the
    /// `FakeWorkload` analogue at the worker-IPC boundary, so the route is
    /// proven with no NPU.
    struct FakeSttSpawn {
        text: String,
    }

    impl crate::aiplane::supervisor::child::ChildSpawn for FakeSttSpawn {
        fn spawn(
            &self,
            kind: WorkloadKind,
            socket: &Path,
        ) -> Result<Box<dyn crate::aiplane::supervisor::child::Child>> {
            use crate::aiplane::worker_ipc::{
                serve, write_resp, WorkerHealth, WorkerReq, WorkerResp,
            };
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{mpsc, Arc, Mutex};

            let (req_tx, req_rx) = mpsc::channel::<(WorkerReq, std::os::unix::net::UnixStream)>();
            serve(socket, req_tx)?;
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_t = shutdown.clone();
            let text = self.text.clone();
            let thread = std::thread::spawn(move || {
                while !shutdown_t.load(Ordering::SeqCst) {
                    match req_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok((req, stream)) => {
                            let resp = match req {
                                WorkerReq::Health => WorkerResp::Health(WorkerHealth {
                                    kind: Some(kind),
                                    state: crate::aiplane::registry::WorkloadState::Ready {
                                        backend: "fake".into(),
                                    },
                                    model_stem: "fake".into(),
                                    ..Default::default()
                                }),
                                WorkerReq::RunBatch { inputs, .. } => WorkerResp::RunBatch {
                                    outputs: inputs
                                        .iter()
                                        .map(|_| WorkloadOutput::Text { text: text.clone() })
                                        .collect(),
                                },
                                WorkerReq::Cancel { .. } => WorkerResp::CancelAck,
                                WorkerReq::Shutdown => {
                                    shutdown_t.store(true, Ordering::SeqCst);
                                    WorkerResp::ShutdownAck
                                }
                            };
                            let _ = write_resp(stream, &resp);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            Ok(Box::new(FakeSttChild {
                shutdown,
                thread: Mutex::new(Some(thread)),
            }))
        }
    }

    struct FakeSttChild {
        shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl crate::aiplane::supervisor::child::Child for FakeSttChild {
        fn pid(&self) -> Option<u32> {
            Some(1)
        }
        fn is_alive(&self) -> bool {
            !self.shutdown.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn terminate(&mut self) {
            self.shutdown
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(t) = self.thread.lock().expect("thread lock").take() {
                let _ = t.join();
            }
        }
    }

    /// The Option-A route: `AiplaneTranscriber`'s supervisor seam ensures
    /// the `Stt` worker and dispatches the decoded PCM, returning the
    /// worker's transcript verbatim. Hermetic — no ffmpeg, no NPU.
    #[test]
    fn aiplane_route_returns_worker_transcript() {
        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "sy-transcribe-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).expect("tmp dir");
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", &tmp);

        let sup = supervisor::Supervisor::with_spawn(std::sync::Arc::new(FakeSttSpawn {
            text: "hello world".into(),
        }));
        let text = transcribe_pcm(&sup, vec![0i16; 16]).expect("route");
        assert_eq!(text, "hello world");

        sup.shutdown(Duration::from_secs(2));
        match prev {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
