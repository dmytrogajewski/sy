//! Unix-socket IPC for sy-aiplane: CLI/MCP ↔ daemon.
//!
//! On the wire, this module speaks IPC v1 (SPEC §4.2) via `sy-ipc`.
//! Two source-level flavours share the same socket:
//!
//!   * Fire-and-forget `Op` ([`send`]): map to a `knowledge.*` v1
//!     method, write a single envelope, close. Used for IndexNow,
//!     FullResync, Pause, etc. The daemon acks with an empty `Ok`;
//!     the client ignores it.
//!   * Request-response `Req`/`Resp` ([`request_with_priority`]): write one
//!     envelope, read one back. Used for Run / Search / SearchRerank
//!     so the CLI and MCP surfaces can offload all NPU inference to
//!     the daemon — the only process with the device bound.
//!
//! The legacy `Op`/`Req`/`Resp` types live on as a thin translation
//! layer while v1 lands across the codebase; new callers should
//! invoke `sy_ipc::Client::call` against the same method strings
//! directly (see [`KNOWLEDGE_METHODS`]).
//!
//! Missing socket = daemon not running; [`send`] is a silent no-op,
//! [`request_with_priority`] returns `IpcError::DaemonDown` so the caller can fall
//! back to in-process embedding.

use std::{
    env,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sy_core::{ErrorCode, Priority};
use sy_ipc::{
    BuildInfo, CancelRegistry, Capabilities, ErrorBody, Handler, HealthFn, HealthSnapshot,
    HealthState, RequestCodec, ResponseCodec, SystemMethods, SCHEMA_VERSION,
};
use tokio::sync::oneshot;
use tokio_util::codec::{FramedRead, FramedWrite};
use ulid::Ulid;

use super::error::AiplaneError;
use super::registry::{WorkloadInput, WorkloadKind, WorkloadOutput};
use super::scheduler::{Request as SchedRequest, Scheduler};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// Re-read sy.toml `[knowledge]` sources + schedule.
    RefreshSources,
    /// Run an incremental index pass right now.
    IndexNow,
    /// Drop the qdrant collection and re-embed everything.
    FullResync,
    /// Re-read schedule from sy.toml (subset of RefreshSources).
    ReloadSchedule,
    /// Re-walk discover roots + shallow-home for `qdr.toml` manifests; diff
    /// the active set, register/unregister watchers + qdrant points.
    RescanDiscovery,
    /// Stop firing scheduled / FS-tickle / IPC-IndexNow passes until
    /// `Resume`. User-driven `sy knowledge index/sync/search` calls
    /// (which run in the CLI process, not the daemon) bypass this.
    Pause,
    /// Resume from a paused state. Triggers a single catch-up pass.
    Resume,
    /// Idempotent toggle (used by the waybar middle-click handler).
    TogglePause,
    /// Cooperatively cancel any in-flight pass. Daemon stays paused if it
    /// was paused before. Files already embedded keep their qdrant points.
    Cancel,
    /// Graceful shutdown.
    Shutdown,
}

/// Pre-search payload filter (REQ-2). Compiled into a qdrant `Filter`
/// downstream (Step 7); here it is pure wire format. Every field is
/// optional/empty by default so an absent filter serializes compactly
/// and pre-Step-6 payloads still deserialize.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchFilter {
    /// Inclusive lower bound, ISO-8601 / RFC-3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_from: Option<String>,
    /// Inclusive upper bound, ISO-8601 / RFC-3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_to: Option<String>,
    /// Sender match set (any-of).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<String>,
    /// `SourceKind` kebab strings (any-of).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kind: Vec<String>,
    /// Source names that must be present (must).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_sources: Vec<String>,
    /// Source names that must be absent (must-not).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_sources: Vec<String>,
    /// `SourceKind` kebab strings that must be absent (must-not). Carries
    /// the REQ-1 default-exclude of `claude-transcripts`, injected by the
    /// CLI/MCP boundary unless the caller opts that kind in. Additive +
    /// defaulted so pre-Step-11 payloads still deserialize.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_kinds: Vec<String>,
}

/// Request-response op. Distinct from `Op` because callers wait for
/// the daemon's reply on the same UnixStream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "kebab-case")]
pub enum Req {
    /// Generic workload dispatch. The daemon validates the input
    /// variant matches the workload's expected shape and returns
    /// `Resp::Run { output }` or `Resp::Error`.
    Run {
        workload: WorkloadKind,
        input: WorkloadInput,
    },
    /// Composite: embed `query` via the Embed workload, then
    /// qdrant top-k cosine search. Kept as a single round-trip so
    /// search consumers don't pay 2× IPC.
    Search {
        query: String,
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// SPEC §4.3 scheduler class for the embed step. Older
        /// callers default to `Interactive`. The bridge stamps this
        /// from the v1 envelope's `priority` field at translation
        /// time.
        #[serde(default = "default_search_priority")]
        priority: Priority,
        /// REQ-2 pre-search filter. `None` ⇒ unfiltered (back-compat).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<SearchFilter>,
        /// REQ-6 abstain cutoff in `[0,1]`. `None` ⇒ server default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abstain_threshold: Option<f32>,
    },
    /// Two-stage retrieval: embed → qdrant top-`candidates` →
    /// bge-reranker cross-encoder scores every (query, doc) pair →
    /// truncate to `limit`. Done daemon-side so the client doesn't
    /// pay one IPC per pair, and so the NPU mutex is held across the
    /// whole rerank pass (no re-entry cost between pairs).
    SearchRerank {
        query: String,
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        /// Top-N pulled from qdrant before reranking. Default 30 in
        /// the CLI / MCP surfaces; tune up for higher recall on long
        /// tails, down for tighter latency.
        candidates: usize,
        /// SPEC §4.3 scheduler class for the embed step. Same
        /// semantics as on [`Req::Search`].
        #[serde(default = "default_search_priority")]
        priority: Priority,
        /// REQ-2 pre-search filter. `None` ⇒ unfiltered (back-compat).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<SearchFilter>,
        /// REQ-6 abstain cutoff in `[0,1]`. `None` ⇒ server default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abstain_threshold: Option<f32>,
    },
    /// REQ-10 fetch-by-id: resolve a single chunk's full (uncapped)
    /// text + payload by the stable `chunk_id` a bounded search result
    /// carries. `chunk_id` is the qdrant point id (blake3-derived in
    /// `chunk::point_id`); the daemon answers with [`Resp::Chunk`].
    GetChunk { chunk_id: String },
}

/// Default priority for legacy [`Req::Search`] / [`Req::SearchRerank`]
/// callers that don't stamp the field. The bridge overrides this
/// from the v1 envelope at translation time; only direct legacy-Req
/// constructors (CLI pre-Step-10) land on this default.
fn default_search_priority() -> Priority {
    Priority::Interactive
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "kebab-case")]
pub enum Resp {
    Run {
        output: WorkloadOutput,
    },
    Search {
        hits: Vec<HitRow>,
        /// REQ-6 calibrated confidence in `[0,1]`. Computed in Step 12;
        /// here it is plumbing-only. Legacy payloads without the field
        /// deserialize to the neutral default `1.0` (uncalibrated /
        /// not-abstained) so a Step-6 daemon and an older CLI interop.
        #[serde(default = "default_search_confidence")]
        confidence: f32,
        /// REQ-6 abstain flag. `false` ⇒ a normal (non-abstained)
        /// response; legacy payloads default to `false`.
        #[serde(default)]
        abstained: bool,
    },
    /// REQ-10 fetch-by-id reply: the full (uncapped) chunk for a
    /// `Req::GetChunk`. `chunk` is `None` when no point matched the id.
    Chunk {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chunk: Option<ChunkRow>,
    },
    Error {
        msg: String,
    },
}

/// Full (uncapped) chunk returned by [`Resp::Chunk`] for a fetch-by-id.
/// Mirrors the SPEC §4 `knowledge_get_chunk` shape — the `payload` carries
/// the source-kind metadata, `text` is the complete chunk text (never the
/// MCP-capped form).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRow {
    pub chunk_id: String,
    pub file_path: String,
    pub chunk_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    pub text: String,
}

/// Neutral confidence for [`Resp::Search`] payloads that predate the
/// Step-6 wire format (and for producers that don't calibrate yet).
pub fn default_search_confidence() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HitRow {
    /// Final score callers should rank by. Cosine similarity for
    /// `Req::Search`; rerank sigmoid for `Req::SearchRerank`.
    pub score: f32,
    /// Stable qdrant point id (blake3-derived, `chunk::point_id`). REQ-10:
    /// a bounded search result carries this so the agent can fetch the
    /// full chunk on demand via `Req::GetChunk`. Defaulted so a pre-Step-14
    /// daemon's hits still deserialize.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub chunk_id: String,
    pub file_path: String,
    pub chunk_index: u32,
    pub chunk_text: String,
    /// Pre-rerank cosine score from qdrant. `None` on the embed-only
    /// path; `Some(_)` only when the daemon reranked the hit so UIs
    /// can show "moved from rank N → M" later if useful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_score: Option<f32>,
}

#[derive(Debug)]
pub enum IpcError {
    /// `connect()` failed — no socket, refused, or removed. Callers
    /// translate this into an in-process fallback.
    DaemonDown,
    /// Wire-level failure (read/write/serde) after the connection was
    /// established. Carries the underlying anyhow chain.
    Wire(anyhow::Error),
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpcError::DaemonDown => write!(f, "sy-aiplane daemon not reachable"),
            IpcError::Wire(e) => write!(f, "ipc: {e}"),
        }
    }
}

impl std::error::Error for IpcError {}

pub fn socket_path() -> PathBuf {
    if let Ok(d) = env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("sy-knowledge.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/sy-knowledge.sock"))
}

// ---------------------------------------------------------------- client side

/// Default per-call deadline (ms) carried in the v1 envelope. 30 s
/// matches the old line-JSON `request()` timeout — covers the cold
/// NPU wake-up + compile-cache hit on the first call.
const DEFAULT_DEADLINE_MS: u64 = 30_000;

/// Fire-and-forget op. Silently succeeds if the daemon isn't listening.
pub fn send(op: &Op) -> Result<()> {
    let path = socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let (method, params) = op_to_v1(op);
    let bytes = serde_json::to_vec(&build_v1_request(method, params))?;
    write_v1_frame(&mut stream, &bytes)?;
    // Drop without reading the ack — the daemon has already forwarded
    // to the ops channel by the time it tries to write a response, so
    // there's nothing to wait for.
    Ok(())
}

/// Synchronous request-response. Builds a v1 envelope, writes one
/// frame, reads one back, translates to the legacy `Resp` enum.
/// Carries the caller's `Priority` in the v1 envelope — used by
/// `sy knowledge search --priority X` so the daemon admits the
/// embed step through the scheduler at the requested class
/// (SPEC §4.7). Internal callers that don't care about priority
/// pass `Priority::Interactive`.
pub fn request_with_priority(req: &Req, priority: Priority) -> std::result::Result<Resp, IpcError> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|_| IpcError::DaemonDown)?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let (method, params) = req_to_v1(req);
    let mut envelope = build_v1_request(method, params);
    envelope.priority = priority;
    let bytes = serde_json::to_vec(&envelope).map_err(wire_err)?;
    write_v1_frame(&mut stream, &bytes).map_err(wire_err)?;
    let payload = read_v1_frame(&mut stream).map_err(wire_err)?;
    let v1: sy_ipc::Response = serde_json::from_slice(&payload).map_err(wire_err)?;
    v1_response_to_legacy(method, v1).map_err(IpcError::Wire)
}

fn wire_err<E: Into<anyhow::Error>>(e: E) -> IpcError {
    IpcError::Wire(e.into())
}

