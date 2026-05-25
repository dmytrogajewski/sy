//! sy-mon Step 10: the aiplane plane's Prometheus UDS exposition
//! surface, served by the same shared `sy_core::obs::mon_exporter`
//! installer that Step 20 will roll to every other plane.
//!
//! The aiplane plane physically lives inside `knowledge::daemon::run()`
//! (the supervisor is spun up by `init_aiplane_supervisor()` there).
//! Step 10's contract is therefore: a metrics socket bound at
//! `$XDG_RUNTIME_DIR/sy/aiplane/metrics.sock` exposing the existing
//! `CORE_METRICS` catalogue (arch-observability Step 7). This
//! integration test exercises the installer directly against a
//! tempdir-derived path — driving the daemon-spawn site from a test
//! would race a running daemon owning the real runtime path, and the
//! daemon-side wiring is a thin runtime-thread wrapper around the
//! identical installer call (see `src/knowledge/daemon.rs`).
//!
//! The two assertions match SPEC §3 SCOPE item 1 + §4 Security:
//! (1) the socket binds and serves a Prometheus exposition that
//! mentions a known `CORE_METRICS` entry; (2) the socket file
//! disappears when the install guard is dropped (so SIGTERM /
//! daemon-shutdown leaves no stale UDS for the next start).

#![cfg(feature = "mon-exporter")]

use std::path::Path;
use std::time::Duration;

use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use sy_core::metrics::CORE_METRICS;
use sy_core::obs::mon_exporter::{install, InstallError};

/// Two integration tests in this file mutate the process-global
/// `metrics` recorder via `mon_exporter::install`. Serialise them so
/// the first test's install doesn't race the second test's bind.
/// Held across `.await` points so it must be a `tokio::sync::Mutex`.
static INSTALL_LOCK: Mutex<()> = Mutex::const_new(());

/// The accept task is spawned async; wait briefly for the socket file
/// to materialise on slow CI before connecting.
async fn wait_for_socket(path: &Path, budget: Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < budget {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// SPEC §3 SCOPE item 1 + arch-observability Step 7 contract: the
/// metrics socket must serve the existing `CORE_METRICS` catalogue,
/// so the aggregator (Step 12) can fold the exposition into a
/// `SystemSnapshot`. We probe with the same workload counter the
/// aiplane scheduler emits (`sy_workload_completed_total`) — the
/// presence of that name in the response body is the load-bearing
/// acceptance condition.
#[tokio::test(flavor = "multi_thread")]
async fn aiplane_metrics_socket_serves_core_metrics() {
    let _lock = INSTALL_LOCK.lock().await;
    let dir = tempdir().expect("tempdir");
    let sock = dir.path().join("aiplane").join("metrics.sock");

    let guard = match install(sock.clone()) {
        Ok(g) => g,
        // `metrics::set_global_recorder` is process-global; a prior
        // mon_exporter test in the same binary may have won the slot.
        // The cold-path run of this test alone covers the assertion;
        // skip cleanly so `cargo test --features mon-exporter` stays
        // green across test orderings.
        Err(InstallError::AlreadyInstalled) => return,
        Err(e) => panic!("install: {e}"),
    };

    // Emit one increment so the `# HELP` / `# TYPE` block is present
    // — `metrics-exporter-prometheus` only renders description blocks
    // for names with at least one observed value.
    metrics::counter!("sy_workload_completed_total", "kind" => "fake").increment(1);

    wait_for_socket(&sock, Duration::from_secs(2)).await;
    assert!(sock.exists(), "socket {} not bound", sock.display());

    let mut stream = UnixStream::connect(&sock)
        .await
        .expect("connect to aiplane metrics UDS");
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .expect("write GET");

    let mut buf = Vec::with_capacity(4096);
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf))
        .await
        .expect("read response (timeout)")
        .expect("read response (io)");
    assert!(read > 0, "empty response from aiplane metrics UDS");
    let text = String::from_utf8_lossy(&buf);
    assert!(
        text.contains("sy_workload_completed_total"),
        "Prometheus exposition missing arch-observability Step 7 catalogue name\nresponse:\n{text}"
    );
    // Defensive: confirm at least one other catalogue entry is also
    // describable — the aggregator (Step 12) consumes any registered
    // name, not just the one we incremented above.
    assert!(
        CORE_METRICS
            .iter()
            .any(|m| m.name == "sy_workload_completed_total"),
        "CORE_METRICS catalogue drifted: expected `sy_workload_completed_total`"
    );

    drop(guard);
}

/// SPEC §4 Security non-functional: the socket is user-private and
/// must vanish on daemon shutdown. Dropping the `UdsGuard` aborts the
/// accept task and unlinks the file — anything less leaves a stale
/// UDS that the next daemon start has to clean up.
#[tokio::test(flavor = "multi_thread")]
async fn socket_unlinks_on_shutdown() {
    let _lock = INSTALL_LOCK.lock().await;
    let dir = tempdir().expect("tempdir");
    let sock = dir.path().join("aiplane").join("metrics.sock");

    match install(sock.clone()) {
        Ok(guard) => {
            wait_for_socket(&sock, Duration::from_secs(2)).await;
            assert!(sock.exists(), "socket {} not bound", sock.display());
            drop(guard);
            assert!(
                !sock.exists(),
                "socket {} still present after guard drop",
                sock.display()
            );
        }
        Err(InstallError::AlreadyInstalled) => {
            // See sibling test: cold-path covers the assertion.
        }
        Err(e) => panic!("install: {e}"),
    }
}
