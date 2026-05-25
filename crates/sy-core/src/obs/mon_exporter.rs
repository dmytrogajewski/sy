//! Prometheus UDS exposition surface (sy-mon Step 9).
//!
//! Every plane daemon that opts into `--features mon-exporter` calls
//! [`install`] once during startup to bind a Unix Domain socket and
//! serve `metrics-exporter-prometheus`'s `/metrics` exposition there.
//! The aggregator (`sy mon collect`, Step 12) connects to each
//! per-plane `metrics.sock`, parses the response, and folds it into a
//! `SystemSnapshot`.
//!
//! ## Design
//!
//! - Single helper, shared by every plane: identical bind logic,
//!   identical Drop-time unlink. No per-plane glue beyond the path.
//! - Uses `metrics-exporter-prometheus`'s `with_http_uds_listener`
//!   API gated on its `uds-listener` feature (SPEC §3 deep-dive
//!   "`metrics-exporter-prometheus uds-listener` paragraph"). We do
//!   NOT introduce a second in-tree HTTP listener.
//! - The recorder is registered globally via
//!   `metrics::set_global_recorder` so every `counter!` / `gauge!` /
//!   `histogram!` call site in the daemon — including the
//!   pre-declared SPEC §4.6 names from
//!   [`crate::metrics::CORE_METRICS`] — flows through this exporter.
//!   `set_global_recorder` is process-global; if a second `install`
//!   races with it (e.g. a test re-entry on the same process), the
//!   first install wins and subsequent calls return [`InstallError::
//!   AlreadyInstalled`]. Callers should `install` exactly once.
//! - SIGTERM handling is the caller's job — the daemon already owns
//!   its signal pipeline. Dropping the [`UdsGuard`] aborts the accept
//!   task and unlinks the socket file, which is enough for graceful
//!   shutdown when the supervisor tears the runtime down.
//! - The accept task is spawned on the **current** tokio runtime; a
//!   runtime must therefore be active when [`install`] is called.
//!   The function panics on a missing runtime in the same way
//!   `tokio::spawn` does — that is a programmer error, not a runtime
//!   condition.
//!
//! ## Path semantics
//!
//! The parent directory of `path` is created with `0700` permissions
//! if it does not already exist (matches SPEC §4 "UDS path under
//! `$XDG_RUNTIME_DIR/sy/<plane>/`"). If a stale socket file exists at
//! `path`, the underlying `metrics-exporter-prometheus` builder
//! removes it before re-binding. The [`UdsGuard`] returned holds the
//! `PathBuf` and unlinks it on Drop.

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use metrics::{KeyName, Recorder, SharedString, Unit};
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::task::JoinHandle;

use crate::metrics::{CoreMetric, MetricKind, CORE_METRICS};

/// 0700 — owner-only, matches every other `$XDG_RUNTIME_DIR/sy/*`
/// socket directory created by the workspace (e.g. sy-ipc's
/// `socket_path()`).
const RUNTIME_DIR_MODE: u32 = 0o700;

/// Errors returned by [`install`]. Variants are exhaustive; callers
/// should pattern-match and log accordingly.
#[derive(Debug)]
pub enum InstallError {
    /// Could not create the parent directory of the socket path.
    CreateDir { path: PathBuf, source: io::Error },
    /// `PrometheusBuilder::build()` failed — typically a bind error
    /// (stale socket the crate could not remove, EACCES on the
    /// runtime dir, ENOENT after a TOCTOU race) or a misconfigured
    /// builder.
    Build {
        path: PathBuf,
        source: metrics_exporter_prometheus::BuildError,
    },
    /// The process already has a global `metrics` recorder
    /// installed. Only one `install` call per process is supported.
    AlreadyInstalled,
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateDir { path, source } => {
                write!(f, "create runtime dir {}: {source}", path.display())
            }
            Self::Build { path, source } => {
                write!(
                    f,
                    "build prometheus exporter for {}: {source}",
                    path.display()
                )
            }
            Self::AlreadyInstalled => f.write_str(
                "metrics recorder already installed (only one `install` per process is supported)",
            ),
        }
    }
}

impl StdError for InstallError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CreateDir { source, .. } => Some(source),
            Self::Build { source, .. } => Some(source),
            Self::AlreadyInstalled => None,
        }
    }
}

/// RAII handle to the running Prometheus UDS exporter. Dropping the
/// guard aborts the accept task and unlinks the socket file. The
/// daemon's supervisor holds it for the supervisor's lifetime
/// (Step 10 wires this for aiplane; Step 20 for the other planes).
#[must_use = "the UDS exporter stops when its guard is dropped"]
pub struct UdsGuard {
    path: PathBuf,
    accept_task: Option<JoinHandle<()>>,
}

