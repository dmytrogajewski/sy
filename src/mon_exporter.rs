//! sy-mon Step 20 — shared runtime-thread wrapper around
//! `sy_core::obs::mon_exporter::install` for every plane that doesn't
//! already own a tokio runtime its mon-exporter can ride on.
//!
//! The shared installer in `sy_core::obs::mon_exporter` requires an
//! active tokio runtime at call time — its accept loop is
//! `tokio::spawn`-ed onto the current runtime. Planes that already
//! drive a tokio runtime (agt, power, mon-collect) call
//! `sy_core::obs::mon_exporter::install(path)` directly from inside
//! their runtime's `block_on`. Planes whose main loop is synchronous
//! (knowledge mpsc loop) or owned by a foreign reactor (iced for the
//! stack bar) spawn a dedicated runtime thread via this module's
//! [`PlaneMonExporter`] so the install lands on a runtime that
//! outlives the call site.
//!
//! ## Path
//!
//! `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock`, per SPEC §3 SCOPE
//! item 1. The shared installer creates the parent dir at 0700 if
//! missing; this wrapper tightens the socket file itself to 0600
//! once [`spawn`] returns, per SPEC §4 Security non-functional.
//!
//! ## Shutdown
//!
//! Dropping the [`PlaneMonExporter`] guard signals the runtime thread
//! to release its `UdsGuard`, which aborts the exporter's accept task
//! and unlinks the socket file before the daemon exits. The runtime
//! thread is then joined within a short budget; if it doesn't exit
//! in time the daemon proceeds anyway — the OS will reclaim the
//! runtime's resources on process exit.
//!
//! Step 10 introduced this wrapper for aiplane only as
//! `aiplane::mon_exporter`; Step 20 generalises it so the knowledge
//! and stack-bar planes can reuse the same plumbing without copying
//! 130 LoC. The aiplane module now delegates here; the public
//! `AiplaneMonExporter` symbol is preserved as a type alias for
//! source compatibility.

#![cfg(feature = "mon-exporter")]

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Mode 0600 on the bound socket file. User-private — every plane UDS
/// in `$XDG_RUNTIME_DIR/sy/*` follows this convention.
const SOCKET_MODE: u32 = 0o600;

/// Budget for the runtime thread to release its guard + unlink the
/// socket on shutdown. The shared installer's Drop path is synchronous
/// (`std::fs::remove_file`) so this is generous; the daemon exits
/// either way after the budget elapses.
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// RAII handle for a single plane's mon-exporter runtime thread.
/// Dropping the guard signals the dedicated runtime thread to release
/// the shared `UdsGuard`, which aborts the exporter's accept task and
/// unlinks the socket file before the daemon exits.
#[must_use = "the plane metrics socket is unbound when the guard is dropped"]
pub struct PlaneMonExporter {
    /// `Sender<()>` paired with the runtime-thread's `Receiver<()>`.
    /// The runtime thread parks on `recv()`; we drop the sender to
    /// wake it. Wrapped in `Option` so `Drop` can take ownership.
    shutdown_tx: Option<mpsc::Sender<()>>,
    /// Joined on Drop with a small budget. Wrapped in `Option` so
    /// `Drop` can take ownership without leaving an uninhabited
    /// field.
    thread: Option<JoinHandle<()>>,
    /// The bound socket path — exposed for logging and doctor checks.
    path: PathBuf,
}

