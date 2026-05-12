//! Unix-socket IPC for sy-aiplane: CLI/MCP ↔ daemon.
//!
//! Two flavours share the same socket:
//!
//!   * Fire-and-forget `Op` (existing): write a JSON line, close. The
//!     daemon owns the work; the client never sees a response. Used
//!     for IndexNow, FullResync, Pause, etc.
//!   * Request-response `Req`/`Resp`: write a JSON line, then read
//!     one back. Used for Embed / Search / generic `Run` so the CLI
//!     and MCP server can offload all NPU inference to the daemon —
//!     the only process with the device bound — instead of spinning
//!     up their own ORT session.
//!
//! Missing socket = daemon not running; `send()` is a silent no-op,
//! `request()` returns `IpcError::DaemonDown` so the caller can fall
//! back to in-process embedding.

use std::{
    env,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::registry::{WorkloadInput, WorkloadKind, WorkloadOutput};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// Re-read sy.toml [knowledge] sources + schedule.
    RefreshSources,
    /// Run an incremental index pass right now.
    IndexNow,
    /// Drop the qdrant collection and re-embed everything.
    FullResync,
    /// Re-read schedule from sy.toml (subset of RefreshSources).
    ReloadSchedule,
    /// Re-walk discover roots + shallow-home for `qdr.toml` manifests; diff
    /// the active set, register/unregister watchers + qdrant points.
    RescanDiscovery,
    /// Stop firing scheduled / FS-tickle / IPC-IndexNow passes until
    /// `Resume`. User-driven `sy knowledge index/sync/search` calls
    /// (which run in the CLI process, not the daemon) bypass this.
    Pause,
    /// Resume from a paused state. Triggers a single catch-up pass.
    Resume,
    /// Idempotent toggle (used by the waybar middle-click handler).
    TogglePause,
    /// Cooperatively cancel any in-flight pass. Daemon stays paused if it
    /// was paused before. Files already embedded keep their qdrant points.
    Cancel,
    /// Graceful shutdown.
    Shutdown,
}

/// Request-response op. Distinct from `Op` because callers wait for
/// the daemon's reply on the same UnixStream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "kebab-case")]
pub enum Req {
    /// Generic workload dispatch. The daemon validates the input
    /// variant matches the workload's expected shape and returns
    /// `Resp::Run { output }` or `Resp::Error`.
    Run {
        workload: WorkloadKind,
        input: WorkloadInput,
    },
    /// Composite: embed `query` via the Embed workload, then
    /// qdrant top-k cosine search. Kept as a single round-trip so
    /// search consumers don't pay 2× IPC.
    Search {
        query: String,
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    /// Legacy adapter: pre-aiplane CLIs sent `Req::Embed { text }`.
    /// The daemon translates this to `Run { Embed, Text(text) }`
    /// internally so older `sy` binaries keep working through the
    /// upgrade window.
    Embed { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "kebab-case")]
pub enum Resp {
    Run { output: WorkloadOutput },
    /// Legacy: kept so the old `Req::Embed` adapter has a
    /// dedicated response shape that maps 1:1 to what the previous
    /// CLI version expected.
    Embed { vector: Vec<f32> },
    Search { hits: Vec<HitRow> },
    Error { msg: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HitRow {
    pub score: f32,
    pub file_path: String,
    pub chunk_index: u32,
    pub chunk_text: String,
}

#[derive(Debug)]
pub enum IpcError {
    /// `connect()` failed — no socket, refused, or removed. Callers
    /// translate this into an in-process fallback.
    DaemonDown,
    /// Wire-level failure (read/write/serde) after the connection was
    /// established. Carries the underlying anyhow chain.
    Wire(anyhow::Error),
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpcError::DaemonDown => write!(f, "sy-aiplane daemon not reachable"),
            IpcError::Wire(e) => write!(f, "ipc: {e}"),
        }
    }
}

impl std::error::Error for IpcError {}

pub fn socket_path() -> PathBuf {
    if let Ok(d) = env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("sy-knowledge.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/sy-knowledge.sock"))
}

/// Fire-and-forget op. Silently succeeds if the daemon isn't listening.
pub fn send(op: &Op) -> Result<()> {
    let p = socket_path();
    let mut stream = match UnixStream::connect(&p) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let line = serde_json::to_string(op)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}

