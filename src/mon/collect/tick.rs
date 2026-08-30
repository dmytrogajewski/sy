//! 1 Hz tick scheduler. Each tick runs the host sample and the cross-
//! plane scrape under a 500 ms per-source timeout (SPEC §4 "per-source
//! timeout 500 ms, failure tagged in snapshot's errors[], never blocks
//! the tick"), projects the host sample into one ring-buffer row, and
//! folds host + plane scrape results into a `SystemSnapshot`.
//!
//! The sample function and the plane-path slice are both injected so
//! tests can swap in a hung sensor / a tempdir-backed fake plane UDS
//! and assert the tick still completes within budget.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sy_core::mon::ring::Ring;
use sy_core::mon::snapshot::{MonError, SystemSnapshot};
use tokio::task::JoinHandle;

use super::sample::{project_row, HostSample};
use super::scrape::{scrape_plane, PlaneMetrics};
use super::snapshot::{fold_into_snapshot, PlaneScrape};

/// Per-source scrape budget (SPEC §4 Reliability). The host sample
/// blocks for ~100 ms reading `/proc/stat` twice, so 500 ms is roomy
/// enough for stalled sensors without dragging the tick past 1 s.
pub const PER_SOURCE_TIMEOUT: Duration = Duration::from_millis(500);

/// `MonError.plane` value emitted when the host sampler times out or
/// panics. Single source of truth so the test and the production tick
/// don't drift on the discriminator.
pub const HOST_PLANE: &str = "host";

/// `MonError.kind` value for a per-source timeout.
pub const KIND_TIMEOUT: &str = "timeout";

/// Plane names whose `metrics.sock` `sy mon collect` attempts to scrape
/// every tick. Step 10 wires aiplane's producer end; Step 20 wires the
/// rest. Until then the scraper's path discovery still includes them so
/// the moment a socket appears it gets picked up — failed connects
/// surface as `MonError { kind: "scrape_failed", … }`.
pub const KNOWN_PLANES: &[&str] = &["aiplane", "knowledge", "agt", "supervisor", "stack-bar"];

/// Resolve one known plane's expected `metrics.sock` path under
/// `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock` — the layout SPEC §3
/// SCOPE item 1 pins down and `aiplane::mon_exporter` enforces on the
/// producer side. Returns `None` when `XDG_RUNTIME_DIR` is missing so
/// the caller can skip the scrape phase cleanly on a non-session host.
pub fn plane_socket_path(plane: &str) -> Option<PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR").ok()?;
    if base.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(base)
            .join("sy")
            .join(plane)
            .join("metrics.sock"),
    )
}

/// Run one tick: sample the host (via `sampler`, wrapped in
/// `spawn_blocking` so blocking reads don't stall the runtime), scrape
/// every plane in `plane_paths` in parallel, apply the per-source
/// timeout to each leg, project the host sample into a ring row, push.
/// Returns the folded `SystemSnapshot` plus any errors observed during
/// the tick (host + per-plane).
///
/// `plane_paths` is a slice of `(plane_name, socket_path)` pairs —
/// production resolves it via [`plane_socket_path`] over `KNOWN_PLANES`,
/// tests inject a tempdir-backed UDS so the end-to-end fold is
/// exercised hermetically.
pub async fn run_once<F>(
    ring: &mut Ring,
    n_metrics: usize,
    sampler: F,
    plane_paths: &[(String, PathBuf)],
) -> (SystemSnapshot, Vec<MonError>)
where
    F: FnOnce() -> HostSample + Send + 'static,
{
    let mut errors = Vec::new();
    let handle: JoinHandle<HostSample> = tokio::task::spawn_blocking(sampler);
    let sample = match tokio::time::timeout(PER_SOURCE_TIMEOUT, handle).await {
        Ok(Ok(s)) => s,
        Ok(Err(join_err)) => {
            errors.push(MonError {
                plane: HOST_PLANE.into(),
                kind: "panic".into(),
                message: format!("host sampler panicked: {join_err}"),
            });
            HostSample::default()
        }
        Err(_elapsed) => {
            errors.push(MonError {
                plane: HOST_PLANE.into(),
                kind: KIND_TIMEOUT.into(),
                message: format!(
                    "host sampler exceeded {} ms budget",
                    PER_SOURCE_TIMEOUT.as_millis()
                ),
            });
            HostSample::default()
        }
    };
    let row = project_row(&sample, n_metrics);
    if let Err(e) = ring.push(&row) {
        errors.push(MonError {
            plane: HOST_PLANE.into(),
            kind: "ring_push_failed".into(),
            message: format!("{e:#}"),
        });
    }

    let plane_results = scrape_all(plane_paths).await;
    let mut snap = fold_into_snapshot(sample, &plane_results, &mut errors);
    snap.captured_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    (snap, errors)
}