fn build_v1_request(method: &str, params: serde_json::Value) -> sy_ipc::Request {
    sy_ipc::Request {
        schema_version: SCHEMA_VERSION,
        request_id: Ulid::new(),
        trace_id: None,
        parent_span_id: None,
        deadline_ms: Some(DEFAULT_DEADLINE_MS),
        priority: Priority::Interactive,
        method: method.into(),
        params,
    }
}

fn write_v1_frame(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
    let len =
        u32::try_from(payload.len()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

fn read_v1_frame(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

fn v1_response_to_legacy(method: &str, v1: sy_ipc::Response) -> Result<Resp> {
    match v1 {
        sy_ipc::Response::Ok { result, .. } => match method {
            M_RUN => {
                let output: WorkloadOutput =
                    serde_json::from_value(result.get("output").cloned().unwrap_or_default())
                        .context("decode Resp::Run output")?;
                Ok(Resp::Run { output })
            }
            M_SEARCH | M_SEARCH_RERANK => {
                let hits: Vec<HitRow> =
                    serde_json::from_value(result.get("hits").cloned().unwrap_or_default())
                        .context("decode Resp::Search hits")?;
                let confidence = result
                    .get("confidence")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or_else(default_search_confidence);
                let abstained = result
                    .get("abstained")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(Resp::Search {
                    hits,
                    confidence,
                    abstained,
                })
            }
            M_GET_CHUNK => {
                let chunk: Option<ChunkRow> = serde_json::from_value(
                    result
                        .get("chunk")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                )
                .context("decode Resp::Chunk")?;
                Ok(Resp::Chunk { chunk })
            }
            _ => {
                // Fire-and-forget ack reaching here is a bug: send()
                // doesn't read responses. Surface as a synthetic empty
                // run so the type signature is honoured; the value is
                // never consumed by real callers.
                Ok(Resp::Run {
                    output: WorkloadOutput::Text {
                        text: String::new(),
                    },
                })
            }
        },
        sy_ipc::Response::Err { error, .. } => Ok(Resp::Error {
            msg: format!("{}: {}", error.code, error.message),
        }),
    }
}

// ---------------------------------------------------------------- server side

/// Bind the v1 listener at [`socket_path`] and dispatch into the
/// supplied channels:
///
///   * `ops_tx` receives every `knowledge.*` fire-and-forget op.
///     The daemon's main loop picks them up next tick.
///   * `req_tx` receives every `knowledge.{run,search,search_rerank}`
///     request paired with a `oneshot` sender the worker writes the
///     `Resp` back through. The bridge translates the `Resp` to a
///     v1 `Response` before sending on the wire.
///
/// `daemon_cancel` is the legacy cooperative-cancel `AtomicBool` the
/// daemon's `RunCtx` checks during long passes. Every `system.cancel`
/// arriving on the v1 listener trips this atomic so an in-flight
/// `knowledge.full_resync` bails within the next iteration.
///
/// Connections whose first byte is `{` are diagnosed as legacy
/// line-JSON and answered with a JSON-line `IncompatibleSchema`
/// error (SPEC §3.4 "no backward-compat for unversioned IPC").
pub fn serve(
    ops_tx: mpsc::Sender<Op>,
    req_tx: mpsc::Sender<(Req, oneshot::Sender<Resp>)>,
    daemon_cancel: Arc<AtomicBool>,
) -> Result<()> {
    serve_with_dispatch(ops_tx, req_tx, daemon_cancel, Arc::new(SupervisorDispatch))
}

/// Variant of [`serve`] that takes an explicit [`AiplaneDispatch`].
/// Tests use this to inject a hermetic `FakeWorkload`-backed
/// dispatcher; production uses [`serve`] which wires the supervisor.
pub fn serve_with_dispatch(
    ops_tx: mpsc::Sender<Op>,
    req_tx: mpsc::Sender<(Req, oneshot::Sender<Resp>)>,
    daemon_cancel: Arc<AtomicBool>,
    aiplane: Arc<dyn AiplaneDispatch>,
) -> Result<()> {
    let path = socket_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Bind synchronously with std so the socket exists by the time
    // `serve()` returns — preserves the contract callers (and the
    // hermetic tests) rely on. The listener is handed off to tokio
    // inside the worker thread via `from_std`.
    let std_listener = std::os::unix::net::UnixListener::bind(&path)
        .with_context(|| format!("bind {}", path.display()))?;
    std_listener
        .set_nonblocking(true)
        .with_context(|| "set_nonblocking on listener")?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));

    let bridge = Arc::new(KnowledgeBridge::new(ops_tx, req_tx, daemon_cancel, aiplane));
    thread::Builder::new()
        .name("sy-knowledge-ipc-v1".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("v1 tokio runtime");
            rt.block_on(async move {
                let listener =
                    tokio::net::UnixListener::from_std(std_listener).expect("convert std listener");
                run_v1_accept_loop(listener, bridge).await;
            });
        })
        .context("spawn v1 listener thread")?;
    Ok(())
}

async fn run_v1_accept_loop<H: Handler>(listener: tokio::net::UnixListener, handler: Arc<H>) {
    let euid = rustix::process::geteuid().as_raw();
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::error!(
                    target: "sy::aiplane::ipc",
                    error = %e,
                    "v1 accept failed"
                );
                continue;
            }
        };
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == euid => {}
            _ => {
                drop(stream);
                continue;
            }
        }
        let h = Arc::clone(&handler);
        tokio::spawn(serve_one_connection(stream, h));
    }
}

async fn serve_one_connection<H: Handler>(stream: tokio::net::UnixStream, handler: Arc<H>) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let (reader, writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let initial = match buf_reader.fill_buf().await {
        Ok(b) if !b.is_empty() => b[0],
        _ => return,
    };
    if initial == b'{' || initial == b'[' || initial.is_ascii_whitespace() {
        // Looks like a legacy line-JSON envelope. Hard-cutover
        // anti-goal per SPEC §3.4 — answer with a v1-shaped JSON line
        // carrying `IncompatibleSchema` and drop.
        reject_legacy_envelope(buf_reader, writer).await;
        return;
    }
    let mut req_stream = FramedRead::new(buf_reader, RequestCodec::default());
    let mut resp_sink = FramedWrite::new(writer, ResponseCodec::default());
    while let Some(decoded) = req_stream.next().await {
        let req = match decoded {
            Ok(r) => r,
            Err(_) => break,
        };
        let resp = handler.handle(req).await;
        if resp_sink.send(resp).await.is_err() {
            break;
        }
    }
}

async fn reject_legacy_envelope(
    mut reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Drain whatever the legacy caller wrote so its write half can
    // finish; we don't need to parse it.
    let mut sink = [0u8; 4096];
    let _ = reader.read(&mut sink).await;
    let err = sy_ipc::Response::Err {
        schema_version: SCHEMA_VERSION,
        request_id: Ulid::new(),
        error: ErrorBody {
            code: ErrorCode::IncompatibleSchema,
            message: "legacy line-JSON IPC is no longer accepted; speak sy-ipc v1".into(),
            retry_after_ms: None,
            details: serde_json::Value::Null,
        },
    };
    if let Ok(line) = serde_json::to_string(&err) {
        let _ = writer.write_all(line.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
        let _ = writer.flush().await;
    }
    let _ = writer.shutdown().await;
}

/// Backend dispatch for `aiplane.{run,batch}` requests. Production
/// wires this to `aiplane::supervisor::current()`; tests inject a
/// `FakeWorkload`-driven dispatcher so the bridge can be exercised
/// without the multi-process supervisor up.
pub trait AiplaneDispatch: Send + Sync + 'static {
    fn run(&self, workload: WorkloadKind, input: WorkloadInput) -> Result<WorkloadOutput>;
    fn batch(
        &self,
        workload: WorkloadKind,
        inputs: Vec<WorkloadInput>,
    ) -> Result<Vec<WorkloadOutput>>;
    /// SPEC §4.3 / ROADMAP Step 4: cooperatively cancel any inflight
    /// `run`/`batch` on `workload` whose request_id matches. Default
    /// impl is a no-op for the test fakes; the production
    /// `SupervisorDispatch` plumbs into [`super::supervisor::Supervisor::cancel`]
    /// which fires `WorkerReq::Cancel` and arms the 500 ms SIGKILL
    /// guard.
    fn cancel(&self, _workload: WorkloadKind, _request_id: ulid::Ulid) -> Result<()> {
        Ok(())
    }
}

/// Production `AiplaneDispatch`: delegates to the running aiplane
/// supervisor. Returns an error if the supervisor isn't initialised
/// (the daemon refuses to boot in that state, so this is a paranoia
/// guard for callers that hold a bridge handle past shutdown).
pub struct SupervisorDispatch;

impl AiplaneDispatch for SupervisorDispatch {
    fn run(&self, workload: WorkloadKind, input: WorkloadInput) -> Result<WorkloadOutput> {
        let sup = super::supervisor::current()
            .ok_or_else(|| anyhow::anyhow!("aiplane supervisor not running"))?;
        sup.run_batch(workload, vec![input]).and_then(|mut outs| {
            outs.pop()
                .ok_or_else(|| anyhow::anyhow!("supervisor returned empty batch"))
        })
    }

    fn batch(
        &self,
        workload: WorkloadKind,
        inputs: Vec<WorkloadInput>,
    ) -> Result<Vec<WorkloadOutput>> {
        let sup = super::supervisor::current()
            .ok_or_else(|| anyhow::anyhow!("aiplane supervisor not running"))?;
        sup.run_batch(workload, inputs)
    }

    fn cancel(&self, workload: WorkloadKind, request_id: ulid::Ulid) -> Result<()> {
        let sup = super::supervisor::current()
            .ok_or_else(|| anyhow::anyhow!("aiplane supervisor not running"))?;
        sup.cancel(workload, request_id)
    }
}

struct KnowledgeBridge {
    ops_tx: mpsc::Sender<Op>,
    req_tx: mpsc::Sender<(Req, oneshot::Sender<Resp>)>,
    daemon_cancel: Arc<AtomicBool>,
    cancel_registry: Arc<CancelRegistry>,
    aiplane: Arc<dyn AiplaneDispatch>,
    /// Strict-priority scheduler (SPEC §4.3) sitting between the
    /// bridge and the workload backend. `aiplane.run` requests admit
    /// here; the paired `Dispatcher` thread (held alive by
    /// `_dispatcher_handle`) drains the queues in priority order and
    /// invokes `self.aiplane.run` on each pulled request.
    scheduler: Arc<Scheduler>,
    /// In-flight `request_id → WorkloadKind` registry (SPEC §4.2 /
    /// arch-aiplane-scheduler Step 7). `handle_aiplane_run` inserts
    /// after admit, removes when the oneshot fires; the
    /// `aiplane.cancel` handler reads it to resolve the workload
    /// kind so callers can fire `sy aiplane cancel <request_id>`
    /// without naming the workload.
    inflight_kinds: Arc<std::sync::Mutex<std::collections::HashMap<Ulid, WorkloadKind>>>,
    /// `JoinHandle` of the scheduler dispatcher thread. Held so the
    /// thread stays alive for the bridge's lifetime; dropping the
    /// bridge drops the Scheduler senders and the dispatcher exits
    /// cleanly when its `select!` sees all four channels disconnect.
    _dispatcher_handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    system: SystemMethods,
}