impl UdsGuard {
    /// The path the socket is bound at. Useful for logging and for
    /// the aggregator's discovery glue.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UdsGuard {
    fn drop(&mut self) {
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
        // Best-effort unlink — match the pattern used by every other
        // sy daemon (see `src/agt/daemon.rs`, `src/power/daemon.rs`):
        // `let _ = remove_file(&sock)`. We are in Drop, so there is
        // no caller to propagate an error to; if the file is already
        // gone (SIGTERM raced our task abort) or the dir is read-only
        // for some pathological reason, the supervisor's next
        // start-up will overwrite anyway.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Install the Prometheus UDS exporter at `path`.
///
/// On success the returned [`UdsGuard`] owns the accept task and the
/// socket-file unlink-on-drop. The global `metrics` recorder is set
/// to the freshly-built `PrometheusRecorder`, and every
/// [`CORE_METRICS`] entry is described against it so the exposition
/// includes the `# HELP` / `# TYPE` lines from the moment the socket
/// is bound — useful for `sy mon doctor` (Step 21) to verify the
/// exporter is alive even before any workload has run.
///
/// ## Tokio requirement
///
/// A tokio runtime must be active when this is called. The
/// underlying `PrometheusBuilder::build()` spawns an upkeep task and
/// the exporter accept-loop on the current runtime. Calling outside
/// a runtime context will panic in the same way `tokio::spawn` does.
///
/// ## Idempotence
///
/// Process-global. The second `install` on the same process returns
/// [`InstallError::AlreadyInstalled`] without binding a second
/// socket. Production daemons call this once during startup; tests
/// run in fresh processes (or accept the AlreadyInstalled error on
/// subsequent installs within the same test binary).
pub fn install(path: PathBuf) -> Result<UdsGuard, InstallError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            create_runtime_dir(parent)?;
        }
    }

    let (recorder, exporter_future) = PrometheusBuilder::new()
        .with_http_uds_listener(&path)
        .build()
        .map_err(|source| InstallError::Build {
            path: path.clone(),
            source,
        })?;

    // Register every CORE_METRICS entry against the freshly-built
    // recorder before installing it globally, so the exposition's
    // `# HELP` / `# TYPE` lines are populated from the first GET
    // /metrics — even if the daemon hasn't emitted any value yet.
    describe_core_metrics(&recorder);

    if metrics::set_global_recorder(recorder).is_err() {
        // The recorder slot is process-global and already taken
        // (typically: a prior `install` call). `build()` has already
        // bound a UDS listener at `path`; drop the future to release
        // the listener and best-effort unlink the file so we leave
        // the filesystem in the state the caller expected.
        drop(exporter_future);
        let _ = std::fs::remove_file(&path);
        return Err(InstallError::AlreadyInstalled);
    }

    let accept_task = tokio::spawn(async move {
        // The exporter future serves the UDS accept loop forever or
        // until aborted by `UdsGuard::drop`. Errors are logged by the
        // exporter crate itself (warn! on `Error accepting
        // connection`). We swallow the terminal result to avoid a
        // panic on shutdown.
        let _ = exporter_future.await;
    });

    Ok(UdsGuard {
        path,
        accept_task: Some(accept_task),
    })
}

/// Create the runtime directory with `0700` perms. No-op if it
/// already exists with any mode (we don't enforce mode on existing
/// dirs — that's the systemd unit's job via `RuntimeDirectoryMode=`).
fn create_runtime_dir(parent: &Path) -> Result<(), InstallError> {
    if parent.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).map_err(|source| InstallError::CreateDir {
        path: parent.to_path_buf(),
        source,
    })?;
    // Best-effort tighten perms after create (Unix-only). Failure to
    // chmod is non-fatal — the systemd unit's RuntimeDirectoryMode
    // is the durable enforcement; this is defence-in-depth for
    // manually-invoked daemons (e.g. `cargo run` during development).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(parent) {
            let mut perms = meta.permissions();
            perms.set_mode(RUNTIME_DIR_MODE);
            let _ = std::fs::set_permissions(parent, perms);
        }
    }
    Ok(())
}

/// Call `Recorder::describe_*` on `recorder` for every entry in
/// [`CORE_METRICS`]. Mirrors [`crate::metrics::register_core_metrics`]
/// but targets a specific recorder instance — the global facade isn't
/// useful here because the global slot only gets set *after* this
/// call (and the macros route through the global, not the local
/// recorder).
fn describe_core_metrics(recorder: &impl Recorder) {
    for entry in CORE_METRICS {
        describe_one(recorder, entry);
    }
}

fn describe_one(recorder: &impl Recorder, entry: &CoreMetric) {
    let key: KeyName = entry.name.into();
    let unit: Option<Unit> = None;
    let description: SharedString = describe_text(entry.name).into();
    match entry.kind {
        MetricKind::Counter => recorder.describe_counter(key, unit, description),
        MetricKind::Gauge => recorder.describe_gauge(key, unit, description),
        MetricKind::Histogram => recorder.describe_histogram(key, unit, description),
    }
}