/// Scrape every plane in `plane_paths` in parallel, applying
/// [`PER_SOURCE_TIMEOUT`] to each leg. Failures (connect / parse /
/// timeout) come back as `Err(_)` so the fold can record a structured
/// `MonError`; successes carry the parsed `PlaneMetrics`.
async fn scrape_all(plane_paths: &[(String, PathBuf)]) -> Vec<PlaneScrape> {
    use futures_util::future::join_all;
    let futures = plane_paths
        .iter()
        .map(|(plane, path)| scrape_one_with_timeout(plane.clone(), path.clone()));
    join_all(futures).await
}

/// Wrap one `scrape_plane` call in `tokio::time::timeout` so a stuck
/// plane can never drag the tick past the 500 ms budget. On `Elapsed`
/// we synthesise a tagged `anyhow::Error` so the fold's `MonError.kind`
/// stays `scrape_failed` (the timeout discriminator on the snapshot is
/// the host's `KIND_TIMEOUT`; per-plane timeouts share the
/// `scrape_failed` bucket per Step 12 spec mapping).
async fn scrape_one_with_timeout(plane: String, path: PathBuf) -> PlaneScrape {
    let result = match tokio::time::timeout(PER_SOURCE_TIMEOUT, scrape_one(&plane, &path)).await {
        Ok(inner) => inner,
        Err(_elapsed) => Err(anyhow::anyhow!(
            "scrape >{}ms",
            PER_SOURCE_TIMEOUT.as_millis()
        )),
    };
    (plane, result)
}

