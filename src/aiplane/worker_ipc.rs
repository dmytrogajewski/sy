//! Supervisor ↔ worker Unix-socket protocol.
//!
//! Internal-only: not exposed to CLI or MCP. The aiplane daemon
//! spawns one worker child per NPU workload (see `aiplane::worker`
//! and `aiplane::supervisor`); this module is the wire between them.
//!
//! Why a separate IPC module from `aiplane::ipc`:
//!   - Different shape: req/resp only, no fire-and-forget `Op` stream.
//!   - Different surface: clients are the daemon itself, not user
//!     tooling, so we don't carry the legacy `Embed`/`Search`
//!     adapter variants — batched RunBatch is the only inference
//!     verb.
//!   - Different socket path: workers bind
//!     `sy-aiplane-worker-<kind>.sock`, separated from the public
//!     daemon socket so accidental `nc -U` against the public
//!     surface can't route to a worker.
//!
//! Frame: one JSON line per request, one JSON line per response.

use std::{
    env,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::registry::{WorkloadInput, WorkloadKind, WorkloadOutput, WorkloadState};

/// Request from the supervisor to a worker child.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "kebab-case")]
pub enum WorkerReq {
    /// Health probe. Cheap; the worker answers from its in-memory
    /// `WorkerHealth` snapshot, never touches the model. The
    /// supervisor polls this every second to drive the daemon
    /// status snapshot and trigger restart on consecutive failures.
    Health,
    /// Batched inference. `inputs` carries one or more `WorkloadInput`
    /// values whose variant must match the worker's kind (validated
    /// by the worker). Returned in order; partial-batch errors fail
    /// the whole call rather than dropping individual rows.
    RunBatch { inputs: Vec<WorkloadInput> },
    /// Cooperative shutdown. The worker flushes any in-flight call,
    /// drops its ORT session, replies `ShutdownAck`, and exits 0.
    Shutdown,
}

/// Response from a worker child to the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "kebab-case")]
pub enum WorkerResp {
    Health(WorkerHealth),
    RunBatch {
        outputs: Vec<WorkloadOutput>,
    },
    /// The request was malformed, the workload isn't ready, or the
    /// inference threw. `msg` is a human-readable chain; the
    /// supervisor turns it into the outer `Resp::Error` shown to
    /// the user (or, for indexing, logs and skips the batch).
    Error {
        msg: String,
    },
    ShutdownAck,
}

/// Worker self-report. Mirrors the daemon-wide `WorkloadHealth` but
/// carries the per-process extras (pid, model_stem, loaded_at).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerHealth {
    pub kind: Option<WorkloadKind>,
    pub state: WorkloadState,
    /// On-disk model stem, e.g. `"multilingual-e5-base"`. Empty
    /// until the worker has parsed its argv (very short window).
    pub model_stem: String,
    /// Worker pid (the worker fills its own pid via `process::id()`).
    pub pid: u32,
    /// Wall-clock when the worker reached `Ready`. 0 if never.
    pub ready_at_unix: u64,
    /// Exponential moving average of `RunBatch` latency in ms.
    pub ema_ms: f64,
    pub calls: u64,
    pub errors: u64,
}

/// Worker-socket path for a given workload kind. Deterministic so
/// the supervisor can reconnect after a worker restart without
/// rendezvous via the FS or argv passing.
pub fn socket_path(kind: WorkloadKind) -> PathBuf {
    let base = if let Ok(d) = env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            PathBuf::from(d)
        } else {
            uid_fallback()
        }
    } else {
        uid_fallback()
    };
    base.join(format!("sy-aiplane-worker-{}.sock", kind.as_str()))
}

fn uid_fallback() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}"))
}

#[derive(Debug)]
pub enum WorkerIpcError {
    /// No socket / refused connection. Caller treats this as
    /// "worker not running yet"; the supervisor's restart loop will
    /// re-spawn.
    WorkerDown,
    /// Wire-level failure after the connection was established.
    Wire(anyhow::Error),
}

impl std::fmt::Display for WorkerIpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerIpcError::WorkerDown => write!(f, "sy-aiplane worker not reachable"),
            WorkerIpcError::Wire(e) => write!(f, "worker ipc: {e}"),
        }
    }
}

impl std::error::Error for WorkerIpcError {}

/// Synchronous request to a worker. `read_timeout` is configurable
/// because Health is sub-second but RunBatch (especially the first
/// after a model load) can be ~10 s for a (B=32, seq=512) rerank.
pub fn request(
    socket: &Path,
    req: &WorkerReq,
    read_timeout: Duration,
) -> std::result::Result<WorkerResp, WorkerIpcError> {
    let stream = UnixStream::connect(socket).map_err(|_| WorkerIpcError::WorkerDown)?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_read_timeout(Some(read_timeout));

    let mut writer = stream
        .try_clone()
        .map_err(|e| WorkerIpcError::Wire(e.into()))?;
    let line = serde_json::to_string(req).map_err(|e| WorkerIpcError::Wire(e.into()))?;
    writer
        .write_all(line.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .map_err(|e| WorkerIpcError::Wire(e.into()))?;
    let _ = writer.shutdown(std::net::Shutdown::Write);

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader
        .read_line(&mut buf)
        .map_err(|e| WorkerIpcError::Wire(e.into()))?;
    if buf.trim().is_empty() {
        return Err(WorkerIpcError::Wire(anyhow::anyhow!(
            "worker closed connection without responding"
        )));
    }
    serde_json::from_str::<WorkerResp>(buf.trim()).map_err(|e| WorkerIpcError::Wire(e.into()))
}

/// Bind a worker socket and forward incoming requests on `req_tx`.
/// The receiver owns the stream, writes the `WorkerResp` JSON line
/// on it, and drops to close. Pre-existing socket files are
/// unlinked (workers are the sole owner of their path; if a stale
/// one exists, the previous worker died without cleanup).
pub fn serve(socket: &Path, req_tx: mpsc::Sender<(WorkerReq, UnixStream)>) -> Result<()> {
    if socket.exists() {
        let _ = std::fs::remove_file(socket);
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("bind worker socket {}", socket.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let req_tx = req_tx.clone();
            thread::spawn(move || handle_conn(stream, req_tx));
        }
    });
    Ok(())
}

