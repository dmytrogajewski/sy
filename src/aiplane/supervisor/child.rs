//! Child-process handle abstraction. Real production spawns
//! `sy aiplane worker ...` via `std::process::Command`; tests
//! substitute in-thread fakes that bind the worker socket.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
};

use anyhow::{Context, Result};

use super::super::registry::WorkloadKind;

/// Behaviour the supervisor needs from a child handle. Implemented by
/// `RealChild` (a `std::process::Child`) in production and by test
/// doubles that wrap a thread.
pub trait Child: Send {
    fn pid(&self) -> Option<u32>;
    /// Cheap liveness check. Real impl calls `try_wait()`; fake impl
    /// reads an AtomicBool.
    fn is_alive(&self) -> bool;
    /// Best-effort: SIGTERM the process (real) or set the shutdown
    /// flag (fake). Returns immediately; supervisor decides whether
    /// to wait for confirmation.
    fn terminate(&mut self);
    /// What kind of workload this child hosts. Used by the
    /// supervisor's reap path to log informatively. `None` only if
    /// the spawn failed before kind was known (unreachable in
    /// practice).
    fn handle_kind(&self) -> Option<WorkloadKind>;
}

/// Strategy for spawning a worker child. Real impl uses
/// `std::process::Command`; test impl spawns an in-thread fake.
pub trait ChildSpawn: Send + Sync {
    fn spawn(&self, kind: WorkloadKind, socket: &Path) -> Result<Box<dyn Child>>;
}

/// Production spawn: invokes `<sy_binary> aiplane worker --kind X
/// --socket P` as a child process. Stdout + stderr inherit so
/// daemon journalctl logs carry the worker output too.
pub struct RealSpawn {
    sy_binary: PathBuf,
}

impl RealSpawn {
    pub fn new(sy_binary: PathBuf) -> Self {
        Self { sy_binary }
    }
}

impl ChildSpawn for RealSpawn {
    fn spawn(&self, kind: WorkloadKind, socket: &Path) -> Result<Box<dyn Child>> {
        let mut cmd = std::process::Command::new(&self.sy_binary);
        cmd.arg("aiplane")
            .arg("worker")
            .arg("--kind")
            .arg(kind.as_str())
            .arg("--socket")
            .arg(socket)
            // Inherit env so AMD venv re-exec markers + LD_LIBRARY_PATH
            // + CAP_IPC_LOCK propagate to the child. The supervisor
            // runs *after* `aiplane::reexec::maybe_reexec_with_amd_env`
            // has already re-exec'd, so the child inherits the AMD
            // venv env automatically.
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let proc = cmd
            .spawn()
            .with_context(|| format!("spawn sy aiplane worker --kind {kind}"))?;
        Ok(Box::new(RealChild {
            kind,
            proc: Mutex::new(Some(proc)),
        }))
    }
}

struct RealChild {
    kind: WorkloadKind,
    proc: Mutex<Option<std::process::Child>>,
}

impl Child for RealChild {
    fn pid(&self) -> Option<u32> {
        self.proc.lock().ok()?.as_ref().map(|c| c.id())
    }

    fn is_alive(&self) -> bool {
        let mut guard = match self.proc.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(c) = guard.as_mut() else {
            return false;
        };
        match c.try_wait() {
            Ok(None) => true,
            Ok(Some(_status)) => {
                *guard = None;
                false
            }
            Err(_) => false,
        }
    }

    fn terminate(&mut self) {
        let Ok(mut guard) = self.proc.lock() else {
            return;
        };
        let Some(c) = guard.as_mut() else {
            return;
        };
        // SIGTERM first; the worker's signal handler flips its
        // shutdown flag and exits cleanly. If it doesn't honour
        // that within the supervisor's wait window, the supervisor
        // calls `terminate` again which on a `std::process::Child`
        // amounts to `kill()` (SIGKILL).
        let _ = c.kill();
        let _ = c.wait();
        *guard = None;
    }

    fn handle_kind(&self) -> Option<WorkloadKind> {
        Some(self.kind)
    }
}