/// Thin wrapper to fix the `?Sized` boundary `join_all` infers when
/// fed a closure that returns `impl Future` directly. Lifting the
/// scrape into a dedicated `async fn` avoids the borrow gymnastics
/// without changing semantics.
async fn scrape_one(plane: &str, path: &Path) -> anyhow::Result<PlaneMetrics> {
    scrape_plane(plane, path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    const N_SECS: u32 = 4;
    const N_METRICS: u32 = 16;
    /// Canned aiplane exposition served over the fake plane UDS in
    /// `tick_folds_aiplane_scrape_into_snapshot`. Same source the
    /// scraper's standalone test uses so the producer/consumer pair
    /// stays in sync.
    const AIPLANE_FIXTURE: &str =
        include_str!("../../../tests/fixtures/mon/prom/aiplane/metrics.txt");

    /// SPEC §4 testing strategy: a tick with a healthy host sample
    /// increments the ring's seq counter and the projected CPU mean
    /// lands in column 0.
    #[tokio::test]
    async fn tick_writes_ring_buffer() {
        use sy_core::sensors::cpu::CpuSample;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        let mut ring = Ring::open_or_rebuild(&path, N_SECS, N_METRICS).expect("ring open");
        assert_eq!(ring.seq(), 0);

        let canned = HostSample {
            cpu: Some(CpuSample {
                per_core_util_pct: vec![10.0, 30.0, 50.0, 70.0],
                freq_mhz: Vec::new(),
                temp_c: None,
            }),
            ..HostSample::default()
        };
        let (_snap, errors) = run_once(&mut ring, N_METRICS as usize, move || canned, &[]).await;
        assert!(
            errors.is_empty(),
            "healthy tick must not emit errors: {errors:?}"
        );
        assert_eq!(ring.seq(), 1, "seq must increment after one push");

        let col0 = ring.read_metric(0, 1).expect("read col 0");
        assert_eq!(col0.len(), 1);
        // (10+30+50+70)/4 = 40.0
        assert!(
            (col0[0] - 40.0).abs() < f32::EPSILON,
            "col 0 expected ~40.0, got {}",
            col0[0]
        );
        // Live-smoke regression: the IPC `system.mon.snapshot` returned
        // `captured_at_ms: 0` because `tick::run_once` folded the snap
        // without stamping the time. Anchor it now — non-zero millis.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock past UNIX_EPOCH")
            .as_millis() as u64;
        assert!(
            _snap.captured_at_ms > 0 && _snap.captured_at_ms <= now_ms,
            "captured_at_ms must be stamped, got {}",
            _snap.captured_at_ms
        );
    }

    /// SPEC §4 Reliability: "per-source timeout 500 ms, failure tagged
    /// in snapshot's errors[], never blocks the tick".
    #[tokio::test(flavor = "multi_thread")]
    async fn scrape_timeout_does_not_block_tick() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        let mut ring = Ring::open_or_rebuild(&path, N_SECS, N_METRICS).expect("ring open");

        let hang_sampler = || {
            // 5-second blocking sleep — well beyond the 500 ms budget.
            std::thread::sleep(Duration::from_secs(5));
            HostSample::default()
        };
        // Step 12 extends the regression to cover the plane-scrape
        // phase: bind a UDS but never accept, so `scrape_plane` hangs
        // until the per-source timeout fires. The tick must still
        // unblock at the 500 ms host budget + 500 ms scrape budget,
        // never the 5 s sleep.
        let hung_sock = dir.path().join("hung.sock");
        let _hung_listener = UnixListener::bind(&hung_sock).expect("bind hung plane");
        let plane_paths = vec![("hung-plane".to_string(), hung_sock.clone())];

        let start = Instant::now();
        let (snap, errors) =
            run_once(&mut ring, N_METRICS as usize, hang_sampler, &plane_paths).await;
        let elapsed = start.elapsed();

        // Tick MUST complete in ~2 × PER_SOURCE_TIMEOUT, not 5 s. The
        // host phase and the scrape phase run sequentially in `run_once`
        // so the worst-case is ~1 s; a generous 2 s ceiling keeps the
        // assertion stable on a loaded CI host.
        assert!(
            elapsed < Duration::from_millis(2_000),
            "tick took {elapsed:?}; should have unblocked at the per-source budget"
        );
        let timeout_err = errors
            .iter()
            .find(|e| e.kind == KIND_TIMEOUT && e.plane == HOST_PLANE)
            .expect("host timeout error must be recorded");
        assert_eq!(timeout_err.plane, HOST_PLANE);
        // The hung plane surfaces as a scrape_failed entry under its
        // plane name — proves the parallel timeout fired.
        let scrape_err = snap
            .errors
            .iter()
            .find(|e| e.plane == "hung-plane")
            .expect("hung plane must surface a scrape_failed error");
        assert_eq!(scrape_err.kind, "scrape_failed");
        // Ring still pushed (with zeros) so seq advances.
        assert_eq!(ring.seq(), 1, "ring still receives a zero row on timeout");
    }

    /// Step 12 DoD "Aggregator with only aiplane wired produces a
    /// populated `SystemSnapshot` end-to-end": a unit test that runs
    /// `tick::run_once` with one real fake plane UDS serving canned
    /// exposition, and the produced `SystemSnapshot` matches.
    #[tokio::test(flavor = "multi_thread")]
    async fn tick_folds_aiplane_scrape_into_snapshot() {
        let dir = tempdir().expect("tempdir");
        let history = dir.path().join("history.bin");
        let mut ring = Ring::open_or_rebuild(&history, N_SECS, N_METRICS).expect("ring open");

        let sock = dir.path().join("aiplane-metrics.sock");
        let listener = UnixListener::bind(&sock).expect("bind UDS");
        let body = AIPLANE_FIXTURE.to_string();
        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.expect("accept");
            let mut req_buf = [0u8; 1024];
            let _ = conn.read(&mut req_buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            conn.write_all(response.as_bytes()).await.expect("write");
            conn.shutdown().await.expect("shutdown");
        });

        let plane_paths = vec![("aiplane".to_string(), sock.clone())];
        let (snap, errors) = run_once(
            &mut ring,
            N_METRICS as usize,
            HostSample::default,
            &plane_paths,
        )
        .await;
        server.await.expect("server task");

        assert!(
            errors.is_empty(),
            "end-to-end tick must surface no errors: {errors:?}"
        );
        assert_eq!(
            snap.aiplane.queue_depth.get("embed").copied(),
            Some(2),
            "expected fold to land sy_queue_depth{{kind=embed}} == 2; got {:?}",
            snap.aiplane.queue_depth
        );
    }
}
