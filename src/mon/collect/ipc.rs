//! sy-ipc `Handler` for the `sy mon collect` aggregator.
//!
//! Serves the three reserved methods (sy-mon ROADMAP Step 13, SPEC §4
//! "CLI / MCP surface"):
//!
//! - `system.mon.snapshot` — unary, returns the most recent
//!   [`SystemSnapshot`] stored by the tick loop.
//! - `system.mon.history` — unary, returns the ring buffer's last
//!   `seconds` `(captured_at_ms, value)` pairs for one metric.
//! - `system.mon.subscribe` — streaming, writes one
//!   [`sy_ipc::Event`] per tick until the client closes the connection.
//!
//! `system.{describe,health,cancel}` are composed via
//! [`sy_ipc::SystemMethods::try_handle`] so the daemon answers the full
//! IPC v1 surface; unknown methods produce `ErrorCode::BadRequest`.
//!
//! ## Streaming
//!
//! sy-ipc's [`sy_ipc::Handler`] trait returns one `Response` per call,
//! so `subscribe` cannot ride on the generic [`sy_ipc::Server`] accept
//! loop. We mirror the bespoke accept loop that `src/agt/daemon.rs`
//! already uses for `agt.tail`: write the initial
//! [`Response::Ok`] ack, switch the writer to [`EventCodec`], emit one
//! `Event { kind = "snapshot", ... }` per tick on the broadcast
//! channel, and terminate with [`Event::closed`] when the broadcast
//! sender drops. Client disconnects bubble up as an
//! [`std::io::ErrorKind::BrokenPipe`] from `event_sink.send(...)` and the
//! per-connection task exits cleanly.
//!
//! ## Concurrency
//!
//! Three shared handles:
//!
//! - [`LatestSnapshot`] (`Arc<ArcSwap<SystemSnapshot>>`) — tick stores,
//!   handlers load; no mutex on the read path.
//! - `Arc<Mutex<Ring>>` — tick pushes a row; `history` reads. The
//!   mutex scope is sub-millisecond on both sides, so a plain mutex is
//!   fine.
//! - `broadcast::Sender<()>` — tick sends one signal after every
//!   `LatestSnapshot::store`; each subscriber's `recv()` wakes up.
//!   `Lagged` errors are treated as a tick signal anyway.

use std::path::Path;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use sy_core::mon::ring::Ring;
use sy_core::mon::snapshot::{LatestSnapshot, SystemSnapshot};
use sy_core::ErrorCode;
use sy_ipc::codec::{RequestCodec, ResponseCodec};
use sy_ipc::envelope::{ErrorBody, Request, Response, SCHEMA_VERSION};
use sy_ipc::stream::{Event, EventCodec};
use sy_ipc::{MonHistoryParams, SystemMethods, SYSTEM_METHODS};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};
use tokio_util::codec::{FramedRead, FramedWrite};
use ulid::Ulid;

/// IPC method names.
const METHOD_SNAPSHOT: &str = "system.mon.snapshot";
const METHOD_SUBSCRIBE: &str = "system.mon.subscribe";
const METHOD_HISTORY: &str = "system.mon.history";

/// `Event.kind` discriminator emitted on every tick of `subscribe`.
pub const EVENT_KIND_SNAPSHOT: &str = "snapshot";

/// Capacity of the per-aggregator tick broadcast. The channel only
/// carries a `()` signal so capacity ≈ "how many ticks a slow
/// subscriber may fall behind before lagged-error wakes it"; 16 is
/// the same order-of-magnitude budget the SPEC §4 Reliability
/// section uses for the per-tick scrape fan-out.
pub const TICK_BROADCAST_CAPACITY: usize = 16;

/// Wire mapping from metric name → ring column index. The aggregator's
/// `project_row` (see `crate::mon::collect::sample`) writes the host
/// sample's four scalars into columns 0..3; columns 4..15 are reserved
/// for Step 12+ projections. Names mirror the catalogue in SPEC §4
/// "Metrics" and `crates/sy-core/src/metrics.rs::CORE_METRICS` where
/// they overlap.
const METRIC_COLUMNS: &[(&str, usize)] = &[
    ("sy_cpu_util", 0),
    ("sy_mem_used_mib", 1),
    ("sy_swap_used_mib", 2),
    ("sy_load_avg_1m", 3),
];