/// Per-name description string. Duplicates
/// [`crate::metrics::describe_text`] (which is `fn`-private) so the
/// local recorder gets the same prose as the global facade emits in
/// `register_core_metrics`.
fn describe_text(name: &str) -> &'static str {
    match name {
        "sy_workload_completed_total" => "successful workload runs, by workload kind",
        "sy_workload_errors_total" => "failed workload runs, by workload kind and reason class",
        "sy_policy_denials_total" => "sandbox policy denials, by tool",
        "sy_ipc_errors_total" => "IPC error responses, by endpoint and error kind",
        "sy_models_warm" => "current warm-pool occupancy, by workload kind",
        "sy_queue_depth" => "scheduler queue depth, by priority class and workload kind",
        "sy_npu_temp_celsius" => "NPU package temperature (latest sysfs reading)",
        "sy_workload_latency_seconds" => {
            "workload dispatch-to-completion latency, by workload kind"
        }
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;

    /// `metrics::set_global_recorder` is process-global; serialise
    /// the install tests so they don't race. The first test to hold
    /// the lock installs; subsequent tests see
    /// `InstallError::AlreadyInstalled` and skip the recorder-set
    /// assertion (the socket-binding and Drop-unlink behaviour is
    /// independently verifiable on each attempt because the bind
    /// happens in `build()` before `set_global_recorder`).
    ///
    /// Held across `.await` points, so this must be a
    /// `tokio::sync::Mutex` rather than `std::sync::Mutex` (clippy
    /// `await_holding_lock` lint).
    static INSTALL_LOCK: Mutex<()> = Mutex::const_new(());

    /// Wait up to `budget` for `path` to exist as a UDS — the
    /// exporter's accept task is async and the file is created when
    /// `build()` calls `UnixListener::bind`, which happens before
    /// `tokio::spawn` returns from our `install`, so the file is
    /// already there. But the bound-but-not-yet-accepting window is
    /// tiny; this helper exists to keep the test deterministic
    /// across slow CI hosts.
    async fn wait_for_socket(path: &Path, budget: Duration) {
        let start = std::time::Instant::now();
        while start.elapsed() < budget {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn install_creates_socket() {
        let _lock = INSTALL_LOCK.lock().await;
        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("metrics.sock");

        let guard_res = install(sock.clone());
        // Either install succeeded, or a previous test on this
        // process already set the global recorder; if the latter,
        // we cannot exercise the binding here without a second
        // recorder install. The drop-unlinks-socket test is the
        // authoritative bind check on the cold path.
        let _guard = match guard_res {
            Ok(g) => g,
            Err(InstallError::AlreadyInstalled) => return,
            Err(e) => panic!("install: {e}"),
        };

        // Emit one counter increment so the exposition has at least
        // one `# HELP` + `# TYPE` block. `metrics-exporter-prometheus`
        // omits descriptions for metrics that were only *described*
        // and never emitted (see the crate's `render_to_write`:
        // HELP / TYPE lines are written per-metric only when there
        // is a value to render). The catalogue name is the right
        // probe because it's also the name the aggregator (Step 12)
        // and `sy mon snapshot` (Step 14) will look for.
        metrics::counter!("sy_workload_completed_total", "kind" => "fake").increment(1);

        wait_for_socket(&sock, Duration::from_secs(2)).await;
        assert!(sock.exists(), "socket file {} not created", sock.display());

        let mut stream = UnixStream::connect(&sock)
            .await
            .expect("connect to metrics UDS");
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .expect("write GET");

        let mut buf = Vec::with_capacity(4096);
        let read = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf))
            .await
            .expect("read response (timeout)")
            .expect("read response (io)");
        assert!(read > 0, "empty response from metrics UDS");
        let text = String::from_utf8_lossy(&buf);
        // The body must look like Prometheus text exposition. Two
        // independent acceptance conditions per the SPEC's "Prom
        // exposition" definition: HELP / TYPE comment lines from the
        // descriptions we registered, OR at least one CORE_METRICS
        // name appearing verbatim. Either is sufficient; both are
        // expected in practice.
        let has_help_or_type = text.contains("# HELP") || text.contains("# TYPE");
        let mentions_core_metric = CORE_METRICS.iter().any(|m| text.contains(m.name));
        assert!(
            has_help_or_type || mentions_core_metric,
            "response body does not look like Prometheus exposition:\n{text}"
        );
    }

    #[tokio::test]
    async fn drop_unlinks_socket() {
        let _lock = INSTALL_LOCK.lock().await;
        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("metrics.sock");

        // If a prior test already installed, we can't install again
        // — but bind happens in `build()` which DOES run, so the
        // file would be created and then the recorder install would
        // fail and we'd have leaked the file. Skip in that case to
        // keep the test deterministic; the cold-path run of this
        // test alone covers the assertion.
        match install(sock.clone()) {
            Ok(guard) => {
                wait_for_socket(&sock, Duration::from_secs(2)).await;
                assert!(sock.exists(), "socket {} not bound", sock.display());
                drop(guard);
                // Drop is sync; the unlink is `std::fs::remove_file`
                // and happens before drop returns.
                assert!(
                    !sock.exists(),
                    "socket {} still present after guard drop",
                    sock.display()
                );
            }
            Err(InstallError::AlreadyInstalled) => {
                // Already-installed path: nothing to drop in this
                // test; the cold-path run covers the assertion.
            }
            Err(e) => panic!("install: {e}"),
        }
    }
}
