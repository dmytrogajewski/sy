//! Unix-socket IPC: CLI → daemon for refresh/index/sync triggers.
//!
//! Two flavours share the same socket:
//!
//!   * Fire-and-forget `Op` (existing): write a JSON line, close. The
//!     daemon owns the work; the client never sees a response. Used
//!     for IndexNow, FullResync, Pause, etc.
//!   * Request-response `Req`/`Resp` (new, May 2026): write a JSON
//!     line, then read one back. Used for Search/Embed so the CLI and
//!     MCP server can offload all embedding to the daemon — the only
//!     process with the NPU bound — instead of spinning up their own
//!     ORT session (which fights the daemon for /dev/accel/accel0 and
//!     silently downgrades to CUDA / CPU).
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
    /// Embed a single string. Returns the L2-normalised vector that
    /// the daemon's embedder produced.
    Embed { text: String },
    /// Top-k semantic search. `prefix` is the already-resolved
    /// absolute file-path prefix (caller does `sources::expand`).
    Search {
        query: String,
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
}

/// Response written back on the same UnixStream by the daemon's Req
/// worker. Stays decoupled from `qdrant::SearchHit` so the wire shape
/// can evolve without dragging qdrant internals across the boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "kebab-case")]
pub enum Resp {
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
            IpcError::DaemonDown => write!(f, "sy-knowledge daemon not reachable"),
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
    let uid = unsafe { libc_getuid() };
    PathBuf::from(format!("/run/user/{uid}/sy-knowledge.sock"))
}

extern "C" {
    fn getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    getuid()
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
/// 10 s timeout because the first request after the daemon has
/// idled out of D3 takes a beat to wake the NPU (typically < 1 s,
/// but the kernel's runtime PM can be lumpy under load).
pub fn request(req: &Req) -> std::result::Result<Resp, IpcError> {
    let p = socket_path();
    let stream = UnixStream::connect(&p).map_err(|_| IpcError::DaemonDown)?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));

    // Write the request line on the original handle.
    let mut writer = stream.try_clone().map_err(|e| IpcError::Wire(e.into()))?;
    let line = serde_json::to_string(req).map_err(|e| IpcError::Wire(e.into()))?;
    writer
        .write_all(line.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .map_err(|e| IpcError::Wire(e.into()))?;
    // Half-close the write side so the daemon's BufReader sees EOF
    // on the *write* end specifically (it keeps the read end open
    // for our response). UnixStream doesn't expose shutdown(WR)
    // directly; std::net::Shutdown is what we want.
    let _ = writer.shutdown(std::net::Shutdown::Write);

    // Read one JSON line back.
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
            // Each connection on its own thread so a slow Req
            // (NPU-bound) doesn't head-of-line block another
            // fire-and-forget Op landing right behind it.
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
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    // Reader holds its own fd via try_clone so we can hand the
    // original `stream` to the Req worker for the response write.
    let Ok(reader_fd) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_fd);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return, // EOF
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
        // Malformed line → swallow and keep reading; the daemon
        // shouldn't die because someone fat-fingered a netcat
        // probe.
    }
}
