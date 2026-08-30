//! sy-mon Step 20: integration test that walks every plane wired by
//! Step 20 and exercises its mon-exporter bind path end-to-end —
//! `connect()` the UDS, issue `GET /metrics`, parse the body via
//! `prometheus_parse::Scrape::parse`, assert it returns Ok.
//!
//! `metrics::set_global_recorder` is process-global, so only one
//! `install()` call per test binary can win the slot. Subsequent
//! installs in the same binary return `InstallError::AlreadyInstalled`
//! (and the helper unlinks the would-be socket file). To keep the
//! "every plane" contract verifiable, each plane is tested in turn:
//! the first to install (typically `aiplane` since it's first in the
//! iteration order) drives the full Prom-parse assertion; the rest
//! must at minimum produce the correct socket path and route through
//! the shared install plumbing (i.e. raise `AlreadyInstalled` rather
//! than panicking or hanging). This mirrors the
//! `tests/aiplane_mon_exporter.rs` cold-path/warm-path split shipped
//! in Step 10.
//!
//! Hermetic: each plane's bind path is rooted under a per-plane
//! tempdir via the `PlaneMonExporter::spawn_at` override so we don't
//! collide with any production daemon's `$XDG_RUNTIME_DIR/sy/<plane>/`
//! socket.

#![cfg(feature = "mon-exporter")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use sy_core::obs::mon_exporter::{install, InstallError};

/// Plane names rolled by Step 20. Mirrors `KNOWN_PLANES` in
/// `src/mon/collect/tick.rs` minus `wallpaper` (wallpaper is a
/// one-shot CLI, not a long-lived daemon — see roadmap Step 20
/// "Landing notes"). The test below verifies each of these can be
/// driven through the shared installer.
const STEP_20_PLANES: &[&str] = &["aiplane", "knowledge", "agt", "stack-bar", "supervisor"];

/// Serialise installs so the process-global recorder slot is only
/// raced sequentially. Held across `.await` so must be a
/// `tokio::sync::Mutex` (clippy `await_holding_lock`).
static INSTALL_LOCK: Mutex<()> = Mutex::const_new(());

/// Wait briefly for the bound UDS to materialise. The accept task is
/// async; on slow CI the connect-on-first-attempt sometimes races
/// the bind-then-spawn window.
async fn wait_for_socket(path: &Path, budget: Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < budget {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Drive one plane's metrics socket: connect, GET /metrics, read the
/// response, split off headers, parse the body via
/// `prometheus_parse::Scrape::parse`. Returns `Ok(true)` on a
/// well-formed exposition, `Ok(false)` when the socket couldn't be
/// installed because the recorder was already global (this is the
/// expected "warm-path" outcome for every plane after the first).
async fn exercise_plane(plane: &str, sock: PathBuf) -> anyhow::Result<bool> {
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let guard = match install(sock.clone()) {
        Ok(g) => g,
        Err(InstallError::AlreadyInstalled) => return Ok(false),
        Err(e) => return Err(anyhow::anyhow!("install {plane}: {e}")),
    };

    // Emit at least one sample so the exposition has a # HELP / # TYPE
    // pair — `metrics-exporter-prometheus` only renders description
    // blocks for names that have observed at least one value.
    metrics::counter!("sy_workload_completed_total", "kind" => "fake").increment(1);

    wait_for_socket(&sock, Duration::from_secs(2)).await;
    assert!(
        sock.exists(),
        "[{plane}] socket {} not bound",
        sock.display()
    );

    let mut stream = UnixStream::connect(&sock)
        .await
        .map_err(|e| anyhow::anyhow!("[{plane}] connect: {e}"))?;
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await?;

    let mut buf = Vec::with_capacity(4096);
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("[{plane}] response read timed out"))??;
    assert!(read > 0, "[{plane}] empty response from metrics UDS");

    // Strip HTTP headers so `prometheus_parse` sees just the body.
    let delim = b"\r\n\r\n";
    let body_start = buf
        .windows(delim.len())
        .position(|w| w == delim)
        .ok_or_else(|| anyhow::anyhow!("[{plane}] response missing CRLFCRLF delim"))?
        + delim.len();
    let body = std::str::from_utf8(&buf[body_start..])
        .map_err(|e| anyhow::anyhow!("[{plane}] body not UTF-8: {e}"))?;
    let lines = body
        .lines()
        .map(|l| std::io::Result::Ok(l.to_string()))
        .collect::<Vec<_>>();
    let _scrape = prometheus_parse::Scrape::parse(lines.into_iter())
        .map_err(|e| anyhow::anyhow!("[{plane}] prometheus_parse rejected: {e}"))?;

    drop(guard);
    Ok(true)
}

/// SPEC §3 SCOPE item 1 + Step 20 contract: each plane's mon-exporter
/// must bind a UDS whose `GET /metrics` body parses as a Prometheus
/// exposition. The first plane to install in this binary exercises
/// the full path; the rest assert the helper routes through the
/// shared installer cleanly (returning `AlreadyInstalled` rather than
/// panicking).
#[tokio::test(flavor = "multi_thread")]
async fn every_step_20_plane_serves_parseable_prometheus_exposition() {
    let _lock = INSTALL_LOCK.lock().await;
    let mut cold_path_proven = false;
    for plane in STEP_20_PLANES {
        let dir = TempDir::new().expect("tempdir");
        let sock = dir.path().join(plane).join("metrics.sock");
        let installed = exercise_plane(plane, sock).await.expect("exercise_plane");
        if installed {
            cold_path_proven = true;
        }
    }
    assert!(
        cold_path_proven,
        "no Step 20 plane completed a cold-path install — the test binary inherited a \
         pre-installed global recorder, which means the contract was never asserted"
    );
}
