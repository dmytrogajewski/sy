//! Unix-socket IPC: CLI → daemon for refresh/index/sync triggers.
//! Fire-and-forget — missing socket = daemon not running, silently no-op.

use std::{
    env,
    io::Write,
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

/// Bind the listener and stream incoming Op messages onto `tx`.
pub fn serve(tx: mpsc::Sender<Op>) -> Result<()> {
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
            let Ok(mut s) = conn else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
            use std::io::Read;
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            for line in buf.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(op) = serde_json::from_str::<Op>(line) {
                    let _ = tx.send(op);
                }
            }
        }
    });
    Ok(())
}