fn handle_conn(stream: UnixStream, req_tx: mpsc::Sender<(WorkerReq, UnixStream)>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let Ok(reader_fd) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_fd);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let Ok(req) = serde_json::from_str::<WorkerReq>(trimmed) else {
        // Malformed → write an Error line and drop.
        let _ = write_resp(
            stream,
            &WorkerResp::Error {
                msg: format!("malformed request: {trimmed}"),
            },
        );
        return;
    };
    let _ = req_tx.send((req, stream));
}

/// Write a `WorkerResp` on a stream and close. Used by the worker's
/// req handler to reply.
pub fn write_resp(mut stream: UnixStream, resp: &WorkerResp) -> Result<()> {
    let line = serde_json::to_string(resp)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_req_health_roundtrip() {
        let r = WorkerReq::Health;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"req":"health"}"#);
        let back: WorkerReq = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, WorkerReq::Health));
    }

    #[test]
    fn worker_req_run_batch_roundtrip() {
        let r = WorkerReq::RunBatch {
            inputs: vec![WorkloadInput::TextPair {
                a: "q".into(),
                b: "d".into(),
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: WorkerReq = serde_json::from_str(&s).unwrap();
        match back {
            WorkerReq::RunBatch { inputs } => {
                assert_eq!(inputs.len(), 1);
                match &inputs[0] {
                    WorkloadInput::TextPair { a, b } => {
                        assert_eq!(a, "q");
                        assert_eq!(b, "d");
                    }
                    _ => panic!("wrong input"),
                }
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn worker_health_carries_state_with_backend() {
        let h = WorkerHealth {
            kind: Some(WorkloadKind::Rerank),
            state: WorkloadState::Ready {
                backend: "vitisai".into(),
            },
            model_stem: "bge-reranker-v2-m3".into(),
            pid: 4242,
            ready_at_unix: 1_700_000_000,
            ema_ms: 32.7,
            calls: 17,
            errors: 0,
        };
        let s = serde_json::to_string(&h).unwrap();
        assert!(s.contains("\"state\":\"ready\""));
        assert!(s.contains("bge-reranker-v2-m3"));
        let back: WorkerHealth = serde_json::from_str(&s).unwrap();
        assert_eq!(back.pid, 4242);
        assert!(back.state.is_ready());
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir_when_set() {
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", "/tmp/sy-worker-test");
        let p = socket_path(WorkloadKind::Rerank);
        assert_eq!(
            p,
            PathBuf::from("/tmp/sy-worker-test/sy-aiplane-worker-rerank.sock")
        );
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    #[test]
    fn worker_resp_error_roundtrip() {
        let r = WorkerResp::Error {
            msg: "model not prepared".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: WorkerResp = serde_json::from_str(&s).unwrap();
        match back {
            WorkerResp::Error { msg } => assert_eq!(msg, "model not prepared"),
            _ => panic!("wrong variant"),
        }
    }

    /// End-to-end: bind a worker socket, run a fake handler that
    /// answers Health, request from the client side, verify shape.
    /// Hermetic — uses a temp XDG_RUNTIME_DIR.
    #[test]
    fn worker_ipc_roundtrip_health() {
        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let unique = format!(
            "sy-worker-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let tmp = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&tmp).unwrap();
        let socket = tmp.join("sy-aiplane-worker-test.sock");

        let (tx, rx) = mpsc::channel::<(WorkerReq, UnixStream)>();
        serve(&socket, tx).expect("serve");

        // Fake handler: answer Health with a canned WorkerHealth.
        thread::spawn(move || {
            while let Ok((req, stream)) = rx.recv() {
                let resp = match req {
                    WorkerReq::Health => WorkerResp::Health(WorkerHealth {
                        kind: Some(WorkloadKind::Embed),
                        state: WorkloadState::Ready {
                            backend: "test".into(),
                        },
                        model_stem: "fake".into(),
                        pid: 1,
                        ready_at_unix: 1,
                        ema_ms: 0.0,
                        calls: 0,
                        errors: 0,
                    }),
                    _ => WorkerResp::Error {
                        msg: "unexpected".into(),
                    },
                };
                let _ = write_resp(stream, &resp);
            }
        });

        let resp = request(&socket, &WorkerReq::Health, Duration::from_secs(2)).expect("request");
        match resp {
            WorkerResp::Health(h) => {
                assert_eq!(h.kind, Some(WorkloadKind::Embed));
                assert!(h.state.is_ready());
                assert_eq!(h.model_stem, "fake");
            }
            _ => panic!("expected Health"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