impl KnowledgeBridge {
    fn new(
        ops_tx: mpsc::Sender<Op>,
        req_tx: mpsc::Sender<(Req, oneshot::Sender<Resp>)>,
        daemon_cancel: Arc<AtomicBool>,
        aiplane: Arc<dyn AiplaneDispatch>,
    ) -> Self {
        let cancel_registry = Arc::new(CancelRegistry::new());
        let (scheduler, dispatcher) = crate::aiplane::scheduler::Scheduler::new();
        let scheduler = Arc::new(scheduler);
        let dispatcher_handle = dispatcher.run(Arc::clone(&aiplane));
        let health_fn: HealthFn = Arc::new(|| HealthSnapshot {
            state: HealthState::Ready,
            status_line: "ready".into(),
            queue_depth: 0,
            warm_models: Vec::new(),
        });
        // `system.describe.methods` surfaces both namespaces — every
        // method the bridge actually handles. Order is deduplicated +
        // sorted by SystemMethods so the wire shape is stable.
        let advertised: Vec<String> = KNOWLEDGE_METHODS
            .iter()
            .chain(AIPLANE_METHODS.iter())
            .map(|s| (*s).to_string())
            .collect();
        let system = SystemMethods::new(
            BuildInfo {
                name: "sy-knowledge".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                git_sha: option_env!("SY_GIT_SHA").unwrap_or("").into(),
            },
            health_fn,
            Arc::clone(&cancel_registry),
            Capabilities::baseline(),
            advertised,
        );
        // Install the scheduler handle as the process-wide reader.
        // `current_scheduler()` is how the daemon's status writer
        // pulls live per-class queue depths into `Status.queue_depths`
        // (SPEC §4.3 / arch-aiplane-scheduler Step 6). Best-effort —
        // a second install (test re-entry, daemon-in-thread harness)
        // silently keeps the first; serve_with_dispatch wraps each
        // bridge in its own scheduler so a stale handle is still a
        // live one.
        let _ = CURRENT_SCHEDULER.set(Arc::clone(&scheduler));
        Self {
            ops_tx,
            req_tx,
            daemon_cancel,
            cancel_registry,
            aiplane,
            scheduler,
            inflight_kinds: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            _dispatcher_handle: std::sync::Mutex::new(Some(dispatcher_handle)),
            system,
        }
    }
}

/// Process-wide handle to the running bridge's [`Scheduler`]. Set
/// once by `KnowledgeBridge::new()` at daemon startup; consumers
/// (the status writer, `sy aiplane status --json`) read it via
/// [`current_scheduler`] and fall back to the default all-zero
/// queue-depths map when the bridge isn't up yet.
static CURRENT_SCHEDULER: std::sync::OnceLock<Arc<Scheduler>> = std::sync::OnceLock::new();

/// Returns the running bridge's scheduler if one was installed,
/// otherwise `None`. Cheap; no locking. The handle is `Arc`-cloned
/// so callers don't have to hold the OnceLock guard.
pub fn current_scheduler() -> Option<Arc<Scheduler>> {
    CURRENT_SCHEDULER.get().cloned()
}

/// Synchronous "admit + wait" through the bridge's scheduler. Used
/// by daemon-side sync handlers (knowledge.search_rerank's
/// embed pass, MCP tool dispatch) that need priority-aware
/// scheduling without rewriting themselves async. Returns
/// `AiplaneError::WorkloadFailed` when no scheduler is installed —
/// callers in pre-daemon paths (CLI fallback, fresh boot) should
/// detect this and fall back to a direct `Supervisor::run_batch`.
pub fn admit_blocking(
    workload: WorkloadKind,
    input: WorkloadInput,
    priority: sy_core::Priority,
) -> Result<WorkloadOutput, super::error::AiplaneError> {
    use super::error::AiplaneError;
    use tokio_util::sync::CancellationToken;
    use ulid::Ulid;
    let scheduler = current_scheduler().ok_or_else(|| {
        AiplaneError::WorkloadFailed(anyhow::anyhow!("aiplane scheduler not running"))
    })?;
    let (req, rx) = SchedRequest::new(
        Ulid::new(),
        workload,
        input,
        priority,
        None,
        CancellationToken::new(),
    );
    scheduler.admit(req)?;
    rx.blocking_recv()
        .map_err(|_| AiplaneError::WorkloadFailed(anyhow::anyhow!("scheduler rx dropped")))?
}

impl Handler for KnowledgeBridge {
    async fn handle(&self, req: sy_ipc::Request) -> sy_ipc::Response {
        // SPEC §4.2: every daemon answers system.*. The bridge
        // intercepts `system.cancel` to also flip the daemon's
        // cooperative cancel atomic — the same atomic an `Op::Cancel`
        // sets — so in-flight passes (`knowledge.full_resync`,
        // `knowledge.index_now`) bail mid-pass.
        if req.method == "system.cancel" {
            let resp = self
                .system
                .try_handle(&req)
                .expect("SystemMethods handles system.cancel");
            self.daemon_cancel.store(true, Ordering::SeqCst);
            return resp;
        }
        if let Some(resp) = self.system.try_handle(&req) {
            return resp;
        }
        // Aiplane v1 namespace. `aiplane.cancel` is just a namespaced
        // wrapper over `system.cancel` semantics; `aiplane.run` and
        // `aiplane.batch` register with the cancel registry before
        // dispatching to the workload backend (SPEC §4.2 step 1).
        match req.method.as_str() {
            M_AIPLANE_RUN => return self.handle_aiplane_run(req).await,
            M_AIPLANE_BATCH => return self.handle_aiplane_batch(req).await,
            M_AIPLANE_CANCEL => return self.handle_aiplane_cancel(req),
            _ => {}
        }
        if let Some(op) = try_method_to_op(&req.method, &req.params) {
            let _ = self.ops_tx.send(op);
            return v1_ok_empty(req.request_id);
        }
        match try_method_to_req(&req.method, &req.params) {
            Ok(Some(mut legacy_req)) => {
                // SPEC §4.3 / Step 10: stamp the v1 envelope's
                // priority onto the legacy `Req::Search` and
                // `Req::SearchRerank` variants. `try_method_to_req`
                // can only see params; the priority lives in the
                // envelope. Other Req variants ignore the field.
                match &mut legacy_req {
                    Req::Search { priority, .. } | Req::SearchRerank { priority, .. } => {
                        *priority = req.priority;
                    }
                    Req::Run { .. } | Req::GetChunk { .. } => {}
                }
                let (tx, rx) = oneshot::channel();
                if self.req_tx.send((legacy_req, tx)).is_err() {
                    return v1_err(req.request_id, ErrorCode::Internal, "req worker offline");
                }
                match rx.await {
                    Ok(resp) => legacy_resp_to_v1(req.request_id, resp),
                    Err(_) => v1_err(req.request_id, ErrorCode::Internal, "req worker dropped"),
                }
            }
            Ok(None) => v1_err(
                req.request_id,
                ErrorCode::BadRequest,
                &format!("unknown method: {}", req.method),
            ),
            Err(e) => v1_err(
                req.request_id,
                ErrorCode::BadRequest,
                &format!("bad params: {e}"),
            ),
        }
    }
}

/// Aiplane request params shared by `aiplane.run` and the singleton
/// element of `aiplane.batch`. The optional `sleep_ms` is honoured
/// by the bridge — not the workload — so even non-NPU dispatchers
/// (FakeWorkload, CPU stubs) gain a cancellable budget for tests
/// without touching every workload impl.
#[derive(serde::Deserialize)]
struct AiplaneRunParams {
    workload: WorkloadKind,
    input: WorkloadInput,
    #[serde(default)]
    sleep_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
struct AiplaneBatchParams {
    workload: WorkloadKind,
    inputs: Vec<WorkloadInput>,
    #[serde(default)]
    sleep_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
struct AiplaneCancelParams {
    target_request_id: Ulid,
    /// Which workload's inflight call to abort. When supplied (and the
    /// daemon is running a real `SupervisorDispatch`), the bridge
    /// signals the worker via `WorkerReq::Cancel`; the worker either
    /// honours [`super::registry::Workload::try_cancel`] within the
    /// SPEC §4.3 500 ms guard or gets SIGKILLed. Optional because the
    /// scheduler-side cancel (the `CancelRegistry` token) suffices for
    /// requests that haven't started running yet — Step 5 will land
    /// the registry that lets callers omit this field.
    #[serde(default)]
    workload: Option<WorkloadKind>,
}

impl KnowledgeBridge {
    async fn handle_aiplane_run(&self, req: sy_ipc::Request) -> sy_ipc::Response {
        let p: AiplaneRunParams = match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return v1_err(
                    req.request_id,
                    ErrorCode::BadRequest,
                    &format!("aiplane.run params: {e}"),
                );
            }
        };
        let guard = self.cancel_registry.register(req.request_id);
        let token = guard.token();
        // SPEC §4.2 step 2: register the workload with the inflight
        // registry *before* the optional pre-admit sleep so a
        // fast-arriving `aiplane.cancel { target_request_id }` (no
        // workload field) can still resolve it. Removed in every
        // exit path below (sleep-cancel, post-sleep token check,
        // admit failure, response).
        self.inflight_kinds
            .lock()
            .expect("inflight_kinds poisoned")
            .insert(req.request_id, p.workload);
        if let Some(ms) = p.sleep_ms {
            // Cancellable budget so the cancel test can interrupt
            // an otherwise instant FakeWorkload before it runs.
            let sleep = tokio::time::sleep(Duration::from_millis(ms));
            tokio::pin!(sleep);
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    self.inflight_kinds
                        .lock()
                        .expect("inflight_kinds poisoned")
                        .remove(&req.request_id);
                    return v1_err(req.request_id, ErrorCode::Cancelled, "aiplane.run cancelled");
                }
                _ = &mut sleep => {}
            }
        }
        if token.is_cancelled() {
            self.inflight_kinds
                .lock()
                .expect("inflight_kinds poisoned")
                .remove(&req.request_id);
            return v1_err(
                req.request_id,
                ErrorCode::Cancelled,
                "aiplane.run cancelled",
            );
        }
        // SPEC §4.3 admission: build a Request tagged with the v1
        // envelope's `Priority`, then `Scheduler::admit` lands it in
        // the right bounded queue. The scheduler dispatcher (running
        // on a dedicated thread) pulls in strict-priority order and
        // invokes `self.aiplane.run` for us.
        let (sched_req, rx) = SchedRequest::new(
            req.request_id,
            p.workload,
            p.input,
            req.priority,
            None,
            token.clone(),
        );
        if let Err(e) = self.scheduler.admit(sched_req) {
            self.inflight_kinds
                .lock()
                .expect("inflight_kinds poisoned")
                .remove(&req.request_id);
            return aiplane_error_to_v1(req.request_id, e);
        }
        let outcome = tokio::select! {
            biased;
            _ = token.cancelled() => {
                self.inflight_kinds
                    .lock()
                    .expect("inflight_kinds poisoned")
                    .remove(&req.request_id);
                return v1_err(req.request_id, ErrorCode::Cancelled, "aiplane.run cancelled");
            }
            r = rx => r,
        };
        self.inflight_kinds
            .lock()
            .expect("inflight_kinds poisoned")
            .remove(&req.request_id);
        drop(guard);
        match outcome {
            Ok(Ok(output)) => sy_ipc::Response::Ok {
                schema_version: SCHEMA_VERSION,
                request_id: req.request_id,
                result: serde_json::json!({ "output": output }),
                blob: None,
            },
            Ok(Err(e)) => aiplane_error_to_v1(req.request_id, e),
            Err(_) => v1_err(
                req.request_id,
                ErrorCode::Internal,
                "aiplane dispatcher dropped response oneshot",
            ),
        }
    }

    async fn handle_aiplane_batch(&self, req: sy_ipc::Request) -> sy_ipc::Response {
        let p: AiplaneBatchParams = match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return v1_err(
                    req.request_id,
                    ErrorCode::BadRequest,
                    &format!("aiplane.batch params: {e}"),
                );
            }
        };
        let guard = self.cancel_registry.register(req.request_id);
        let token = guard.token();
        if let Some(ms) = p.sleep_ms {
            let sleep = tokio::time::sleep(Duration::from_millis(ms));
            tokio::pin!(sleep);
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    return v1_err(req.request_id, ErrorCode::Cancelled, "aiplane.batch cancelled");
                }
                _ = &mut sleep => {}
            }
        }
        let dispatcher = Arc::clone(&self.aiplane);
        let workload = p.workload;
        let inputs = p.inputs;
        let dispatch = tokio::task::spawn_blocking(move || dispatcher.batch(workload, inputs));
        let outcome = tokio::select! {
            biased;
            _ = token.cancelled() => {
                return v1_err(req.request_id, ErrorCode::Cancelled, "aiplane.batch cancelled");
            }
            r = dispatch => r,
        };
        drop(guard);
        match outcome {
            Ok(Ok(outputs)) => sy_ipc::Response::Ok {
                schema_version: SCHEMA_VERSION,
                request_id: req.request_id,
                result: serde_json::json!({ "outputs": outputs }),
                blob: None,
            },
            Ok(Err(e)) => v1_err(req.request_id, ErrorCode::Internal, &format!("{e:#}")),
            Err(join_err) => v1_err(
                req.request_id,
                ErrorCode::Internal,
                &format!("aiplane dispatch task panicked: {join_err}"),
            ),
        }
    }

    fn handle_aiplane_cancel(&self, req: sy_ipc::Request) -> sy_ipc::Response {
        let p: AiplaneCancelParams = match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return v1_err(
                    req.request_id,
                    ErrorCode::BadRequest,
                    &format!("aiplane.cancel params: {e}"),
                );
            }
        };
        // SPEC §4.2 step 2: resolve the workload kind *before*
        // firing `cancel_registry.cancel` — the registered run
        // handler removes its entry from `inflight_kinds` as soon as
        // its `token.cancelled()` branch fires, so reading after the
        // cancel would TOCTOU-race the removal and lose the kind.
        let resolved_workload = p.workload.or_else(|| {
            self.inflight_kinds
                .lock()
                .expect("inflight_kinds poisoned")
                .get(&p.target_request_id)
                .copied()
        });
        let cancelled = self.cancel_registry.cancel(p.target_request_id);
        // SPEC §4.3 step 4-5: also try to abort the inflight call
        // inside the worker. The caller may name the workload
        // explicitly; otherwise the inflight registry resolved it
        // above. If neither path knew the kind, the scheduler-side
        // cancel token still tripped — the request is either still
        // queued (cancel surfaces on dispatch) or has already
        // completed (a no-op).
        let mut worker_cancel_error: Option<String> = None;
        if let Some(workload) = resolved_workload {
            if let Err(e) = self.aiplane.cancel(workload, p.target_request_id) {
                worker_cancel_error = Some(format!("{e:#}"));
            }
        }
        sy_ipc::Response::Ok {
            schema_version: SCHEMA_VERSION,
            request_id: req.request_id,
            result: serde_json::json!({
                "target_request_id": p.target_request_id,
                "cancelled": cancelled,
                "worker_cancel_error": worker_cancel_error,
            }),
            blob: None,
        }
    }
}