/// Public projection of the metric names this aggregator understands,
/// in the wire order defined by [`METRIC_COLUMNS`]. Sy-mon Step 14's
/// MCP layer reads this to power the Levenshtein "did you mean?" hint
/// — keeping the list here means both surfaces drift together.
pub const KNOWN_METRICS: &[&str] = &[
    "sy_cpu_util",
    "sy_mem_used_mib",
    "sy_swap_used_mib",
    "sy_load_avg_1m",
];

/// Resolve a metric name to a ring column index. Returns the sorted
/// list of known names in the error message so an operator typo gets a
/// "did you mean X?"-shaped hint without us having to ship a fuzzy
/// matcher in this step (Step 14 layers Levenshtein on top in MCP).
fn column_for_metric(metric: &str) -> Result<usize, String> {
    if let Some((_, idx)) = METRIC_COLUMNS.iter().find(|(name, _)| *name == metric) {
        return Ok(*idx);
    }
    let known: Vec<&str> = METRIC_COLUMNS.iter().map(|(name, _)| *name).collect();
    Err(format!(
        "unknown metric {metric:?}; known metrics: {}",
        known.join(", ")
    ))
}

/// Aggregator's IPC server state. Cloning shares every handle.
#[derive(Clone)]
pub struct MonHandler {
    latest: LatestSnapshot,
    ring: Arc<Mutex<Ring>>,
    tick: broadcast::Sender<()>,
    system: Arc<SystemMethods>,
}

impl MonHandler {
    pub fn new(
        latest: LatestSnapshot,
        ring: Arc<Mutex<Ring>>,
        tick: broadcast::Sender<()>,
        system: Arc<SystemMethods>,
    ) -> Self {
        Self {
            latest,
            ring,
            tick,
            system,
        }
    }

    /// Dispatch a unary request. Streaming `subscribe` rides on
    /// [`stream_subscribe`] instead.
    fn handle_unary(&self, req: &Request) -> Response {
        if let Some(resp) = self.system.try_handle(req) {
            return resp;
        }
        match req.method.as_str() {
            METHOD_SNAPSHOT => self.handle_snapshot(req),
            METHOD_HISTORY => self.handle_history(req),
            METHOD_SUBSCRIBE => err_response(
                req.request_id,
                ErrorCode::BadRequest,
                format!("{METHOD_SUBSCRIBE} must be invoked over the streaming channel"),
            ),
            other => err_response(
                req.request_id,
                ErrorCode::BadRequest,
                format!("unknown method {other:?}; known: {SYSTEM_METHODS:?}"),
            ),
        }
    }

    fn handle_snapshot(&self, req: &Request) -> Response {
        let snap: Arc<SystemSnapshot> = self.latest.load();
        match serde_json::to_value(snap.as_ref()) {
            Ok(result) => ok_response(req.request_id, result),
            Err(e) => err_response(
                req.request_id,
                ErrorCode::Internal,
                format!("serialise SystemSnapshot: {e}"),
            ),
        }
    }

    fn handle_history(&self, req: &Request) -> Response {
        let params: MonHistoryParams = match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return err_response(
                    req.request_id,
                    ErrorCode::BadRequest,
                    format!("{METHOD_HISTORY} params: {e}"),
                );
            }
        };
        if let Err(reason) = params.validate() {
            return err_response(
                req.request_id,
                ErrorCode::BadRequest,
                format!("{METHOD_HISTORY} params: {reason}"),
            );
        }
        let idx = match column_for_metric(&params.metric) {
            Ok(i) => i,
            Err(msg) => {
                return err_response(req.request_id, ErrorCode::BadRequest, msg);
            }
        };
        // Hold the ring lock just long enough to copy the window out;
        // the lock is dropped before we touch serde_json.
        let (values, captured_at_ms) = {
            let ring = match self.ring.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    // Tick was mid-push; try a short blocking wait.
                    // History calls are best-effort over a quick window
                    // so a 1 ms wait is plenty.
                    self.ring.blocking_lock()
                }
            };
            let v = match ring.read_metric(idx, params.seconds as usize) {
                Ok(v) => v,
                Err(e) => {
                    drop(ring);
                    return err_response(
                        req.request_id,
                        ErrorCode::Internal,
                        format!("ring.read_metric: {e:#}"),
                    );
                }
            };
            drop(ring);
            let snap = self.latest.load();
            (v, snap.captured_at_ms)
        };
        let pairs = stamp_history(&values, captured_at_ms);
        let result = serde_json::json!({
            "metric": params.metric,
            "samples": pairs,
        });
        ok_response(req.request_id, result)
    }
}