impl PlaneMonExporter {
    /// The path the exporter is bound at.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for PlaneMonExporter {
    fn drop(&mut self) {
        self.shutdown_tx.take();
        if let Some(handle) = self.thread.take() {
            // `JoinHandle` doesn't expose a timed join; poll
            // `is_finished()` until the budget elapses. The shared
            // installer's Drop is fast (sync `remove_file`) so this
            // loop typically exits on the first iteration.
            let start = std::time::Instant::now();
            while !handle.is_finished() && start.elapsed() < SHUTDOWN_JOIN_TIMEOUT {
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
            // If the thread didn't finish in time we let it leak — the
            // OS reclaims everything on process exit. Logging is the
            // caller's job (Drop has no `tracing` context).
        }
    }
}

/// Bring up the named plane's Prometheus UDS exporter at
/// `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock`.
///
/// Spawns a dedicated tokio runtime thread that owns the install's
/// `UdsGuard` for the returned handle's lifetime. The call returns
/// once the socket is bound (or the install errors); the caller's
/// thread does not need to host a tokio runtime.
pub fn spawn(plane: &'static str) -> Result<PlaneMonExporter> {
    let path = socket_path_for(plane)?;
    spawn_at(plane, path)
}

/// Variant of [`spawn`] that accepts an explicit path. Used by the
/// daemon when an override env / CLI flag is in scope, and by tests
/// that bind under a tempdir.
pub fn spawn_at(plane: &'static str, path: PathBuf) -> Result<PlaneMonExporter> {
    // Readiness channel: the runtime thread sends a `Result` once
    // `install()` has returned (success or failure). `sync_channel(0)`
    // is a rendezvous — the thread blocks until we `recv()` here.
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<()>>(0);
    // Shutdown channel: dropped on `PlaneMonExporter::drop`. The
    // runtime thread parks on `recv()`; the recv returns `Err(_)`
    // when we drop the sender on the main side.
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    let path_for_thread = path.clone();
    let thread_name = format!("sy-{plane}-mon-exporter");
    let thread = thread::Builder::new()
        .name(thread_name)
        .spawn(move || run_exporter_thread(plane, path_for_thread, ready_tx, shutdown_rx))
        .with_context(|| format!("spawn {plane} mon-exporter thread"))?;

    // Wait for the runtime thread to report readiness. The first
    // `recv()` is the install result; on success the socket is bound
    // and the chmod has been applied.
    let ready = ready_rx
        .recv()
        .with_context(|| format!("{plane} mon-exporter thread exited before readiness"))?;
    ready?;

    Ok(PlaneMonExporter {
        shutdown_tx: Some(shutdown_tx),
        thread: Some(thread),
        path,
    })
}

/// Body of the dedicated runtime thread: build a single-worker tokio
/// runtime, call the shared `install` inside its context, chmod the
/// socket to 0600, signal readiness, then park until the shutdown
/// channel closes. Dropping the runtime aborts the install guard,
/// which unlinks the socket.
fn run_exporter_thread(
    plane: &'static str,
    path: PathBuf,
    ready_tx: mpsc::SyncSender<Result<()>>,
    shutdown_rx: mpsc::Receiver<()>,
) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name(format!("sy-{plane}-mon-exporter-rt"))
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready_tx.send(Err(
                anyhow!(e).context(format!("build {plane} mon-exporter runtime"))
            ));
            return;
        }
    };

    // `mon_exporter::install` is synchronous but requires an active
    // tokio runtime to `tokio::spawn` its accept task. Enter the
    // runtime so the spawn inside `install` lands on it.
    let _enter = rt.enter();
    let guard = match sy_core::obs::mon_exporter::install(path.clone()) {
        Ok(g) => g,
        Err(e) => {
            let _ = ready_tx.send(Err(
                anyhow!(e.to_string()).context(format!("install {plane} mon-exporter"))
            ));
            return;
        }
    };
    drop(_enter);

    // Tighten the socket file to 0600 (SPEC §4 Security). Best-effort:
    // the shared installer doesn't enforce a mode and a non-fatal
    // chmod failure shouldn't kill the daemon.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut perms = meta.permissions();
            perms.set_mode(SOCKET_MODE);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }

    // Signal readiness — the daemon's main thread is blocked on this
    // recv and can resume once the socket is bound + chmod-ed.
    if ready_tx.send(Ok(())).is_err() {
        drop(guard);
        return;
    }

    // Park until the daemon drops its `PlaneMonExporter` (which drops
    // the matching `Sender`, making this recv return `Err`).
    let _ = shutdown_rx.recv();

    // Dropping the guard aborts the accept task and unlinks the socket
    // file. Dropping the runtime afterwards waits for spawned tasks to
    // finish (the accept task has just been aborted).
    drop(guard);
    drop(rt);
}

/// `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock`. Returns an error when
/// `XDG_RUNTIME_DIR` is unset or empty — a user-session daemon
/// without a runtime dir is mis-launched, and silently falling back
/// to `/tmp` would risk crossing the user-boundary the SPEC §4
/// Security non-functional pins down.
pub fn socket_path_for(plane: &str) -> Result<PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .with_context(|| format!("read XDG_RUNTIME_DIR for {plane} metrics socket"))?;
    if base.is_empty() {
        return Err(anyhow!(
            "XDG_RUNTIME_DIR is empty; refusing to bind {plane} metrics socket"
        ));
    }
    Ok(PathBuf::from(base)
        .join("sy")
        .join(plane)
        .join("metrics.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `socket_path_for(plane)` must derive
    /// `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock` for every plane —
    /// the SPEC §3 SCOPE item one path layout.
    #[test]
    fn socket_path_derives_xdg_runtime_layout() {
        let _env_lock = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp/sy-mon-exporter-test-xdg");
        let got = socket_path_for("knowledge").expect("socket_path_for");
        if let Some(v) = prev {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        assert_eq!(
            got,
            std::path::PathBuf::from("/tmp/sy-mon-exporter-test-xdg/sy/knowledge/metrics.sock"),
        );
    }
}
