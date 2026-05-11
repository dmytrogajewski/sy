//! Unix-socket IPC between `sy stack <cli>` and the `sy stack bar` daemon.
//!
//! Wire format: one JSON object per line. Client→daemon ops are
//! fire-and-forget; the daemon writes nothing back (CLI commands work
//! without the bar running — missing socket = silent no-op).

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
    /// Reload items.json from disk and repaint.
    Refresh,
    /// Hide if visible, show if hidden.
    Toggle,
    /// Re-read theme (sy.toml + themes/<name>.toml) and repaint.
    ReloadTheme,
}

pub fn socket_path() -> PathBuf {
    if let Ok(d) = env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            let dir = PathBuf::from(d).join("sy");
            let _ = std::fs::create_dir_all(&dir);
            return dir.join("stackbar.sock");
        }
    }
    let uid = unsafe { libc_getuid() };
    let dir = PathBuf::from(format!("/run/user/{uid}/sy"));
    let _ = std::fs::create_dir_all(&dir);
    dir.join("stackbar.sock")
}

extern "C" {
    fn getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    getuid()
}

/// Send an op to the bar daemon. Silently succeeds if the daemon is not
/// running (CLI commands must work standalone).
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

/// Listen for ops on the socket. Drops messages into the supplied mpsc
/// sender; the bar's main loop polls the receiver each tick.
#[allow(dead_code)]
pub fn serve(tx: mpsc::Sender<Op>) -> Result<()> {
    let p = socket_path();
    if p.exists() {
        let _ = std::fs::remove_file(&p);
    }
    let listener = UnixListener::bind(&p)
        .with_context(|| format!("bind {}", p.display()))?;
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
