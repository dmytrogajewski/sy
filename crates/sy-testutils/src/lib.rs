//! Daemon-in-thread harness for sy workspace tests.
//!
//! Formalises the pattern already exercised by
//! `src/aiplane/ipc.rs::tests::daemon_smoke_*` (commit
//! `9bd8ba5 prep_npu_workload.py + daemon-in-thread integration
//! test`). The flow each test follows:
//!
//! 1. Allocate a hermetic `$XDG_RUNTIME_DIR` so the daemon's
//!    socket can't collide with the live `~/.local/bin/sy` daemon's
//!    `/run/user/$uid/sy-*.sock` listener.
//! 2. Spawn the daemon on a dedicated thread with its own tokio
//!    runtime so the test's `#[tokio::test]` runtime stays
//!    uncontaminated.
//! 3. Drive the daemon through a real IPC client.
//! 4. On test exit, the closure returns, the thread joins, the
//!    tempdir is cleaned, and the previous `XDG_RUNTIME_DIR` (if
//!    any) is restored.
//!
//! ## Process-wide env caveat
//!
//! `XDG_RUNTIME_DIR` is process-wide, not thread-local. Two
//! `IsolatedRuntimeDir`s alive at the same time would race on the
//! env var. The harness serialises them via a private `Mutex` so
//! concurrent `cargo test` workers can both hit the harness without
//! corrupting each other. The lock is released when
//! `IsolatedRuntimeDir` drops.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::thread;

use anyhow::{anyhow, Result};
use tempfile::TempDir;
use tokio::runtime::Builder;

/// Process-wide serialisation for `XDG_RUNTIME_DIR` mutation. Each
/// `IsolatedRuntimeDir` holds the lock for its full lifetime so two
/// concurrent test threads can't observe each other's tempdir as
/// `$XDG_RUNTIME_DIR`.
static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Hermetic `$XDG_RUNTIME_DIR` backed by a `tempfile::TempDir`.
///
/// On construction: takes the env-serialisation lock, allocates a
/// tempdir, stashes the previous `$XDG_RUNTIME_DIR` value (if any),
/// and points `$XDG_RUNTIME_DIR` at the tempdir.
///
/// On drop: restores the previous env var, lets `TempDir` clean the
/// directory, releases the lock.
pub struct IsolatedRuntimeDir {
    // Order matters: `_guard` must drop last so the env-var restore
    // happens while we still own the serialisation lock. Rust drops
    // fields in declaration order, so `_temp` (cleanup) → `path` /
    // `prev_xdg` (env restore happens in our own `Drop`) → `_guard`.
    _temp: TempDir,
    path: PathBuf,
    prev_xdg: Option<OsString>,
    _guard: MutexGuard<'static, ()>,
}

impl IsolatedRuntimeDir {
    /// Allocate a fresh tempdir and rebind `$XDG_RUNTIME_DIR` to it.
    /// Blocks if another `IsolatedRuntimeDir` is already alive in
    /// this process.
    pub fn new() -> Result<Self> {
        let guard = TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir()?;
        let path = temp.path().to_path_buf();
        let prev_xdg = env::var_os("XDG_RUNTIME_DIR");
        env::set_var("XDG_RUNTIME_DIR", &path);
        Ok(Self {
            _temp: temp,
            path,
            prev_xdg,
            _guard: guard,
        })
    }

    /// Path the harness wrote into `$XDG_RUNTIME_DIR`. Useful for
    /// asserting socket paths or pre-creating subdirs the daemon
    /// expects (e.g. `$XDG_RUNTIME_DIR/sy/`).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IsolatedRuntimeDir {
    fn drop(&mut self) {
        match self.prev_xdg.take() {
            Some(v) => env::set_var("XDG_RUNTIME_DIR", v),
            None => env::remove_var("XDG_RUNTIME_DIR"),
        }
        // `_temp.drop()` removes the directory.
        // `_guard.drop()` releases TEST_ENV_LOCK.
    }
}

/// Handle to a daemon spawned on a dedicated thread by
/// [`spawn_in_thread`]. The daemon stops when its closure-returned
/// future completes; `shutdown()` joins the thread.
pub struct DaemonHandle {
    thread: Option<thread::JoinHandle<()>>,
}

impl DaemonHandle {
    /// Await the daemon thread's exit. Returns once the
    /// `spawn_in_thread` closure's future has resolved and the
    /// thread has joined. Wraps `join()` in `spawn_blocking` so it
    /// doesn't block the caller's tokio runtime.
    pub async fn shutdown(mut self) -> Result<()> {
        let Some(t) = self.thread.take() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || t.join())
            .await
            .map_err(|e| anyhow!("await join task: {e}"))?
            .map_err(|_| anyhow!("daemon thread panicked"))?;
        Ok(())
    }
}

/// Spawn a daemon closure on a dedicated OS thread with its own
/// current-thread tokio runtime and a fresh hermetic
/// `$XDG_RUNTIME_DIR`. The closure receives ownership of the
/// `IsolatedRuntimeDir` so it can wire socket paths into the daemon
/// before serving; when the closure's future resolves, the dir
/// drops, env state is restored, and the thread exits.
pub fn spawn_in_thread<F, Fut>(f: F) -> DaemonHandle
where
    F: FnOnce(IsolatedRuntimeDir) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let t = thread::spawn(move || {
        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime build");
        let dir = IsolatedRuntimeDir::new().expect("allocate isolated runtime dir");
        rt.block_on(f(dir));
    });
    DaemonHandle { thread: Some(t) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    const SHUTDOWN_BUDGET: Duration = Duration::from_secs(1);

    #[test]
    fn isolated_runtime_dir_round_trip() {
        let dir = IsolatedRuntimeDir::new().expect("allocate");
        let p = dir.path().to_path_buf();
        assert!(p.exists(), "tempdir should exist after new()");
        assert_eq!(
            env::var_os("XDG_RUNTIME_DIR").as_deref(),
            Some(p.as_os_str()),
            "XDG_RUNTIME_DIR should point at our tempdir while dir is alive",
        );
        let marker = p.join("marker");
        fs::write(&marker, b"hello").expect("write marker");
        assert!(marker.exists());

        drop(dir);

        assert!(
            !p.exists(),
            "tempdir should be removed by TempDir's Drop after IsolatedRuntimeDir drops"
        );
    }

    #[tokio::test]
    async fn spawn_in_thread_runs_and_shuts_down() {
        let h = spawn_in_thread(|_dir| async { /* no-op daemon body */ });
        let start = Instant::now();
        h.shutdown().await.expect("clean shutdown");
        assert!(
            start.elapsed() < SHUTDOWN_BUDGET,
            "no-op daemon should shut down inside {SHUTDOWN_BUDGET:?}",
        );
    }
}