/// Stamp a 1 Hz history window with descending-then-ascending
/// timestamps so the oldest sample lives at `captured_at_ms - (n-1)*1000`
/// and the most recent at `captured_at_ms`. The ring already returns
/// oldest-first, so the timestamps are just `captured_at_ms - offset`
/// reversed.
fn stamp_history(values: &[f32], captured_at_ms: u64) -> Vec<(u64, f32)> {
    if values.is_empty() {
        return Vec::new();
    }
    let n = values.len() as u64;
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let offset_ms = (n - 1 - i as u64) * 1000;
            let ts = captured_at_ms.saturating_sub(offset_ms);
            (ts, *v)
        })
        .collect()
}

/// One streaming `subscribe` connection. Returns when the client
/// disconnects or the broadcast sender goes away.
async fn stream_subscribe(
    req_id: Ulid,
    latest: LatestSnapshot,
    mut tick: broadcast::Receiver<()>,
    event_sink: &mut FramedWrite<tokio::net::unix::OwnedWriteHalf, EventCodec>,
) {
    loop {
        let recv = tick.recv().await;
        match recv {
            Ok(()) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Treat lag as a tick signal — we just emit the
                // latest snapshot regardless. The whole point of the
                // bounded channel is "if you're behind, you only owe
                // one frame".
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
        let snap = latest.load();
        let payload = match serde_json::to_value(snap.as_ref()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let evt = Event {
            schema_version: SCHEMA_VERSION,
            request_id: req_id,
            kind: EVENT_KIND_SNAPSHOT.into(),
            params: payload,
        };
        if event_sink.send(evt).await.is_err() {
            // Client dropped — exit the loop so the sentinel write
            // below is skipped (the socket is already gone).
            return;
        }
    }
    let _ = event_sink.send(Event::closed(req_id)).await;
}

/// Bind a sy-ipc UDS at `path`, accept connections, and dispatch each
/// to [`MonHandler`]. Returns only on listener error; per-connection
/// errors are swallowed so a misbehaving client cannot take the
/// aggregator down (mirrors `sy_ipc::Server::serve`).
pub async fn serve(handler: MonHandler, listener: UnixListener) -> std::io::Result<()> {
    let euid = rustix::process::geteuid().as_raw();
    loop {
        let (stream, _addr) = listener.accept().await?;
        if !is_peer_self(&stream, euid) {
            drop(stream);
            continue;
        }
        let h = handler.clone();
        tokio::spawn(async move { handle_client(stream, h).await });
    }
}

/// SPEC §4.2 origin check — peer uid must equal this process's
/// effective uid. Logic mirrors `sy_ipc::server::is_peer_self`, which
/// is private to that crate.
fn is_peer_self(stream: &UnixStream, euid: u32) -> bool {
    let Ok(cred) = stream.peer_cred() else {
        return false;
    };
    cred.uid() == euid
}

async fn handle_client(stream: UnixStream, handler: MonHandler) {
    let (reader, writer) = stream.into_split();
    let mut req_stream = FramedRead::new(reader, RequestCodec::default());
    let mut resp_sink = FramedWrite::new(writer, ResponseCodec::default());
    while let Some(decoded) = req_stream.next().await {
        let req = match decoded {
            Ok(r) => r,
            Err(_) => break,
        };
        if req.method == METHOD_SUBSCRIBE {
            // Ack first, then switch the writer to event-codec.
            let ack = ok_response(req.request_id, serde_json::json!({ "streaming": true }));
            if resp_sink.send(ack).await.is_err() {
                return;
            }
            let writer = resp_sink.into_inner();
            let mut event_sink = FramedWrite::new(writer, EventCodec::default());
            stream_subscribe(
                req.request_id,
                handler.latest.clone(),
                handler.tick.subscribe(),
                &mut event_sink,
            )
            .await;
            return;
        }
        let resp = handler.handle_unary(&req);
        if resp_sink.send(resp).await.is_err() {
            break;
        }
    }
}

/// Bind the aggregator's sy-ipc UDS at `path`, replacing any stale
/// socket file from a previous crash. Mode is tightened to 0600 after
/// bind per SPEC §4 Security (single-user host, no cross-uid access).
pub fn bind_uds(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() {
        // Best-effort unlink of a stale socket from a previous run.
        // Ignore errors here — the `bind()` below will surface a real
        // failure with the actual reason (EADDRINUSE / EACCES / …).
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    // 0600 on the socket file — only the owner can connect (SPEC §4
    // Security). The kernel also enforces the parent dir's mode
    // (`$XDG_RUNTIME_DIR` is 0700), so this is belt-and-braces.
    use std::os::unix::fs::PermissionsExt;
    let perm = std::fs::Permissions::from_mode(0o600);
    if let Err(e) = std::fs::set_permissions(path, perm) {
        // If chmod failed (e.g. tmpfs in a weird state), unbind and
        // surface the error rather than leaving a 0755 socket sitting
        // around.
        drop(listener);
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(listener)
}

fn ok_response(request_id: Ulid, result: serde_json::Value) -> Response {
    Response::Ok {
        schema_version: SCHEMA_VERSION,
        request_id,
        result,
        blob: None,
    }
}

fn err_response(request_id: Ulid, code: ErrorCode, message: String) -> Response {
    Response::Err {
        schema_version: SCHEMA_VERSION,
        request_id,
        error: ErrorBody {
            code,
            message,
            retry_after_ms: None,
            details: serde_json::Value::Null,
        },
    }
}

/// Construct a `SystemMethods` suitable for the aggregator. The
/// aggregator never runs domain methods of its own — every method on
/// the wire is either reserved (`system.{describe,health,cancel}`) or
/// belongs to the `system.mon.*` family this module dispatches. The
/// `daemon_methods` list therefore stays empty; `describe_methods`
/// returns the reserved set verbatim.
pub fn system_methods(
    build_info: sy_ipc::BuildInfo,
    cancel_registry: Arc<sy_ipc::CancelRegistry>,
) -> SystemMethods {
    use sy_ipc::{Capabilities, HealthSnapshot, HealthState};
    let health_fn: sy_ipc::HealthFn = Arc::new(|| HealthSnapshot {
        state: HealthState::Ready,
        status_line: "sy-mon-collect: aggregator running".into(),
        queue_depth: 0,
        warm_models: Vec::new(),
    });
    let mut caps = Capabilities::baseline();
    caps.streaming = true;
    SystemMethods::new(build_info, health_fn, cancel_registry, caps, Vec::new())
}

/// Spawn the aggregator's tick broadcast channel sized at
/// [`TICK_BROADCAST_CAPACITY`]. Kept here so the tick loop and the
/// IPC server share one construction site.
pub fn tick_channel() -> broadcast::Sender<()> {
    let (tx, _rx) = broadcast::channel(TICK_BROADCAST_CAPACITY);
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::time::Duration;
    use sy_core::mon::snapshot::SystemSnapshot;
    use sy_ipc::client::{CallOpts, Client};
    use sy_ipc::BuildInfo;
    use sy_ipc::CancelRegistry;
    use tempfile::tempdir;

    const RING_N_SECS: u32 = 32;
    const RING_N_METRICS: u32 = 16;
    /// Test fixture: one captured_at_ms value distinct enough to spot
    /// in a diff — base "May 22 2026 22:30:00 UTC" + 1234 ms.
    const FIXTURE_CAPTURED_AT_MS: u64 = 1_747_956_600_000;

    fn build_handler() -> (MonHandler, LatestSnapshot, StdArc<Mutex<Ring>>) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        // Keep the tempdir alive for the test by leaking it — the test
        // process exits when the test returns, so OS reclaim is fine.
        // `TempDir::keep` consumes the guard without cleanup.
        let _ = dir.keep();
        let ring = Ring::open_or_rebuild(&path, RING_N_SECS, RING_N_METRICS).expect("ring");
        let ring = StdArc::new(Mutex::new(ring));
        let latest = LatestSnapshot::new();
        let tx = tick_channel();
        let system = StdArc::new(system_methods(
            BuildInfo {
                name: "sy-mon-collect-test".into(),
                version: "0.0.0".into(),
                git_sha: "test".into(),
            },
            StdArc::new(CancelRegistry::new()),
        ));
        let handler = MonHandler::new(latest.clone(), ring.clone(), tx, system);
        (handler, latest, ring)
    }

    async fn start_server(handler: MonHandler) -> (tempfile::TempDir, tokio::task::JoinHandle<()>) {
        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("mon.sock");
        let listener = bind_uds(&sock).expect("bind");
        let h = tokio::spawn(async move {
            let _ = serve(handler, listener).await;
        });
        (dir, h)
    }

    /// SPEC §4 "CLI / MCP surface": `system.mon.snapshot` returns the
    /// most recent `SystemSnapshot` the aggregator published. The
    /// roundtrip is end-to-end: client connects, the daemon writes the
    /// snapshot, the client deserialises it back into the typed struct.
    #[tokio::test]
    async fn snapshot_roundtrip() {
        let (handler, latest, _ring) = build_handler();
        let snap = SystemSnapshot {
            captured_at_ms: FIXTURE_CAPTURED_AT_MS,
            ..SystemSnapshot::default()
        };
        latest.store(snap.clone());

        let (dir, server) = start_server(handler).await;
        let sock = dir.path().join("mon.sock");
        let mut client = Client::connect(&sock).await.expect("connect");
        let resp = client
            .call(METHOD_SNAPSHOT, serde_json::json!({}), CallOpts::default())
            .await
            .expect("call");
        match resp {
            Response::Ok { result, .. } => {
                let got: SystemSnapshot =
                    serde_json::from_value(result).expect("deserialise SystemSnapshot");
                assert_eq!(got, snap);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        server.abort();
    }

    /// Step 13 spec: three broadcast signals → three frames. After the
    /// client drops, the broadcast Sender's strong count drops; the
    /// per-connection task exits cleanly. Pin the per-tick frame count
    /// here so a regression that emitted a frame on connect (instead
    /// of waiting for the tick) is caught.
    #[tokio::test]
    async fn subscribe_emits_frame_per_tick() {
        let (handler, latest, _ring) = build_handler();
        let tick = handler.tick.clone();
        let (dir, server) = start_server(handler).await;
        let sock = dir.path().join("mon.sock");
        let mut client = Client::connect(&sock).await.expect("connect");
        // Initial ack.
        let ack = client
            .call(METHOD_SUBSCRIBE, serde_json::json!({}), CallOpts::default())
            .await
            .expect("call");
        match ack {
            Response::Ok { result, .. } => {
                assert_eq!(result, serde_json::json!({ "streaming": true }));
            }
            other => panic!("expected ack Ok, got {other:?}"),
        }
        let mut stream = client.into_event_stream();
        // Hand the broadcast time to attach a subscriber, then push
        // three ticks with distinct captured_at_ms values.
        tokio::task::yield_now().await;
        // Wait for at least one subscriber to attach. The
        // per-connection task calls `tick.subscribe()`; we spin until
        // the broadcast reports a receiver_count >= 1 (bounded by a
        // generous timeout so a regression doesn't hang the suite).
        let attach = tokio::time::timeout(Duration::from_secs(2), async {
            while tick.receiver_count() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        attach.expect("subscriber must attach within 2s");
        // Drive three ticks. Pump one at a time, reading the frame
        // it produced before scheduling the next so the test pins the
        // "one frame per tick signal" contract — the broadcast carries
        // the *trigger*, the per-tick payload is always
        // `latest.load()` at delivery time, so racing three sends
        // back-to-back can collapse to fewer wakeups (lagged drop).
        let mut seen = Vec::new();
        for ms in [1_000_u64, 2_000, 3_000] {
            let snap = SystemSnapshot {
                captured_at_ms: ms,
                ..SystemSnapshot::default()
            };
            latest.store(snap);
            tick.send(()).expect("send tick");
            let frame = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("frame within 2s")
                .expect("frame present")
                .expect("frame decodes");
            assert_eq!(frame.kind, EVENT_KIND_SNAPSHOT);
            let got: SystemSnapshot =
                serde_json::from_value(frame.params).expect("snapshot decodes");
            seen.push(got.captured_at_ms);
        }
        // Three ticks → three frames; each frame carried the latest
        // snapshot at delivery time so the captured_at_ms sequence
        // matches the store sequence.
        assert_eq!(seen, vec![1_000, 2_000, 3_000]);
        // Drop the client; the per-connection task observes broken
        // pipe on the next `send` and exits. We don't have direct
        // visibility into that exit, but the server task is still
        // running the accept loop, so just abort cleanly.
        drop(stream);
        server.abort();
    }

    /// Step 13 spec: pre-populate the ring with 10 known values,
    /// request `seconds=10`, get back exactly 10 `(ts, value)` pairs
    /// stamped one second apart and anchored on the latest snapshot's
    /// `captured_at_ms`.
    #[tokio::test]
    async fn history_returns_ring_samples() {
        let (handler, latest, ring) = build_handler();
        // Pre-populate column 0 (sy_cpu_util) with 10 known values.
        {
            let mut g = ring.lock().await;
            for i in 0..10_u32 {
                let mut row = vec![0.0_f32; RING_N_METRICS as usize];
                row[0] = (i as f32) * 10.0; // 0, 10, 20, ..., 90
                g.push(&row).expect("ring push");
            }
        }
        let snap = SystemSnapshot {
            captured_at_ms: FIXTURE_CAPTURED_AT_MS,
            ..SystemSnapshot::default()
        };
        latest.store(snap);

        let (dir, server) = start_server(handler).await;
        let sock = dir.path().join("mon.sock");
        let mut client = Client::connect(&sock).await.expect("connect");
        let resp = client
            .call(
                METHOD_HISTORY,
                serde_json::json!({ "metric": "sy_cpu_util", "seconds": 10 }),
                CallOpts::default(),
            )
            .await
            .expect("call");
        let Response::Ok { result, .. } = resp else {
            panic!("expected Ok, got {resp:?}");
        };
        let pairs: Vec<(u64, f32)> =
            serde_json::from_value(result["samples"].clone()).expect("samples decode");
        assert_eq!(pairs.len(), 10);
        // Oldest entry is captured_at_ms - 9_000; newest is captured_at_ms.
        assert_eq!(pairs[0].0, FIXTURE_CAPTURED_AT_MS - 9_000);
        assert_eq!(pairs[9].0, FIXTURE_CAPTURED_AT_MS);
        for (i, (_, v)) in pairs.iter().enumerate() {
            assert!(
                ((*v) - (i as f32) * 10.0).abs() < f32::EPSILON,
                "pair[{i}] = {v}; expected {}",
                (i as f32) * 10.0,
            );
        }
        server.abort();
    }

    /// SPEC §4 MCP schema: `seconds` ∈ [1, 600]. The handler must
    /// reject `seconds > 600` with `BadRequest` before touching the
    /// ring — otherwise a misbehaving client could ask for a window
    /// larger than the ring shape and crash the read path.
    #[tokio::test]
    async fn history_rejects_seconds_above_600() {
        let (handler, _latest, _ring) = build_handler();
        let (dir, server) = start_server(handler).await;
        let sock = dir.path().join("mon.sock");
        let mut client = Client::connect(&sock).await.expect("connect");
        let resp = client
            .call(
                METHOD_HISTORY,
                serde_json::json!({ "metric": "sy_cpu_util", "seconds": 601 }),
                CallOpts::default(),
            )
            .await
            .expect("call");
        match resp {
            Response::Err { error, .. } => {
                assert_eq!(error.code, ErrorCode::BadRequest);
                assert!(
                    error.message.contains("seconds"),
                    "error message must mention seconds; got {:?}",
                    error.message
                );
            }
            other => panic!("expected Err BadRequest, got {other:?}"),
        }
        // And seconds == 0 (below MON_SECONDS_MIN) also rejected.
        let resp = client
            .call(
                METHOD_HISTORY,
                serde_json::json!({ "metric": "sy_cpu_util", "seconds": 0 }),
                CallOpts::default(),
            )
            .await
            .expect("call");
        match resp {
            Response::Err { error, .. } => {
                assert_eq!(error.code, ErrorCode::BadRequest);
            }
            other => panic!("expected Err BadRequest for seconds=0, got {other:?}"),
        }
        server.abort();
    }

    /// Step 14 guard: the public `KNOWN_METRICS` projection MUST mirror
    /// the private `METRIC_COLUMNS` table (same names, same order). The
    /// MCP layer's "did you mean?" hint reads `KNOWN_METRICS`; a drift
    /// would silently leave the MCP error message naming a metric the
    /// aggregator no longer recognises.
    #[test]
    fn known_metrics_mirrors_metric_columns() {
        let from_columns: Vec<&str> = METRIC_COLUMNS.iter().map(|(name, _)| *name).collect();
        assert_eq!(KNOWN_METRICS, from_columns.as_slice());
    }
}