fn v1_ok_empty(request_id: Ulid) -> sy_ipc::Response {
    sy_ipc::Response::Ok {
        schema_version: SCHEMA_VERSION,
        request_id,
        result: serde_json::json!({}),
        blob: None,
    }
}

fn v1_err(request_id: Ulid, code: ErrorCode, message: &str) -> sy_ipc::Response {
    sy_ipc::Response::Err {
        schema_version: SCHEMA_VERSION,
        request_id,
        error: ErrorBody {
            code,
            message: message.into(),
            retry_after_ms: None,
            details: serde_json::Value::Null,
        },
    }
}

/// Translate a daemon-local `AiplaneError` into the v1 wire envelope.
/// `Overloaded` carries `retry_after_ms` per SPEC §4.2 example; other
/// variants elide the hint.
fn aiplane_error_to_v1(request_id: Ulid, err: AiplaneError) -> sy_ipc::Response {
    let details = match &err {
        AiplaneError::Overloaded {
            class,
            queue_depth,
            retry_after_ms: _,
        } => serde_json::json!({
            "class": class.as_str(),
            "queue_depth": queue_depth,
        }),
        _ => serde_json::Value::Null,
    };
    sy_ipc::Response::Err {
        schema_version: SCHEMA_VERSION,
        request_id,
        error: ErrorBody {
            code: err.wire_code(),
            message: err.to_string(),
            retry_after_ms: err.retry_after_ms(),
            details,
        },
    }
}

fn legacy_resp_to_v1(request_id: Ulid, resp: Resp) -> sy_ipc::Response {
    match resp {
        Resp::Run { output } => sy_ipc::Response::Ok {
            schema_version: SCHEMA_VERSION,
            request_id,
            result: serde_json::json!({ "output": output }),
            blob: None,
        },
        Resp::Search {
            hits,
            confidence,
            abstained,
        } => sy_ipc::Response::Ok {
            schema_version: SCHEMA_VERSION,
            request_id,
            result: serde_json::json!({
                "hits": hits,
                "confidence": confidence,
                "abstained": abstained,
            }),
            blob: None,
        },
        Resp::Chunk { chunk } => sy_ipc::Response::Ok {
            schema_version: SCHEMA_VERSION,
            request_id,
            result: serde_json::json!({ "chunk": chunk }),
            blob: None,
        },
        Resp::Error { msg } => v1_err(request_id, ErrorCode::Internal, &msg),
    }
}

// --- IPC v1 method translation -----------------------------------------------
//
// The legacy `Op`/`Req` types stay alive as a source-level
// translation layer while the daemon flips to IPC v1 (SPEC §4.2).
// `op_to_v1` and `req_to_v1` map legacy variants → `(method, params)`
// pairs the daemon side decodes with `try_method_to_op` /
// `try_method_to_req`. New callers should use `sy_ipc::Client::call`
// with these method strings directly.

const M_REFRESH_SOURCES: &str = "knowledge.refresh_sources";
const M_INDEX_NOW: &str = "knowledge.index_now";
const M_FULL_RESYNC: &str = "knowledge.full_resync";
const M_RELOAD_SCHEDULE: &str = "knowledge.reload_schedule";
const M_RESCAN_DISCOVERY: &str = "knowledge.rescan_discovery";
const M_PAUSE: &str = "knowledge.pause";
const M_RESUME: &str = "knowledge.resume";
const M_TOGGLE_PAUSE: &str = "knowledge.toggle_pause";
const M_CANCEL: &str = "knowledge.cancel";
const M_SHUTDOWN: &str = "knowledge.shutdown";
const M_RUN: &str = "knowledge.run";
const M_SEARCH: &str = "knowledge.search";
const M_SEARCH_RERANK: &str = "knowledge.search_rerank";
const M_GET_CHUNK: &str = "knowledge.get_chunk";

const M_AIPLANE_RUN: &str = "aiplane.run";
const M_AIPLANE_BATCH: &str = "aiplane.batch";
const M_AIPLANE_CANCEL: &str = "aiplane.cancel";

/// Every IPC v1 method the knowledge daemon advertises (sorted).
/// Surfaced via `system.describe.result.methods`.
/// IPC v1 method namespace served by the aiplane bridge. Shares the
/// `sy-knowledge.sock` listener with `KNOWLEDGE_METHODS` under v1;
/// Zone 5 splits the daemons onto distinct sockets.
pub const AIPLANE_METHODS: &[&str] = &[M_AIPLANE_BATCH, M_AIPLANE_CANCEL, M_AIPLANE_RUN];

pub const KNOWLEDGE_METHODS: &[&str] = &[
    M_CANCEL,
    M_FULL_RESYNC,
    M_GET_CHUNK,
    M_INDEX_NOW,
    M_PAUSE,
    M_REFRESH_SOURCES,
    M_RELOAD_SCHEDULE,
    M_RESCAN_DISCOVERY,
    M_RESUME,
    M_RUN,
    M_SEARCH,
    M_SEARCH_RERANK,
    M_SHUTDOWN,
    M_TOGGLE_PAUSE,
];

/// Translate a legacy fire-and-forget `Op` into the `(method, params)`
/// pair an IPC v1 envelope carries.
pub fn op_to_v1(op: &Op) -> (&'static str, serde_json::Value) {
    let m = match op {
        Op::RefreshSources => M_REFRESH_SOURCES,
        Op::IndexNow => M_INDEX_NOW,
        Op::FullResync => M_FULL_RESYNC,
        Op::ReloadSchedule => M_RELOAD_SCHEDULE,
        Op::RescanDiscovery => M_RESCAN_DISCOVERY,
        Op::Pause => M_PAUSE,
        Op::Resume => M_RESUME,
        Op::TogglePause => M_TOGGLE_PAUSE,
        Op::Cancel => M_CANCEL,
        Op::Shutdown => M_SHUTDOWN,
    };
    (m, serde_json::json!({}))
}

/// Inverse of `op_to_v1`. `None` for any non-knowledge.* method;
/// `Some(Err(_))` only if the params shape is malformed (none of the
/// Op variants carry params today, so the params arg is unused).
pub fn try_method_to_op(method: &str, _params: &serde_json::Value) -> Option<Op> {
    Some(match method {
        M_REFRESH_SOURCES => Op::RefreshSources,
        M_INDEX_NOW => Op::IndexNow,
        M_FULL_RESYNC => Op::FullResync,
        M_RELOAD_SCHEDULE => Op::ReloadSchedule,
        M_RESCAN_DISCOVERY => Op::RescanDiscovery,
        M_PAUSE => Op::Pause,
        M_RESUME => Op::Resume,
        M_TOGGLE_PAUSE => Op::TogglePause,
        M_CANCEL => Op::Cancel,
        M_SHUTDOWN => Op::Shutdown,
        _ => return None,
    })
}

/// Translate a legacy request-response `Req` into the
/// `(method, params)` pair carried by an IPC v1 envelope.
pub fn req_to_v1(req: &Req) -> (&'static str, serde_json::Value) {
    match req {
        Req::Run { workload, input } => (
            M_RUN,
            serde_json::json!({ "workload": workload, "input": input }),
        ),
        Req::Search {
            query,
            limit,
            prefix,
            // `priority` is carried by the v1 envelope, not the
            // method params — the bridge restamps it on the inverse
            // translation.
            priority: _,
            filter,
            abstain_threshold,
        } => (
            M_SEARCH,
            serde_json::json!({
                "query": query,
                "limit": limit,
                "prefix": prefix,
                "filter": filter,
                "abstain_threshold": abstain_threshold,
            }),
        ),
        Req::SearchRerank {
            query,
            limit,
            prefix,
            candidates,
            priority: _,
            filter,
            abstain_threshold,
        } => (
            M_SEARCH_RERANK,
            serde_json::json!({
                "query": query,
                "limit": limit,
                "prefix": prefix,
                "candidates": candidates,
                "filter": filter,
                "abstain_threshold": abstain_threshold,
            }),
        ),
        Req::GetChunk { chunk_id } => (M_GET_CHUNK, serde_json::json!({ "chunk_id": chunk_id })),
    }
}