/// Synchronous request-response. Connects, writes one JSON line,
/// reads one back. Errors disambiguate "no daemon" (so callers can
/// fall back) from "wire failure" (which they should surface).
///
/// 30 s timeout: first request after the daemon has idled out of D3
/// takes a beat to wake the NPU + a few hundred ms compile cache
/// hit; under load + a long input (STT of a multi-second audio)
/// the upper bound is real.
pub fn request(req: &Req) -> std::result::Result<Resp, IpcError> {
    let p = socket_path();
    let stream = UnixStream::connect(&p).map_err(|_| IpcError::DaemonDown)?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));

    let mut writer = stream.try_clone().map_err(|e| IpcError::Wire(e.into()))?;
    let line = serde_json::to_string(req).map_err(|e| IpcError::Wire(e.into()))?;
    writer
        .write_all(line.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .map_err(|e| IpcError::Wire(e.into()))?;
    // Half-close the write side so the daemon's BufReader sees EOF on
    // the write end specifically; the read end stays open for the
    // response.
    let _ = writer.shutdown(std::net::Shutdown::Write);

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader
        .read_line(&mut buf)
        .map_err(|e| IpcError::Wire(e.into()))?;
    if buf.trim().is_empty() {
        return Err(IpcError::Wire(anyhow::anyhow!(
            "daemon closed connection without responding"
        )));
    }
    serde_json::from_str::<Resp>(buf.trim()).map_err(|e| IpcError::Wire(e.into()))
}

/// Bind the listener and dispatch incoming JSON lines:
///
///   * `Op` (fire-and-forget) → forwarded on `ops_tx`. Connection
///     can carry multiple ops and is read until EOF.
///   * `Req` (request-response) → forwarded on `req_tx` as
///     `(Req, UnixStream)`. The receiver owns the stream, writes
///     the `Resp` JSON line on it, and drops to close.
pub fn serve(
    ops_tx: mpsc::Sender<Op>,
    req_tx: mpsc::Sender<(Req, UnixStream)>,
) -> Result<()> {
    let p = socket_path();
    if p.exists() {
        let _ = std::fs::remove_file(&p);
    }
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&p).with_context(|| format!("bind {}", p.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let ops_tx = ops_tx.clone();
            let req_tx = req_tx.clone();
            thread::spawn(move || handle_conn(stream, ops_tx, req_tx));
        }
    });
    Ok(())
}

fn handle_conn(
    stream: UnixStream,
    ops_tx: mpsc::Sender<Op>,
    req_tx: mpsc::Sender<(Req, UnixStream)>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let Ok(reader_fd) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_fd);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(op) = serde_json::from_str::<Op>(trimmed) {
            let _ = ops_tx.send(op);
            continue;
        }
        if let Ok(req) = serde_json::from_str::<Req>(trimmed) {
            // Hand off the *original* stream so the worker can
            // write the response on the still-open write half.
            let _ = req_tx.send((req, stream));
            return;
        }
        // Malformed line → swallow and keep reading.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_roundtrip_all_variants() {
        for op in [
            Op::RefreshSources,
            Op::IndexNow,
            Op::FullResync,
            Op::ReloadSchedule,
            Op::RescanDiscovery,
            Op::Pause,
            Op::Resume,
            Op::TogglePause,
            Op::Cancel,
            Op::Shutdown,
        ] {
            let s = serde_json::to_string(&op).unwrap();
            let back: Op = serde_json::from_str(&s).unwrap();
            // Discriminant equality via Debug — Op doesn't derive PartialEq.
            assert_eq!(format!("{op:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn req_run_roundtrip() {
        let r = Req::Run {
            workload: WorkloadKind::Embed,
            input: WorkloadInput::Text {
                text: "hello".into(),
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Req = serde_json::from_str(&s).unwrap();
        match back {
            Req::Run { workload, input } => {
                assert_eq!(workload, WorkloadKind::Embed);
                match input {
                    WorkloadInput::Text { text } => assert_eq!(text, "hello"),
                    _ => panic!("wrong input variant"),
                }
            }
            _ => panic!("wrong req variant"),
        }
    }

    #[test]
    fn req_search_with_prefix_omits_when_none() {
        let r = Req::Search {
            query: "q".into(),
            limit: 3,
            prefix: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("prefix"), "prefix=None must be omitted");
    }

    #[test]
    fn legacy_embed_req_still_parses() {
        let s = r#"{"req":"embed","text":"привет"}"#;
        let r: Req = serde_json::from_str(s).unwrap();
        match r {
            Req::Embed { text } => assert_eq!(text, "привет"),
            _ => panic!("expected Embed variant"),
        }
    }

    #[test]
    fn resp_run_roundtrip_vector() {
        let r = Resp::Run {
            output: WorkloadOutput::Vector {
                vector: vec![0.1, 0.2, 0.3],
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Resp = serde_json::from_str(&s).unwrap();
        match back {
            Resp::Run {
                output: WorkloadOutput::Vector { vector },
            } => assert_eq!(vector, vec![0.1, 0.2, 0.3]),
            _ => panic!("wrong resp shape"),
        }
    }

    #[test]
    fn malformed_request_returns_serde_err_not_panic() {
        let r: Result<Req, _> = serde_json::from_str("not json");
        assert!(r.is_err());
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir_when_set() {
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", "/tmp/sy-test-runtime");
        let p = socket_path();
        assert_eq!(p, PathBuf::from("/tmp/sy-test-runtime/sy-knowledge.sock"));
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }
}
