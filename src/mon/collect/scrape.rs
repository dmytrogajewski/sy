//! Cross-plane scrape leg of the `sy mon collect` tick (SPEC §3 SCOPE
//! item 3 + roadmap Step 12). Each plane daemon binds a Prometheus UDS
//! exposition surface at `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock`
//! (Steps 10 + 20); the aggregator connects, issues a minimal HTTP/1.1
//! `GET /metrics`, reads to EOF, and parses the body via
//! `prometheus_parse`.
//!
//! Why a hand-written request instead of `reqwest` / `hyper` — the SPEC
//! §6 "second HTTP stack" risk pins the producer side to one
//! Prometheus exporter; the consumer side stays equally bare. The
//! request body is fixed (`GET /metrics HTTP/1.1\r\nHost: x\r\n
//! Connection: close\r\n\r\n`) and `Connection: close` lets us read
//! until EOF without parsing chunked transfer encoding.
//!
//! The 500 ms per-source timeout (SPEC §4 Reliability) lives in the
//! tick that calls this module — `scrape_plane` itself returns
//! quickly on ENOENT / parse failure so the tick can tag a structured
//! `MonError` and move on.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// HTTP/1.1 request literal sent on every scrape. `Connection: close`
/// signals the server to flush + close after the response body so the
/// client can read until EOF without speaking chunked encoding.
const SCRAPE_REQUEST: &[u8] = b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

/// CRLFCRLF delimiter between an HTTP response's headers and body.
const HEADER_BODY_DELIM: &[u8] = b"\r\n\r\n";

/// One plane's parsed metrics — the unit `fold_into_snapshot` consumes.
/// Lightweight (no Serialize) on purpose: only `SystemSnapshot` is
/// serialised on the wire; `PlaneMetrics` is the aggregator's in-memory
/// intermediate.
#[derive(Debug, Clone)]
pub struct PlaneMetrics {
    /// Plane name as keyed in the snapshot (`"aiplane"`, `"knowledge"`,
    /// …). Set by the caller so the scraper doesn't have to invert the
    /// path → plane mapping itself.
    pub plane: String,
    /// Parsed samples from the plane's Prometheus exposition. May be
    /// empty when the plane is up but hasn't observed any of its
    /// declared metrics yet (e.g. a fresh aiplane daemon before the
    /// first workload).
    pub samples: Vec<prometheus_parse::Sample>,
}

/// Scrape one plane's `metrics.sock`. Connects, sends `GET /metrics`,
/// reads the response, splits off the headers, parses the body. Caller
/// supplies the plane name so error reporting stays attached to the
/// canonical snapshot key.
pub async fn scrape_plane(plane: &str, path: &Path) -> Result<PlaneMetrics> {
    let body = fetch_metrics_body(path).await?;
    let samples = parse_body(&body).with_context(|| format!("parse {} metrics body", plane))?;
    Ok(PlaneMetrics {
        plane: plane.to_string(),
        samples,
    })
}

/// Connect to the plane UDS, issue `SCRAPE_REQUEST`, read to EOF, and
/// return everything after the `CRLFCRLF` header delimiter. Errors
/// thread through `anyhow::Context` so the tick's structured
/// `MonError.message` carries the failure site (`connect` / `write` /
/// `read` / `delimiter`).
async fn fetch_metrics_body(path: &Path) -> Result<Vec<u8>> {
    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connect {}", path.display()))?;
    stream
        .write_all(SCRAPE_REQUEST)
        .await
        .context("write scrape request")?;
    let mut buf = Vec::with_capacity(4096);
    stream
        .read_to_end(&mut buf)
        .await
        .context("read scrape response")?;
    let delim = buf
        .windows(HEADER_BODY_DELIM.len())
        .position(|w| w == HEADER_BODY_DELIM)
        .ok_or_else(|| anyhow!("response missing CRLFCRLF header/body delimiter"))?;
    let body_start = delim + HEADER_BODY_DELIM.len();
    Ok(buf[body_start..].to_vec())
}

/// Run a `&[u8]` body through `prometheus_parse::Scrape::parse`. The
/// crate expects an iterator of `io::Result<String>`; we feed UTF-8
/// lines split on `\n` (the exposition format is line-oriented).
fn parse_body(body: &[u8]) -> Result<Vec<prometheus_parse::Sample>> {
    let text = std::str::from_utf8(body).context("metrics body is not UTF-8")?;
    let lines = text
        .lines()
        .map(|l| std::io::Result::Ok(l.to_string()))
        .collect::<Vec<_>>();
    let scrape = prometheus_parse::Scrape::parse(lines.into_iter())
        .context("prometheus_parse rejected exposition body")?;
    Ok(scrape.samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    /// Fixture path used by `fake_plane_yields_metrics`. The repo-root
    /// `tests/fixtures/mon/prom/aiplane/metrics.txt` is hand-written to
    /// match `crates/sy-core/src/metrics.rs::CORE_METRICS` names so the
    /// folder downstream (Step 12 snapshot fold) can be driven from the
    /// same canned exposition.
    const AIPLANE_FIXTURE: &str =
        include_str!("../../../tests/fixtures/mon/prom/aiplane/metrics.txt");

    /// SPEC §3 SCOPE item 3 + roadmap Step 12: when a plane serves
    /// canned exposition over its `metrics.sock`, `scrape_plane` parses
    /// it into `PlaneMetrics` whose `samples` contain the catalogued
    /// metric names. Drives a `tokio::net::UnixListener` in a spawned
    /// task that serves one connection and exits.
    #[tokio::test(flavor = "multi_thread")]
    async fn fake_plane_yields_metrics() {
        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("metrics.sock");
        let listener = UnixListener::bind(&sock).expect("bind UDS");
        let body = AIPLANE_FIXTURE.to_string();

        // Serve exactly one connection: read the request, respond with
        // `HTTP/1.1 200 OK` + the canned body, close. Spawned on the
        // multi-thread runtime so the client's `connect` doesn't
        // deadlock on a single-threaded scheduler.
        let server = tokio::spawn(async move {
            let (mut conn, _addr) = listener.accept().await.expect("accept");
            // Drain the request so the client's write completes.
            let mut req_buf = [0u8; 1024];
            let _ = conn.read(&mut req_buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            conn.write_all(response.as_bytes()).await.expect("write");
            conn.shutdown().await.expect("shutdown");
        });

        let metrics = scrape_plane("aiplane", &sock).await.expect("scrape");
        server.await.expect("server task");

        assert_eq!(metrics.plane, "aiplane");
        let names: Vec<&str> = metrics.samples.iter().map(|s| s.metric.as_str()).collect();
        assert!(
            names.contains(&"sy_queue_depth"),
            "expected sy_queue_depth in samples, got {names:?}"
        );
        assert!(
            names.contains(&"sy_workload_completed_total"),
            "expected sy_workload_completed_total in samples, got {names:?}"
        );
    }
}