/// Inverse of `req_to_v1`. Returns `Ok(None)` for any non-knowledge
/// request-response method; `Ok(Some(_))` for a known one with
/// well-formed params; `Err(_)` when the method is known but params
/// are malformed (e.g. missing `query` on search).
pub fn try_method_to_req(method: &str, params: &serde_json::Value) -> Result<Option<Req>, String> {
    match method {
        M_RUN => {
            #[derive(Deserialize)]
            struct RunParams {
                workload: WorkloadKind,
                input: WorkloadInput,
            }
            let RunParams { workload, input } =
                serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
            Ok(Some(Req::Run { workload, input }))
        }
        M_SEARCH => {
            #[derive(Deserialize)]
            struct SearchParams {
                query: String,
                limit: usize,
                #[serde(default)]
                prefix: Option<String>,
                #[serde(default)]
                filter: Option<SearchFilter>,
                #[serde(default)]
                abstain_threshold: Option<f32>,
            }
            let SearchParams {
                query,
                limit,
                prefix,
                filter,
                abstain_threshold,
            } = serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
            Ok(Some(Req::Search {
                query,
                limit,
                prefix,
                priority: default_search_priority(),
                filter,
                abstain_threshold,
            }))
        }
        M_SEARCH_RERANK => {
            #[derive(Deserialize)]
            struct RerankParams {
                query: String,
                limit: usize,
                #[serde(default)]
                prefix: Option<String>,
                candidates: usize,
                #[serde(default)]
                filter: Option<SearchFilter>,
                #[serde(default)]
                abstain_threshold: Option<f32>,
            }
            let RerankParams {
                query,
                limit,
                prefix,
                candidates,
                filter,
                abstain_threshold,
            } = serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
            Ok(Some(Req::SearchRerank {
                query,
                limit,
                prefix,
                priority: default_search_priority(),
                candidates,
                filter,
                abstain_threshold,
            }))
        }
        M_GET_CHUNK => {
            #[derive(Deserialize)]
            struct GetChunkParams {
                chunk_id: String,
            }
            let GetChunkParams { chunk_id } =
                serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
            Ok(Some(Req::GetChunk { chunk_id }))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_roundtrip_all_variants() {
        for op in [
            Op::RefreshSources,
            Op::IndexNow,
            Op::FullResync,
            Op::ReloadSchedule,
            Op::RescanDiscovery,
            Op::Pause,
            Op::Resume,
            Op::TogglePause,
            Op::Cancel,
            Op::Shutdown,
        ] {
            let s = serde_json::to_string(&op).unwrap();
            let back: Op = serde_json::from_str(&s).unwrap();
            // Discriminant equality via Debug — Op doesn't derive PartialEq.
            assert_eq!(format!("{op:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn req_run_roundtrip() {
        let r = Req::Run {
            workload: WorkloadKind::Embed,
            input: WorkloadInput::Text {
                text: "hello".into(),
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Req = serde_json::from_str(&s).unwrap();
        match back {
            Req::Run { workload, input } => {
                assert_eq!(workload, WorkloadKind::Embed);
                match input {
                    WorkloadInput::Text { text } => assert_eq!(text, "hello"),
                    _ => panic!("wrong input variant"),
                }
            }
            _ => panic!("wrong req variant"),
        }
    }

    #[test]
    fn req_search_with_prefix_omits_when_none() {
        let r = Req::Search {
            query: "q".into(),
            limit: 3,
            prefix: None,
            priority: Priority::Interactive,
            filter: None,
            abstain_threshold: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("prefix"), "prefix=None must be omitted");
    }

    #[test]
    fn req_search_rerank_roundtrip() {
        let r = Req::SearchRerank {
            query: "Анна Лу".into(),
            limit: 5,
            prefix: None,
            candidates: 30,
            priority: Priority::Interactive,
            filter: None,
            abstain_threshold: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"req\":\"search-rerank\""));
        assert!(s.contains("\"candidates\":30"));
        let back: Req = serde_json::from_str(&s).unwrap();
        match back {
            Req::SearchRerank {
                query,
                limit,
                candidates,
                prefix,
                priority: _,
                ..
            } => {
                assert_eq!(query, "Анна Лу");
                assert_eq!(limit, 5);
                assert_eq!(candidates, 30);
                assert!(prefix.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn hit_row_embed_score_omitted_when_none() {
        let h = HitRow {
            score: 0.5,
            chunk_id: String::new(),
            file_path: "/x".into(),
            chunk_index: 0,
            chunk_text: "".into(),
            embed_score: None,
        };
        let s = serde_json::to_string(&h).unwrap();
        assert!(!s.contains("embed_score"));
        let h2 = HitRow {
            embed_score: Some(0.42),
            ..h
        };
        let s2 = serde_json::to_string(&h2).unwrap();
        assert!(s2.contains("\"embed_score\":0.42"));
    }

    #[test]
    fn resp_run_roundtrip_vector() {
        let r = Resp::Run {
            output: WorkloadOutput::Vector {
                vector: vec![0.1, 0.2, 0.3],
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Resp = serde_json::from_str(&s).unwrap();
        match back {
            Resp::Run {
                output: WorkloadOutput::Vector { vector },
            } => assert_eq!(vector, vec![0.1, 0.2, 0.3]),
            _ => panic!("wrong resp shape"),
        }
    }

    #[test]
    fn op_to_v1_round_trips_every_variant() {
        for op in [
            Op::RefreshSources,
            Op::IndexNow,
            Op::FullResync,
            Op::ReloadSchedule,
            Op::RescanDiscovery,
            Op::Pause,
            Op::Resume,
            Op::TogglePause,
            Op::Cancel,
            Op::Shutdown,
        ] {
            let (method, params) = op_to_v1(&op);
            assert!(method.starts_with("knowledge."));
            let back = try_method_to_op(method, &params).expect("method must decode");
            assert_eq!(format!("{op:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn req_run_round_trips_through_v1() {
        let r = Req::Run {
            workload: WorkloadKind::Embed,
            input: WorkloadInput::Text {
                text: "hello".into(),
            },
        };
        let (method, params) = req_to_v1(&r);
        assert_eq!(method, "knowledge.run");
        let back = try_method_to_req(method, &params)
            .expect("ok")
            .expect("some");
        match back {
            Req::Run { workload, input } => {
                assert_eq!(workload, WorkloadKind::Embed);
                match input {
                    WorkloadInput::Text { text } => assert_eq!(text, "hello"),
                    other => panic!("wrong input: {other:?}"),
                }
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn req_search_rerank_round_trips_through_v1() {
        let r = Req::SearchRerank {
            query: "Анна Лу".into(),
            limit: 5,
            prefix: Some("scope:".into()),
            candidates: 30,
            priority: Priority::Interactive,
            filter: None,
            abstain_threshold: None,
        };
        let (method, params) = req_to_v1(&r);
        assert_eq!(method, "knowledge.search_rerank");
        let back = try_method_to_req(method, &params)
            .expect("ok")
            .expect("some");
        match back {
            Req::SearchRerank {
                query,
                limit,
                prefix,
                candidates,
                priority: _,
                ..
            } => {
                assert_eq!(query, "Анна Лу");
                assert_eq!(limit, 5);
                assert_eq!(prefix.as_deref(), Some("scope:"));
                assert_eq!(candidates, 30);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn getchunk_req_roundtrips() {
        // REQ-10 fetch-by-id: `Req::GetChunk` survives the v1
        // `(method, params)` round-trip with its `chunk_id` intact.
        let r = Req::GetChunk {
            chunk_id: "deadbeef-0000-1111-2222-333344445555".into(),
        };
        let (method, params) = req_to_v1(&r);
        assert_eq!(method, "knowledge.get_chunk");
        let back = try_method_to_req(method, &params)
            .expect("ok")
            .expect("some");
        match back {
            Req::GetChunk { chunk_id } => {
                assert_eq!(chunk_id, "deadbeef-0000-1111-2222-333344445555");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn search_req_roundtrips_with_filter_and_threshold() {
        let filter = SearchFilter {
            date_from: Some("2024-12-31T00:00:00Z".into()),
            date_to: Some("2025-01-08T00:00:00Z".into()),
            from: vec!["Анна".into()],
            kind: vec!["telegram".into()],
            include_sources: vec!["tg-main".into()],
            exclude_sources: vec!["claude-transcripts".into()],
            ..Default::default()
        };
        let r = Req::Search {
            query: "новый год".into(),
            limit: 5,
            prefix: None,
            priority: Priority::Interactive,
            filter: Some(filter.clone()),
            abstain_threshold: Some(0.5),
        };
        let s = serde_json::to_string(&r).expect("serialize");
        let back: Req = serde_json::from_str(&s).expect("deserialize");
        match back {
            Req::Search {
                filter: Some(f),
                abstain_threshold: Some(t),
                ..
            } => {
                assert_eq!(f, filter);
                assert_eq!(t, 0.5);
            }
            other => panic!("wrong variant or missing fields: {other:?}"),
        }
    }

    #[test]
    fn resp_search_carries_confidence() {
        let r = Resp::Search {
            hits: Vec::new(),
            confidence: 0.73,
            abstained: true,
        };
        let s = serde_json::to_string(&r).expect("serialize");
        let back: Resp = serde_json::from_str(&s).expect("deserialize");
        match back {
            Resp::Search {
                confidence,
                abstained,
                ..
            } => {
                assert_eq!(confidence, 0.73);
                assert!(abstained);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn method_to_req_parses_filter_params() {
        let params = serde_json::json!({
            "query": "новый год",
            "limit": 5,
            "filter": {
                "date_from": "2024-12-31T00:00:00Z",
                "kind": ["telegram"],
                "exclude_sources": ["claude-transcripts"]
            },
            "abstain_threshold": 0.5
        });
        let back = try_method_to_req(M_SEARCH, &params)
            .expect("ok")
            .expect("some");
        match back {
            Req::Search {
                filter: Some(f),
                abstain_threshold: Some(t),
                ..
            } => {
                assert_eq!(f.date_from.as_deref(), Some("2024-12-31T00:00:00Z"));
                assert_eq!(f.kind, vec!["telegram".to_string()]);
                assert_eq!(f.exclude_sources, vec!["claude-transcripts".to_string()]);
                assert_eq!(t, 0.5);
            }
            other => panic!("wrong variant or missing fields: {other:?}"),
        }
    }

    #[test]
    fn try_method_to_op_returns_none_for_unknown_method() {
        assert!(try_method_to_op("system.describe", &serde_json::json!({})).is_none());
        assert!(try_method_to_op("not.a.method", &serde_json::json!({})).is_none());
    }

    #[test]
    fn try_method_to_req_rejects_malformed_params() {
        // Missing `limit` on a search request must surface as an
        // explicit error, not silently translated.
        let err = try_method_to_req("knowledge.search", &serde_json::json!({ "query": "x" }));
        assert!(err.is_err(), "expected params-error, got {err:?}");
    }

    #[test]
    fn knowledge_methods_array_matches_actual_methods() {
        // Guards against a typo in one place: the advertised methods
        // list must contain exactly the methods `op_to_v1` /
        // `req_to_v1` emit. A drift on either side would mean
        // `system.describe` lies about what the daemon serves.
        let mut emitted: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for op in [
            Op::RefreshSources,
            Op::IndexNow,
            Op::FullResync,
            Op::ReloadSchedule,
            Op::RescanDiscovery,
            Op::Pause,
            Op::Resume,
            Op::TogglePause,
            Op::Cancel,
            Op::Shutdown,
        ] {
            emitted.insert(op_to_v1(&op).0);
        }
        for req in [
            Req::Run {
                workload: WorkloadKind::Embed,
                input: WorkloadInput::Text { text: "x".into() },
            },
            Req::Search {
                query: "q".into(),
                limit: 1,
                prefix: None,
                priority: Priority::Interactive,
                filter: None,
                abstain_threshold: None,
            },
            Req::SearchRerank {
                query: "q".into(),
                limit: 1,
                prefix: None,
                candidates: 1,
                priority: Priority::Interactive,
                filter: None,
                abstain_threshold: None,
            },
            Req::GetChunk {
                chunk_id: "id".into(),
            },
        ] {
            emitted.insert(req_to_v1(&req).0);
        }
        let advertised: std::collections::BTreeSet<&str> =
            KNOWLEDGE_METHODS.iter().copied().collect();
        assert_eq!(emitted, advertised);
    }

    #[test]
    fn malformed_request_returns_serde_err_not_panic() {
        let r: Result<Req, _> = serde_json::from_str("not json");
        assert!(r.is_err());
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir_when_set() {
        // Hold `TEST_ENV_LOCK` so the env-var mutation does not race
        // with the daemon-smoke tests below that also set
        // `XDG_RUNTIME_DIR` (and read it back inside `socket_path()`
        // between bind and connect). Without this lock the smoke
        // tests intermittently bind on one path and connect on
        // another, surfacing as ENOENT / ECONNREFUSED flakes.
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", "/tmp/sy-test-runtime");
        let p = socket_path();
        assert_eq!(p, PathBuf::from("/tmp/sy-test-runtime/sy-knowledge.sock"));
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// Daemon-in-thread end-to-end smoke. Exercises the entire IPC
    /// path that the live daemon uses: `serve` binds a Unix socket,
    /// `handle_conn` parses the wire, a worker dispatches
    /// `Req::Run { Embed, Text }` through a `Registry` populated
    /// with the deterministic `FakeWorkload`, the response travels
    /// back on the same stream, and `request()` reads it. No
    /// `/dev/accel/accel0`, no qdrant child, no real ONNX.
    #[test]
    fn daemon_smoke_run_roundtrip_via_fake_workload() {
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use crate::aiplane::registry::Registry;
        use crate::aiplane::session::SessionPool;
        use crate::aiplane::workloads::fake::FakeWorkload;
        use std::sync::Arc;
        use std::thread;

        // Hermetic socket under /tmp so concurrent test runs don't
        // collide with the live daemon's
        // /run/user/$uid/sy-knowledge.sock.
        let unique = format!(
            "sy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let tmp = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        // Spawn `serve` with a Req worker that dispatches through a
        // Registry holding only the FakeWorkload-as-Embed.
        let (ops_tx, _ops_rx) = mpsc::channel::<Op>();
        let (req_tx, req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        serve(ops_tx, req_tx, cancel).expect("serve");

        let registry: Arc<Registry> = {
            let pool = Arc::new(SessionPool::new());
            let mut r = Registry::new(pool);
            r.register(Arc::new(FakeWorkload::embed()));
            Arc::new(r)
        };
        let registry_for_worker = registry.clone();
        thread::spawn(move || {
            while let Ok((req, tx)) = req_rx.recv() {
                let resp = match req {
                    Req::Run { workload, input } => {
                        match registry_for_worker.run(workload, input) {
                            Ok(out) => Resp::Run { output: out },
                            Err(e) => Resp::Error { msg: e.to_string() },
                        }
                    }
                    Req::Search { .. } => Resp::Error {
                        msg: "search not exercised by smoke".into(),
                    },
                    Req::SearchRerank { .. } => Resp::Error {
                        msg: "search-rerank not exercised by smoke".into(),
                    },
                    Req::GetChunk { .. } => Resp::Error {
                        msg: "get-chunk not exercised by smoke".into(),
                    },
                };
                let _ = tx.send(resp);
            }
        });

        // Drive the client side.
        let resp = request_with_priority(
            &Req::Run {
                workload: WorkloadKind::Embed,
                input: WorkloadInput::Text {
                    text: "hello daemon".into(),
                },
            },
            Priority::Interactive,
        )
        .expect("request");
        match resp {
            Resp::Run {
                output: WorkloadOutput::Vector { vector },
            } => {
                assert_eq!(vector.len(), crate::aiplane::workloads::VECTOR_DIM);
                let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
                assert!(
                    (norm - 1.0).abs() < 1e-4,
                    "FakeWorkload returns unit-norm vectors; got {norm}"
                );
            }
            other => panic!("expected Run/Vector, got {other:?}"),
        }

        // Determinism: same input → same vector.
        let r1 = request_with_priority(
            &Req::Run {
                workload: WorkloadKind::Embed,
                input: WorkloadInput::Text { text: "x".into() },
            },
            Priority::Interactive,
        )
        .unwrap();
        let r2 = request_with_priority(
            &Req::Run {
                workload: WorkloadKind::Embed,
                input: WorkloadInput::Text { text: "x".into() },
            },
            Priority::Interactive,
        )
        .unwrap();
        match (r1, r2) {
            (
                Resp::Run {
                    output: WorkloadOutput::Vector { vector: a },
                },
                Resp::Run {
                    output: WorkloadOutput::Vector { vector: b },
                },
            ) => assert_eq!(a, b),
            _ => panic!("non-Vector responses"),
        }

        // Cleanup the hermetic socket.
        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// `Req::SearchRerank` wire path: the daemon's real
    /// `handle_search_rerank` orchestrates embed → qdrant top-N →
    /// rerank, which can't run hermetically (qdrant child + an actual
    /// reranker model). This test exercises the *IPC* contract instead:
    /// it stands up `serve()`, wires a worker that mimics the
    /// orchestration with synthetic candidates and a deterministic
    /// FakeWorkload(Rerank), and verifies the response shape, ordering,
    /// `embed_score` preservation, and limit truncation.
    #[test]
    fn daemon_smoke_search_rerank_via_fake_workload() {
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use crate::aiplane::registry::{Registry, WorkloadInput};
        use crate::aiplane::session::SessionPool;
        use crate::aiplane::workloads::fake::FakeWorkload;
        use std::sync::Arc;
        use std::thread;

        let unique = format!(
            "sy-test-rerank-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let tmp = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        let (ops_tx, _ops_rx) = mpsc::channel::<Op>();
        let (req_tx, req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        serve(ops_tx, req_tx, cancel).expect("serve");

        let registry: Arc<Registry> = {
            let pool = Arc::new(SessionPool::new());
            let mut r = Registry::new(pool);
            r.register(Arc::new(FakeWorkload::new(WorkloadKind::Rerank)));
            Arc::new(r)
        };
        let reg_w = registry.clone();
        thread::spawn(move || {
            while let Ok((req, tx)) = req_rx.recv() {
                let resp = match req {
                    Req::SearchRerank {
                        query,
                        limit,
                        candidates,
                        prefix: _,
                        priority: _,
                        ..
                    } => {
                        // Synthetic candidate set with descending
                        // cosine score so we can verify the rerank
                        // actually changed ordering and the
                        // `embed_score` field carries the prior rank.
                        let raw: Vec<(f32, String, String)> = (0..candidates)
                            .map(|i| {
                                let cosine = 1.0 - (i as f32) * 0.01;
                                let doc = format!("doc-{i}");
                                let path = format!("/tmp/{i}.md");
                                (cosine, path, doc)
                            })
                            .collect();
                        let mut scored: Vec<(f32, f32, String, String)> = raw
                            .into_iter()
                            .map(|(cos, path, doc)| {
                                let s = match reg_w
                                    .run(
                                        WorkloadKind::Rerank,
                                        WorkloadInput::TextPair {
                                            a: query.clone(),
                                            b: doc.clone(),
                                        },
                                    )
                                    .expect("fake rerank")
                                {
                                    WorkloadOutput::Score { score } => score,
                                    _ => panic!("expected Score"),
                                };
                                (s, cos, path, doc)
                            })
                            .collect();
                        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
                        let hits: Vec<HitRow> = scored
                            .into_iter()
                            .take(limit)
                            .enumerate()
                            .map(|(i, (rerank, cos, path, doc))| HitRow {
                                score: rerank,
                                chunk_id: String::new(),
                                file_path: path,
                                chunk_index: i as u32,
                                chunk_text: doc,
                                embed_score: Some(cos),
                            })
                            .collect();
                        Resp::Search {
                            hits,
                            confidence: default_search_confidence(),
                            abstained: false,
                        }
                    }
                    other => Resp::Error {
                        msg: format!("unexpected variant: {other:?}"),
                    },
                };
                let _ = tx.send(resp);
            }
        });

        let resp = request_with_priority(
            &Req::SearchRerank {
                query: "what gifts does Anna Lu like".into(),
                limit: 3,
                prefix: None,
                candidates: 10,
                priority: Priority::Interactive,
                filter: None,
                abstain_threshold: None,
            },
            Priority::Interactive,
        )
        .expect("request");

        match resp {
            Resp::Search { hits, .. } => {
                assert!(hits.len() <= 3, "limit truncation");
                assert!(!hits.is_empty(), "non-empty result");
                // Scores monotonically non-increasing.
                for w in hits.windows(2) {
                    assert!(
                        w[0].score >= w[1].score,
                        "rerank scores must be descending: {} then {}",
                        w[0].score,
                        w[1].score,
                    );
                }
                // embed_score is preserved end-to-end.
                for h in &hits {
                    assert!(
                        h.embed_score.is_some(),
                        "rerank path must carry the pre-rerank cosine score"
                    );
                }
            }
            other => panic!("expected Search resp, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    // arch-ipc-v1 Step 4 DoD integration tests. They live inside the
    // bin's unit-test target because the crate ships only a binary
    // (no `[lib]`), so a Cargo-level `tests/` integration file
    // can't reach `aiplane::ipc::serve` from outside the bin. The
    // tests still exercise the same end-to-end wire (real Unix
    // socket, real `sy_ipc::Client`, real bridge handler).

    /// Hermetic v1 round-trip: `system.describe` lists the knowledge
    /// methods and `knowledge.search` returns a `Resp::Search` payload
    /// from a stub req worker. SPEC §4.2 contract.
    #[test]
    fn knowledge_ipc_v1_round_trip() {
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = unique_tmp_dir("knowledge-v1-roundtrip");
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        let (ops_tx, _ops_rx) = mpsc::channel::<Op>();
        let (req_tx, req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel = Arc::new(AtomicBool::new(false));
        serve(ops_tx, req_tx, cancel).expect("serve v1");

        // Stub req worker that returns a single deterministic hit.
        thread::spawn(move || {
            while let Ok((req, tx)) = req_rx.recv() {
                let resp = match req {
                    Req::Search { query, .. } => Resp::Search {
                        hits: vec![HitRow {
                            score: 0.99,
                            chunk_id: String::new(),
                            file_path: "/tmp/match.md".into(),
                            chunk_index: 0,
                            chunk_text: format!("hit for: {query}"),
                            embed_score: None,
                        }],
                        confidence: default_search_confidence(),
                        abstained: false,
                    },
                    _ => Resp::Error {
                        msg: "unexpected req in roundtrip test".into(),
                    },
                };
                let _ = tx.send(resp);
            }
        });

        // Drive sy_ipc::Client against the bridge.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client rt");
        rt.block_on(async {
            let path = socket_path();
            let mut client = sy_ipc::Client::connect(&path).await.expect("connect");
            let describe = client
                .call(
                    "system.describe",
                    serde_json::json!({}),
                    sy_ipc::CallOpts::default(),
                )
                .await
                .expect("system.describe");
            match describe {
                sy_ipc::Response::Ok { result, .. } => {
                    let methods = result["methods"].as_array().expect("methods array");
                    let names: Vec<&str> = methods.iter().filter_map(|v| v.as_str()).collect();
                    assert!(
                        names.contains(&"knowledge.search"),
                        "describe must list knowledge.search; got {names:?}"
                    );
                    assert!(names.contains(&"system.describe"));
                }
                other => panic!("expected Ok, got {other:?}"),
            }
            let search = client
                .call(
                    "knowledge.search",
                    serde_json::json!({ "query": "hi", "limit": 1, "prefix": null }),
                    sy_ipc::CallOpts::default(),
                )
                .await
                .expect("knowledge.search");
            match search {
                sy_ipc::Response::Ok { result, .. } => {
                    let hits = result["hits"].as_array().expect("hits");
                    assert_eq!(hits.len(), 1);
                    assert_eq!(hits[0]["chunk_text"], "hit for: hi");
                }
                other => panic!("expected Ok, got {other:?}"),
            }
        });

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// SPEC §3.4 anti-goal: legacy line-JSON envelopes (`{"op":...}`)
    /// must be rejected with `IncompatibleSchema`. The daemon answers
    /// with a JSON line carrying the v1-shaped error envelope so the
    /// legacy caller's `serde_json::from_str` over `Resp` sees a
    /// recognisable failure mode.
    #[test]
    fn knowledge_ipc_v1_rejects_legacy_envelope() {
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = unique_tmp_dir("knowledge-v1-reject-legacy");
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        let (ops_tx, _ops_rx) = mpsc::channel::<Op>();
        let (req_tx, _req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel = Arc::new(AtomicBool::new(false));
        serve(ops_tx, req_tx, cancel).expect("serve v1");

        let path = socket_path();
        let mut stream = std::os::unix::net::UnixStream::connect(&path).expect("legacy connect");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        stream
            .write_all(b"{\"op\":\"index-now\"}\n")
            .expect("write legacy line");
        let mut buf = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(&stream)
            .read_line(&mut buf)
            .expect("read reject line");
        assert!(
            buf.contains("IncompatibleSchema"),
            "rejection response must carry IncompatibleSchema; got {buf:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// `system.cancel` must propagate into the legacy cooperative-
    /// cancel `AtomicBool` so an in-flight `knowledge.full_resync`
    /// pass (a synchronous loop owned by `knowledge::daemon::run`)
    /// observes the cancel within its next iteration. The actual pass
    /// is not run here (it needs qdrant + the aiplane supervisor);
    /// the test verifies the bridge sets the atomic — the next-tick
    /// observe path is exercised at the daemon level once the
    /// scheduler split lands.
    #[test]
    fn knowledge_ipc_v1_cancel() {
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = unique_tmp_dir("knowledge-v1-cancel");
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        let (ops_tx, ops_rx) = mpsc::channel::<Op>();
        let (req_tx, _req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel = Arc::new(AtomicBool::new(false));
        serve(ops_tx, req_tx, Arc::clone(&cancel)).expect("serve v1");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client rt");
        rt.block_on(async {
            let path = socket_path();
            let mut client = sy_ipc::Client::connect(&path).await.expect("connect");

            let full_resync_id =
                ulid::Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");
            let opts = sy_ipc::CallOpts {
                request_id: Some(full_resync_id),
                ..sy_ipc::CallOpts::default()
            };
            let ack = client
                .call("knowledge.full_resync", serde_json::json!({}), opts)
                .await
                .expect("full_resync ack");
            assert!(
                matches!(ack, sy_ipc::Response::Ok { .. }),
                "full_resync ack must be Ok"
            );
            assert!(
                !cancel.load(Ordering::SeqCst),
                "cancel atomic should still be false before system.cancel"
            );

            let cancel_resp = client
                .call(
                    "system.cancel",
                    serde_json::json!({ "target_request_id": full_resync_id }),
                    sy_ipc::CallOpts::default(),
                )
                .await
                .expect("system.cancel");
            assert!(
                matches!(cancel_resp, sy_ipc::Response::Ok { .. }),
                "system.cancel must Ok"
            );
        });

        // Within 500 ms (SPEC §4.2 cancellation budget) the bridge
        // must have flipped the daemon's cooperative cancel flag.
        let budget = Duration::from_millis(500);
        let start = std::time::Instant::now();
        while !cancel.load(Ordering::SeqCst) {
            assert!(
                start.elapsed() < budget,
                "cancel atomic not flipped within {budget:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }

        // The full_resync op forwarded to the daemon's main-loop
        // queue too — drain it so we don't leak the receiver on the
        // env-locked thread.
        let _ = ops_rx.try_recv();

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    // arch-ipc-v1 Step 5 DoD integration tests. Same `[[bin]]`-only
    // crate constraint as the Step 4 set — see the note above.

    /// `aiplane.run` routed through a hermetic `FakeWorkload` backend
    /// returns a unit-norm embedding vector. Proves the v1 wire +
    /// the [`AiplaneDispatch`] indirection plumb end-to-end without
    /// the multi-process supervisor.
    #[test]
    fn aiplane_ipc_v1_run() {
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = unique_tmp_dir("aiplane-v1-run");
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        let (ops_tx, _ops_rx) = mpsc::channel::<Op>();
        let (req_tx, _req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel = Arc::new(AtomicBool::new(false));
        let dispatch: Arc<dyn AiplaneDispatch> = Arc::new(FakeAiplaneDispatch::embed());
        serve_with_dispatch(ops_tx, req_tx, cancel, dispatch).expect("serve v1");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client rt");
        rt.block_on(async {
            let path = socket_path();
            let mut client = sy_ipc::Client::connect(&path).await.expect("connect");
            let resp = client
                .call(
                    "aiplane.run",
                    serde_json::json!({
                        "workload": "embed",
                        "input": { "kind": "text", "text": "hello v1" },
                    }),
                    sy_ipc::CallOpts::default(),
                )
                .await
                .expect("aiplane.run");
            match resp {
                sy_ipc::Response::Ok { result, .. } => {
                    let output: WorkloadOutput =
                        serde_json::from_value(result["output"].clone()).expect("decode output");
                    match output {
                        WorkloadOutput::Vector { vector } => {
                            assert_eq!(vector.len(), crate::aiplane::workloads::VECTOR_DIM);
                            let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
                            assert!(
                                (norm - 1.0).abs() < 1e-4,
                                "FakeWorkload returns unit-norm vectors; got {norm}"
                            );
                        }
                        other => panic!("expected Vector, got {other:?}"),
                    }
                }
                other => panic!("expected Ok, got {other:?}"),
            }
        });

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// SPEC §4.2 cancellation budget: `system.cancel` lands the
    /// `Cancelled` outcome on an in-flight `aiplane.run` (whose
    /// hermetic `sleep_ms` is sized 10× the budget) within 500 ms.
    #[test]
    fn aiplane_ipc_v1_cancel() {
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = unique_tmp_dir("aiplane-v1-cancel");
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        let (ops_tx, _ops_rx) = mpsc::channel::<Op>();
        let (req_tx, _req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel = Arc::new(AtomicBool::new(false));
        let dispatch: Arc<dyn AiplaneDispatch> = Arc::new(FakeAiplaneDispatch::embed());
        serve_with_dispatch(ops_tx, req_tx, cancel, dispatch).expect("serve v1");

        const SLEEP_MS: u64 = 5_000;
        const CANCEL_BUDGET_MS: u64 = 500;
        const KICK_MS: u64 = 100;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("client rt");
        rt.block_on(async {
            let path = socket_path();
            let mut runner = sy_ipc::Client::connect(&path)
                .await
                .expect("connect runner");
            let mut canceler = sy_ipc::Client::connect(&path)
                .await
                .expect("connect canceler");

            let request_id = ulid::Ulid::new();
            let run_opts = sy_ipc::CallOpts {
                request_id: Some(request_id),
                deadline_ms: Some(SLEEP_MS + 1_000),
                ..sy_ipc::CallOpts::default()
            };
            let run_call = tokio::spawn(async move {
                runner
                    .call(
                        "aiplane.run",
                        serde_json::json!({
                            "workload": "embed",
                            "input": { "kind": "text", "text": "slow" },
                            "sleep_ms": SLEEP_MS,
                        }),
                        run_opts,
                    )
                    .await
            });

            // Give the server time to register the request_id before
            // the cancel hits — otherwise we'd race the LSP property
            // (cancel-before-register no-op).
            tokio::time::sleep(Duration::from_millis(KICK_MS)).await;
            let cancel_resp = canceler
                .call(
                    "system.cancel",
                    serde_json::json!({ "target_request_id": request_id }),
                    sy_ipc::CallOpts::default(),
                )
                .await
                .expect("system.cancel");
            match cancel_resp {
                sy_ipc::Response::Ok { result, .. } => {
                    assert_eq!(result["cancelled"], serde_json::Value::Bool(true));
                }
                other => panic!("expected Ok ack from cancel, got {other:?}"),
            }

            let start = std::time::Instant::now();
            let run_outcome =
                tokio::time::timeout(Duration::from_millis(CANCEL_BUDGET_MS), run_call)
                    .await
                    .expect("run must return within cancel budget")
                    .expect("run task join")
                    .expect("run call");
            assert!(
                start.elapsed() < Duration::from_millis(CANCEL_BUDGET_MS),
                "Cancelled response should arrive within {CANCEL_BUDGET_MS} ms; took {:?}",
                start.elapsed()
            );
            match run_outcome {
                sy_ipc::Response::Err { error, .. } => {
                    assert_eq!(
                        error.code,
                        sy_core::ErrorCode::Cancelled,
                        "expected ErrorCode::Cancelled, got {error:?}"
                    );
                }
                other => panic!("expected Err(Cancelled), got {other:?}"),
            }
        });

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// SPEC §4.2 + arch-aiplane-scheduler Step 7: `aiplane.cancel`
    /// resolves the inflight `target_request_id` to its workload
    /// kind via the bridge's `inflight_kinds` registry when the
    /// caller omits `workload`. `sy aiplane cancel <ulid>` relies on
    /// this so users don't have to type the workload name they don't
    /// know.
    #[test]
    fn aiplane_cancel_resolves_workload_from_inflight_registry() {
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = unique_tmp_dir("aiplane-v1-cancel-registry");
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        let (ops_tx, _ops_rx) = mpsc::channel::<Op>();
        let (req_tx, _req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel_atomic = Arc::new(AtomicBool::new(false));
        let (recorder, cancels) = RecordingDispatch::embed();
        let dispatch: Arc<dyn AiplaneDispatch> = recorder.clone() as Arc<dyn AiplaneDispatch>;
        serve_with_dispatch(ops_tx, req_tx, cancel_atomic, dispatch).expect("serve v1");

        const SLEEP_MS: u64 = 5_000;
        const KICK_MS: u64 = 100;
        const CANCEL_BUDGET_MS: u64 = 1_500;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("client rt");
        rt.block_on(async {
            let path = socket_path();
            let mut runner = sy_ipc::Client::connect(&path)
                .await
                .expect("connect runner");
            let mut canceler = sy_ipc::Client::connect(&path)
                .await
                .expect("connect canceler");

            let request_id = ulid::Ulid::new();
            let run_opts = sy_ipc::CallOpts {
                request_id: Some(request_id),
                deadline_ms: Some(SLEEP_MS + 1_000),
                ..sy_ipc::CallOpts::default()
            };
            let run_call = tokio::spawn(async move {
                runner
                    .call(
                        "aiplane.run",
                        serde_json::json!({
                            "workload": "embed",
                            "input": { "kind": "text", "text": "slow" },
                            "sleep_ms": SLEEP_MS,
                        }),
                        run_opts,
                    )
                    .await
            });

            // Let the bridge admit + register the request before the
            // cancel arrives (otherwise the inflight registry won't
            // know the kind).
            tokio::time::sleep(Duration::from_millis(KICK_MS)).await;
            let cancel_resp = canceler
                .call(
                    "aiplane.cancel",
                    // Crucially: no `workload` field — the bridge
                    // must resolve it from the inflight registry.
                    serde_json::json!({ "target_request_id": request_id }),
                    sy_ipc::CallOpts::default(),
                )
                .await
                .expect("aiplane.cancel");
            match cancel_resp {
                sy_ipc::Response::Ok { result, .. } => {
                    assert_eq!(result["cancelled"], serde_json::Value::Bool(true));
                }
                other => panic!("expected Ok ack from cancel, got {other:?}"),
            }

            let run_outcome =
                tokio::time::timeout(Duration::from_millis(CANCEL_BUDGET_MS), run_call)
                    .await
                    .expect("run must return within cancel budget")
                    .expect("run task join")
                    .expect("run call");
            match run_outcome {
                sy_ipc::Response::Err { error, .. } => {
                    assert_eq!(error.code, sy_core::ErrorCode::Cancelled);
                }
                other => panic!("expected Err(Cancelled), got {other:?}"),
            }

            // The bridge resolved the workload kind from the
            // inflight registry and called `AiplaneDispatch::cancel`
            // with it — RecordingDispatch captured the (kind, id).
            let recorded = cancels.lock().expect("cancels poisoned").clone();
            assert_eq!(recorded.len(), 1, "exactly one cancel forwarded");
            assert_eq!(recorded[0].0, WorkloadKind::Embed);
            assert_eq!(recorded[0].1, request_id);
        });

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// `system.describe` advertises the four scheduler priority
    /// classes (`Realtime` / `Interactive` / `Background` / `Batch`)
    /// even before the scheduler split lands in Zone 3 — clients can
    /// validate `--priority Foo` at submit time without round-trip
    /// rejections.
    #[test]
    fn aiplane_ipc_v1_describe_capabilities() {
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = unique_tmp_dir("aiplane-v1-describe");
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        let (ops_tx, _ops_rx) = mpsc::channel::<Op>();
        let (req_tx, _req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel = Arc::new(AtomicBool::new(false));
        let dispatch: Arc<dyn AiplaneDispatch> = Arc::new(FakeAiplaneDispatch::embed());
        serve_with_dispatch(ops_tx, req_tx, cancel, dispatch).expect("serve v1");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client rt");
        rt.block_on(async {
            let path = socket_path();
            let mut client = sy_ipc::Client::connect(&path).await.expect("connect");
            let resp = client
                .call(
                    "system.describe",
                    serde_json::json!({}),
                    sy_ipc::CallOpts::default(),
                )
                .await
                .expect("system.describe");
            match resp {
                sy_ipc::Response::Ok { result, .. } => {
                    let classes = result["capabilities"]["priority_classes"]
                        .as_array()
                        .expect("priority_classes array");
                    let names: Vec<&str> = classes.iter().filter_map(|v| v.as_str()).collect();
                    assert_eq!(
                        names,
                        vec!["Realtime", "Interactive", "Background", "Batch"],
                        "priority classes must be the four canonical names in spec order"
                    );

                    let methods = result["methods"].as_array().expect("methods array");
                    let method_names: Vec<&str> =
                        methods.iter().filter_map(|v| v.as_str()).collect();
                    for required in ["aiplane.run", "aiplane.batch", "aiplane.cancel"] {
                        assert!(
                            method_names.contains(&required),
                            "describe must list {required}; got {method_names:?}"
                        );
                    }
                }
                other => panic!("expected Ok, got {other:?}"),
            }
        });

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// Step 2 e2e priority test: with three slow `Background` requests
    /// admitted ahead of one `Interactive`, the dispatcher pulls
    /// Interactive after the first in-flight Background completes —
    /// not after all three. Step 2 doesn't preempt the in-flight call
    /// (Step 4 adds the cross-class hard escape); it only re-orders
    /// the *next* pull.
    #[test]
    fn scheduler_priority_e2e() {
        // The dispatcher's first call hits `SlowFakeDispatch`'s gate
        // and parks until the test releases it. That lets every
        // priority class admit *before* the second dispatch can fire,
        // so the strict-priority order is observable deterministically
        // — no wall-clock assumptions, no flake under parallel
        // `cargo test` load.
        const SLEEP_MS: u64 = 50;
        const N_BG: usize = 3;
        const ADMIT_SETTLE: Duration = Duration::from_millis(150);
        let _smoke = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = unique_tmp_dir("aiplane-v1-priority");
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);

        let (ops_tx, _ops_rx) = mpsc::channel::<Op>();
        let (req_tx, _req_rx) = mpsc::channel::<(Req, oneshot::Sender<Resp>)>();
        let cancel = Arc::new(AtomicBool::new(false));
        let slow = Arc::new(SlowFakeDispatch::new(Duration::from_millis(SLEEP_MS)));
        let dispatch: Arc<dyn AiplaneDispatch> = Arc::clone(&slow) as Arc<dyn AiplaneDispatch>;
        serve_with_dispatch(ops_tx, req_tx, cancel, dispatch).expect("serve v1");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("client rt");
        rt.block_on(async {
            let path = socket_path();
            let mut bg_handles = Vec::with_capacity(N_BG);
            for i in 0..N_BG {
                let p = path.clone();
                let handle = tokio::spawn(async move {
                    let mut c = sy_ipc::Client::connect(&p).await.expect("bg connect");
                    c.call(
                        "aiplane.run",
                        serde_json::json!({
                            "workload": "embed",
                            "input": { "kind": "text", "text": format!("bg-{i}") },
                        }),
                        sy_ipc::CallOpts {
                            priority: Priority::Background,
                            deadline_ms: Some(10_000),
                            ..sy_ipc::CallOpts::default()
                        },
                    )
                    .await
                    .expect("bg aiplane.run")
                });
                bg_handles.push(handle);
            }
            // Let the three Backgrounds admit; whichever the
            // dispatcher races to pull first will park on the gate.
            tokio::time::sleep(ADMIT_SETTLE).await;

            let interactive_handle = tokio::spawn({
                let p = path.clone();
                async move {
                    let mut c = sy_ipc::Client::connect(&p).await.expect("interactive connect");
                    c.call(
                        "aiplane.run",
                        serde_json::json!({
                            "workload": "embed",
                            "input": { "kind": "text", "text": "interactive" },
                        }),
                        sy_ipc::CallOpts {
                            priority: Priority::Interactive,
                            deadline_ms: Some(10_000),
                            ..sy_ipc::CallOpts::default()
                        },
                    )
                    .await
                    .expect("interactive aiplane.run")
                }
            });
            tokio::time::sleep(ADMIT_SETTLE).await;

            // All four are queued (one on the gate, three in their
            // class queues). Release — the dispatcher then pulls in
            // strict priority order.
            slow.release_gate();

            let interactive_resp = interactive_handle.await.expect("interactive join");
            assert!(
                matches!(interactive_resp, sy_ipc::Response::Ok { .. }),
                "interactive must succeed"
            );
            for (i, h) in bg_handles.into_iter().enumerate() {
                let resp = h.await.expect("bg join");
                assert!(matches!(resp, sy_ipc::Response::Ok { .. }), "bg-{i} ok");
            }

            // Strict-priority guarantee: whichever Background hit the
            // gate first runs first; then Interactive (the highest
            // remaining); then the two remaining Backgrounds in any
            // order. So `"interactive"` lands at index 1 in the
            // dispatch log.
            let order = slow.snapshot();
            assert_eq!(order.len(), N_BG + 1, "all 4 dispatched: {order:?}");
            let interactive_idx = order
                .iter()
                .position(|s| s == "interactive")
                .expect("interactive dispatched");
            assert_eq!(
                interactive_idx, 1,
                "Interactive must run second (after the one BG that hit the gate first): order={order:?}"
            );
            assert!(
                order[0].starts_with("bg-"),
                "first dispatch must be a Background (raced to gate first): order={order:?}"
            );
        });

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// `AiplaneDispatch` decorator used by the cancel-via-registry
    /// test: forwards `run`/`batch` to the inner FakeWorkload-backed
    /// dispatch, and records every `cancel(workload, request_id)`
    /// call so the test can assert the bridge resolved the workload
    /// kind from the inflight registry rather than from the params.
    type CancelLog = Arc<std::sync::Mutex<Vec<(WorkloadKind, Ulid)>>>;

    struct RecordingDispatch {
        inner: Arc<FakeAiplaneDispatch>,
        cancels: CancelLog,
    }

    impl RecordingDispatch {
        fn embed() -> (Arc<Self>, CancelLog) {
            let cancels: CancelLog = Arc::new(std::sync::Mutex::new(Vec::new()));
            let inner = Arc::new(FakeAiplaneDispatch::embed());
            let recorder = Arc::new(Self {
                inner,
                cancels: Arc::clone(&cancels),
            });
            (recorder, cancels)
        }
    }

    impl AiplaneDispatch for RecordingDispatch {
        fn run(&self, workload: WorkloadKind, input: WorkloadInput) -> Result<WorkloadOutput> {
            self.inner.run(workload, input)
        }
        fn batch(
            &self,
            workload: WorkloadKind,
            inputs: Vec<WorkloadInput>,
        ) -> Result<Vec<WorkloadOutput>> {
            self.inner.batch(workload, inputs)
        }
        fn cancel(&self, workload: WorkloadKind, request_id: Ulid) -> Result<()> {
            self.cancels
                .lock()
                .expect("cancels poisoned")
                .push((workload, request_id));
            Ok(())
        }
    }

    /// `AiplaneDispatch` that sleeps for a fixed duration per call so
    /// the e2e priority test can observe queue ordering. Otherwise
    /// the FakeWorkload's instant return makes the priority ordering
    /// race against scheduler thread wake-up. Records the input text
    /// of every dispatched call in `order` so callers can verify the
    /// strict-priority semantic deterministically — relying on
    /// wall-clock elapsed times flakes badly under parallel
    /// `cargo test` load.
    struct SlowFakeDispatch {
        per_call: Duration,
        order: std::sync::Mutex<Vec<String>>,
        gate: std::sync::Mutex<bool>,
        gate_cv: std::sync::Condvar,
        gate_consumed: std::sync::atomic::AtomicBool,
    }

    impl SlowFakeDispatch {
        fn new(per_call: Duration) -> Self {
            Self {
                per_call,
                order: std::sync::Mutex::new(Vec::new()),
                gate: std::sync::Mutex::new(false),
                gate_cv: std::sync::Condvar::new(),
                gate_consumed: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn snapshot(&self) -> Vec<String> {
            self.order.lock().expect("order poisoned").clone()
        }

        fn release_gate(&self) {
            *self.gate.lock().expect("gate poisoned") = true;
            self.gate_cv.notify_all();
        }
    }

    impl AiplaneDispatch for SlowFakeDispatch {
        fn run(&self, _workload: WorkloadKind, input: WorkloadInput) -> Result<WorkloadOutput> {
            // First call to `run` blocks on `gate` so the e2e test
            // can admit every priority class before the dispatcher
            // pulls the second call — the strict-priority ordering
            // is then observable without wall-clock assumptions.
            if self
                .gate_consumed
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                let mut g = self.gate.lock().expect("gate poisoned");
                while !*g {
                    g = self.gate_cv.wait(g).expect("gate poisoned");
                }
            }
            std::thread::sleep(self.per_call);
            let text = match input {
                WorkloadInput::Text { text } => text,
                _ => String::new(),
            };
            self.order
                .lock()
                .expect("order poisoned")
                .push(text.clone());
            Ok(WorkloadOutput::Text { text })
        }
        fn batch(
            &self,
            workload: WorkloadKind,
            inputs: Vec<WorkloadInput>,
        ) -> Result<Vec<WorkloadOutput>> {
            inputs.into_iter().map(|i| self.run(workload, i)).collect()
        }
    }

    /// Hermetic [`AiplaneDispatch`] that drives a `FakeWorkload`
    /// directly — no supervisor, no real ORT session.
    struct FakeAiplaneDispatch {
        registry: std::sync::Arc<crate::aiplane::registry::Registry>,
    }

    impl FakeAiplaneDispatch {
        fn embed() -> Self {
            use crate::aiplane::registry::Registry;
            use crate::aiplane::session::SessionPool;
            use crate::aiplane::workloads::fake::FakeWorkload;
            let pool = std::sync::Arc::new(SessionPool::new());
            let mut reg = Registry::new(pool);
            reg.register(std::sync::Arc::new(FakeWorkload::embed()));
            Self {
                registry: std::sync::Arc::new(reg),
            }
        }
    }

    impl AiplaneDispatch for FakeAiplaneDispatch {
        fn run(&self, workload: WorkloadKind, input: WorkloadInput) -> Result<WorkloadOutput> {
            self.registry.run(workload, input)
        }
        fn batch(
            &self,
            workload: WorkloadKind,
            inputs: Vec<WorkloadInput>,
        ) -> Result<Vec<WorkloadOutput>> {
            let mut out = Vec::with_capacity(inputs.len());
            for input in inputs {
                out.push(self.registry.run(workload, input)?);
            }
            Ok(out)
        }
    }

    fn unique_tmp_dir(prefix: &str) -> std::path::PathBuf {
        let id = format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(id)
    }
}
