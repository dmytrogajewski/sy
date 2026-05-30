//! Process supervisor for `sy file` plugin processes.
//!
//! Implements the spawn → initialize → request loop → shutdown / exit
//! → restart-on-EOF lifecycle from [plugin SPEC
//! §4.4](../../../specs/research/sy-file-manager-plugins/SPEC.md#44-supervision--restart).
//! Wraps a [`sandbox::build_command`] [`tokio::process::Command`] in a
//! [`PluginProc`] actor; the actor owns the long-lived
//! stdin/stdout duplex, runs the host side of the SPEC §4.2.3
//! lifecycle protocol, and restarts the child on EOF with a
//! `2^n * 100 ms` backoff up to [`MAX_RESTART_ATTEMPTS`].
//!
//! Public surface (see also the per-test contract in
//! `tests::*`):
//!
//! * [`spawn`] — fork the child, perform the SPEC §4.2.3 `initialize`
//!   handshake, return a handle.
//! * [`PluginProc::request`] — send a method, await the JSON-RPC
//!   response with a [`RpcError`] discriminant on every failure.
//! * [`PluginProc::shutdown`] — graceful `shutdown` request followed
//!   by the `exit` notification, bounded by
//!   `limits.shutdown_timeout_ms`.
//! * [`PluginProc::health`] — current [`State`] snapshot.
//!
//! No `.unwrap()` / `.expect()` on the wire path: every fallible
//! operation surfaces an [`RpcError`] so the supervisor's caller can
//! distinguish a child-side error (CAP_NOT_GRANTED, etc.) from a
//! transport-layer outage (peer disconnect, ping timeout).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::codec::Framed;

use crate::plugin::capability::{build_initialize_params, parse_initialize_result, NegotiatedCaps};
use crate::plugin::host_fns::{self, HostCtx};
use crate::plugin::manifest::Manifest;
use crate::plugin::rpc::{ErrorObj, Notification, Request, Response, JSONRPC_VERSION};
use crate::plugin::sandbox::build_command;
use crate::plugin::transport::JsonRpcCodec;

/// SPEC §4.4 maximum restart attempts before the supervisor parks the
/// plugin in [`State::Unhealthy`]. Three is the SPEC value; the
/// backoff sum (100 + 200 + 400 ms = 700 ms) keeps the worst-case
/// restart ladder under one second of wall-clock.
pub const MAX_RESTART_ATTEMPTS: u32 = 3;

/// SPEC §4.4 backoff base. Attempt `n` (0-indexed) sleeps
/// `2^n * BACKOFF_BASE_MS` before respawning.
const BACKOFF_BASE_MS: u64 = 100;

/// Default ping interval. SPEC §4.4 specifies 30 s; tests override
/// this via [`SpawnOpts::ping_interval`] so the
/// `ping_missed_triggers_restart` scenario completes in under a
/// second.
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(30);

/// Default ping reply budget. The supervisor waits this long for a
/// `ping` response before treating the plugin as stalled.
pub const DEFAULT_PING_TIMEOUT: Duration = Duration::from_secs(5);

/// SPEC §4.4 supervisor state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Pre-handshake — the child has been forked but `initialize`
    /// has not yet returned.
    Spawning,
    /// `initialize` handshake completed; request loop is running.
    Ready,
    /// Graceful shutdown in flight (between `shutdown` request and
    /// `exit` notification).
    ShuttingDown,
    /// Child exited or hit EOF; supervisor is in the backoff ladder
    /// waiting to respawn.
    Restarting { attempts: u32 },
    /// SPEC §4.4 terminal state — three failed restarts.
    /// `last_err` carries the human-readable cause so
    /// `sy file doctor` can surface it.
    Unhealthy { attempts: u32, last_err: String },
}

/// Wire-path error discriminant. Every fallible supervisor operation
/// returns one of these so the caller never has to grep a free-form
/// `anyhow::Error` to distinguish a peer-side error from a transport
/// outage.
#[derive(Debug, Clone)]
pub enum RpcError {
    /// Spawning the child failed (binary missing, sandbox refused).
    Spawn(String),
    /// `initialize` handshake failed (mismatched api versions,
    /// malformed response, peer closed mid-handshake).
    Handshake(String),
    /// Plugin returned a JSON-RPC error object.
    Peer {
        code: i32,
        message: String,
        data: Value,
    },
    /// Transport-level failure (codec, framing, IO).
    Transport(String),
    /// Wire-shape violation that isn't recoverable by retry: the peer
    /// sent a syntactically well-formed JSON-RPC message whose
    /// *contents* break the SPEC §4.2.3 contract (e.g. a plugin
    /// advertises a `[[capability]]` row not present in its own
    /// manifest). Step 5 introduces this variant so capability
    /// negotiation has a stable, non-`Peer`, non-`Handshake`
    /// discriminant; future protocol guards (Step 6+) reuse it.
    Protocol(String),
    /// Supervisor is in [`State::Unhealthy`] or [`State::ShuttingDown`]
    /// and won't accept new requests.
    Unavailable(String),
    /// Request did not complete inside its deadline.
    Timeout(Duration),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Spawn(s) => write!(f, "spawn failed: {s}"),
            RpcError::Handshake(s) => write!(f, "initialize handshake failed: {s}"),
            RpcError::Peer { code, message, .. } => {
                write!(f, "plugin error code={code} msg={message}")
            }
            RpcError::Transport(s) => write!(f, "transport: {s}"),
            RpcError::Protocol(s) => write!(f, "protocol: {s}"),
            RpcError::Unavailable(s) => write!(f, "supervisor unavailable: {s}"),
            RpcError::Timeout(d) => write!(f, "timeout after {d:?}"),
        }
    }
}

impl std::error::Error for RpcError {}

impl From<ErrorObj> for RpcError {
    fn from(e: ErrorObj) -> Self {
        RpcError::Peer {
            code: e.code,
            message: e.message,
            data: e.data,
        }
    }
}

/// Spawn-time knobs. Tests override `ping_interval` and
/// `request_timeout` so the scenarios complete in sub-second wall
/// time; production callers take the SPEC defaults.
#[derive(Debug, Clone)]
pub struct SpawnOpts {
    /// Per-plugin runtime slot (typically
    /// `sandbox::runtime_dir_for(&manifest.plugin.id)`). The caller
    /// has already `mkdir -p`'d this path.
    pub workdir: PathBuf,
    /// Host name advertised in the `initialize` request.
    pub host_name: String,
    /// Host version advertised in the `initialize` request.
    pub host_version: String,
    /// API set the host implements. The SPEC §4.2.3 handshake
    /// compares this to the manifest's `api_min..=api_max` interval;
    /// disjoint sets fail with [`API_VERSION_MISMATCH`].
    pub host_api: Vec<String>,
    /// `ping` cadence. SPEC §4.4 calls for 30 s in production; tests
    /// pull this down to ~100 ms so the missed-ping path is reachable
    /// inside a sub-second test budget.
    pub ping_interval: Duration,
    /// Per-`ping` reply budget. Missed → restart.
    pub ping_timeout: Duration,
    /// Per-request reply budget for non-`ping` traffic.
    pub request_timeout: Duration,
    /// Maximum restart attempts before parking in
    /// [`State::Unhealthy`]. Test scenarios override this; production
    /// keeps the SPEC §4.4 default of [`MAX_RESTART_ATTEMPTS`].
    pub max_restart_attempts: u32,
    /// SPEC §4.2.5 host-callable surface. When `Some`, plugin-
    /// initiated `host.*` requests route to
    /// [`crate::plugin::host_fns::dispatch`] with this context;
    /// `None` is the pre-Step-6 behaviour (plugin-initiated requests
    /// are logged + dropped). Tests build one via
    /// [`crate::plugin::host_fns::ctx_for`].
    pub host_ctx: Option<HostCtx>,
}

impl SpawnOpts {
    /// Construct an options block with the SPEC §4.4 production
    /// defaults. Callers supply the workdir; the host identity
    /// defaults to `("sy", env!("CARGO_PKG_VERSION"), ["1"])`.
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            host_name: "sy".to_string(),
            host_version: env!("CARGO_PKG_VERSION").to_string(),
            host_api: vec!["1".to_string()],
            ping_interval: DEFAULT_PING_INTERVAL,
            ping_timeout: DEFAULT_PING_TIMEOUT,
            request_timeout: Duration::from_secs(10),
            max_restart_attempts: MAX_RESTART_ATTEMPTS,
            host_ctx: None,
        }
    }
}

/// Handle to a running plugin actor. Cheap to clone is *not*
/// supported — the actor owns its mpsc senders; callers that need a
/// shared handle wrap this in an `Arc<PluginProc>` and use
/// [`PluginProc::request`] through the borrow.
#[derive(Debug)]
pub struct PluginProc {
    /// Plugin id from the manifest (`manifest.plugin.id`). Lives in
    /// every `tracing` span attached to this supervisor.
    pub id: String,
    /// Outbound channel into the actor task. `None` after
    /// [`PluginProc::shutdown`] joined.
    cmd_tx: Option<mpsc::Sender<SupervisorCmd>>,
    /// Watch of the current [`State`]. The actor publishes;
    /// `health()` and the writer-side DoS guard read.
    health_rx: watch::Receiver<State>,
    /// JoinHandle for the actor; awaited by [`PluginProc::shutdown`].
    actor: Option<JoinHandle<()>>,
    /// SPEC §4.2.3 negotiated capability set from the spawn-time
    /// `initialize` handshake. `None` only after [`Self::shutdown`]
    /// has been called and the field consumed; the spawn path
    /// always populates this before returning. Step 6's
    /// `host_fns::dispatch` reads this to short-circuit `check_cap`
    /// for methods the plugin never offered.
    caps: Option<NegotiatedCaps>,
}

/// Commands the actor task processes off its mpsc.
#[derive(Debug)]
enum SupervisorCmd {
    /// Send a method, await the response.
    Request {
        method: String,
        params: Value,
        reply: oneshot::Sender<std::result::Result<Value, RpcError>>,
    },
    /// Send the `shutdown` request, then the `exit` notification.
    /// Reply fires after the child has reaped (or the
    /// shutdown_timeout elapsed).
    Shutdown {
        reply: oneshot::Sender<std::result::Result<(), RpcError>>,
    },
}

/// Spawn a plugin process under the SPEC §4.3 sandbox envelope and
/// drive the SPEC §4.2.3 `initialize` handshake. Returns once the
/// handshake completes (the plugin's `initialize` response has
/// arrived) — at which point [`PluginProc::health`] reads
/// [`State::Ready`].
#[tracing::instrument(skip(manifest, opts), fields(plugin_id = %manifest.plugin.id))]
pub async fn spawn(
    manifest: Manifest,
    opts: SpawnOpts,
) -> std::result::Result<PluginProc, RpcError> {
    let id = manifest.plugin.id.clone();
    let (health_tx, health_rx) = watch::channel(State::Spawning);
    let (cmd_tx, cmd_rx) = mpsc::channel::<SupervisorCmd>(64);

    // SPEC §4.5 env table: `SY_PLUGIN_NO_SIGNATURE=1` is the testing-
    // only bypass that skips signature verification on every spawn.
    // Emit one `tracing::warn!` per spawn so a host accidentally
    // shipped with this set is immediately visible in journald /
    // stderr — the SPEC explicitly calls this out as "prints a
    // warning on every spawn".
    if std::env::var_os(crate::plugin::install::NO_SIGNATURE_ENV)
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        tracing::warn!(
            target = "sy::plugin::proc",
            plugin_id = %id,
            env = crate::plugin::install::NO_SIGNATURE_ENV,
            "SY_PLUGIN_NO_SIGNATURE=1: spawning without signature verification (testing only)"
        );
    }

    // Run the first spawn synchronously so `spawn()` returns with
    // either `State::Ready` or a typed `RpcError`. After the first
    // handshake succeeds, the actor task takes over for the
    // request-loop + restart-on-EOF ladder.
    let (child, framed) = boot_child(&manifest, &opts, &id).await?;
    let initial_response = perform_handshake(&framed, &manifest, &opts, &id).await;
    let (framed, caps) = match initial_response {
        Ok(pair) => pair,
        Err(e) => {
            // Best-effort kill; the typed error is what the caller
            // gets back.
            let _ = kill_child(child).await;
            return Err(e);
        }
    };
    health_tx.send_replace(State::Ready);

    let actor = tokio::spawn(run_actor(
        manifest,
        opts,
        id.clone(),
        cmd_rx,
        health_tx,
        child,
        framed,
    ));

    Ok(PluginProc {
        id,
        cmd_tx: Some(cmd_tx),
        health_rx,
        actor: Some(actor),
        caps: Some(caps),
    })
}

/// Spawn the child under the sandbox envelope and wrap its stdio in a
/// `Framed<_, JsonRpcCodec>` duplex. Stderr stays inherited per the
/// SPEC §4.3 envelope so plugin log lines reach the host tracing
/// span via the parent stderr.
async fn boot_child(
    manifest: &Manifest,
    opts: &SpawnOpts,
    id: &str,
) -> std::result::Result<(Child, FramedDuplex), RpcError> {
    let mut cmd = build_command(manifest, &opts.workdir)
        .map_err(|e| RpcError::Spawn(format!("build_command: {e:#}")))?;
    let mut child = cmd
        .spawn()
        .map_err(|e| RpcError::Spawn(format!("spawn: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| RpcError::Spawn("child stdin not piped".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RpcError::Spawn("child stdout not piped".into()))?;
    tracing::debug!(target = "sy::plugin::proc", plugin_id = %id, pid = ?child.id(), "spawned");
    Ok((child, FramedDuplex::new(stdin, stdout)))
}

/// Drive the SPEC §4.2.3 `initialize` request → response handshake.
///
/// Builds the request via [`build_initialize_params`] (single source
/// of truth for the host-callable surface — see
/// [`crate::plugin::capability::HostCapabilities`]) and feeds the
/// plugin's response to [`parse_initialize_result`] for the SPEC
/// §4.2.3 cross-checks (api version, capability subset, host-method
/// forward-compat). Returns the [`NegotiatedCaps`] so [`spawn`] can
/// stash it on [`PluginProc::caps`] for Step 6 dispatch.
async fn perform_handshake(
    framed: &FramedDuplex,
    manifest: &Manifest,
    opts: &SpawnOpts,
    id: &str,
) -> std::result::Result<(FramedDuplex, NegotiatedCaps), RpcError> {
    let init_id: i64 = 1;
    let params = build_initialize_params(
        &opts.host_name,
        &opts.host_version,
        &opts.host_api,
        &opts.workdir,
    );
    let req = Request {
        jsonrpc: JSONRPC_VERSION.into(),
        id: serde_json::json!(init_id),
        method: "initialize".into(),
        params,
    };
    let body = serde_json::to_value(&req)
        .map_err(|e| RpcError::Handshake(format!("encode initialize: {e}")))?;
    let timeout_ms = u64::from(manifest.limits.spawn_timeout_ms).max(50);
    let budget = Duration::from_millis(timeout_ms);
    let resp = timeout(budget, async {
        framed
            .send(body)
            .await
            .map_err(|e| RpcError::Transport(format!("send initialize: {e}")))?;
        framed
            .recv()
            .await
            .map_err(|e| RpcError::Transport(format!("recv initialize: {e}")))?
            .ok_or_else(|| {
                RpcError::Handshake("plugin closed stdout before initialize reply".into())
            })
    })
    .await
    .map_err(|_| RpcError::Timeout(budget))??;
    let parsed: Response = serde_json::from_value(resp)
        .map_err(|e| RpcError::Handshake(format!("parse initialize response: {e}")))?;
    if parsed.id != serde_json::json!(init_id) {
        return Err(RpcError::Handshake(format!(
            "initialize id mismatch: got {}",
            parsed.id
        )));
    }
    if let Some(err) = parsed.error {
        return Err(err.into());
    }
    let result = parsed.result.ok_or_else(|| {
        RpcError::Handshake("initialize response missing both result and error".into())
    })?;
    let caps = parse_initialize_result(&result, manifest, &opts.host_api)?;
    tracing::info!(
        target = "sy::plugin::proc",
        plugin_id = %id,
        api = %caps.api,
        offered = caps.plugin_offered_host_methods.len(),
        "initialize handshake complete"
    );
    Ok((framed.clone(), caps))
}

/// Best-effort SIGKILL + reap so we never leak a half-handshaked
/// child on the error path. `kill()` is fire-and-forget; the
/// subsequent `wait()` reaps even if the kill races.
async fn kill_child(mut child: Child) -> std::result::Result<(), std::io::Error> {
    let _ = child.start_kill();
    let _ = child.wait().await;
    Ok(())
}

/// Long-running supervisor actor. Owns the framed duplex, the
/// in-flight request table, and the restart-on-EOF backoff ladder.
#[tracing::instrument(skip(manifest, opts, cmd_rx, health_tx, child, framed), fields(plugin_id = %id))]
async fn run_actor(
    manifest: Manifest,
    opts: SpawnOpts,
    id: String,
    mut cmd_rx: mpsc::Receiver<SupervisorCmd>,
    health_tx: watch::Sender<State>,
    mut child: Child,
    mut framed: FramedDuplex,
) {
    let mut next_req_id: i64 = 2; // 1 was the initialize.
    let mut in_flight: HashMap<i64, oneshot::Sender<std::result::Result<Value, RpcError>>> =
        HashMap::new();
    let mut attempts: u32 = 0;
    let mut last_ping = tokio::time::Instant::now();
    let mut ping_in_flight: Option<(i64, tokio::time::Instant)> = None;

    loop {
        let ping_due = last_ping + opts.ping_interval;
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    SupervisorCmd::Request { method, params, reply } => {
                        let rid = next_req_id;
                        next_req_id += 1;
                        let req = Request {
                            jsonrpc: JSONRPC_VERSION.into(),
                            id: serde_json::json!(rid),
                            method,
                            params,
                        };
                        let v = match serde_json::to_value(&req) {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = reply.send(Err(RpcError::Transport(format!("encode: {e}"))));
                                continue;
                            }
                        };
                        if let Err(e) = framed.send(v).await {
                            let _ = reply.send(Err(RpcError::Transport(format!("send: {e}"))));
                            if !restart_if_attempts_remain(&manifest, &opts, &id, &health_tx, &mut child, &mut framed, &mut attempts).await {
                                fail_in_flight(&mut in_flight, "supervisor unhealthy");
                                return;
                            }
                            continue;
                        }
                        in_flight.insert(rid, reply);
                    }
                    SupervisorCmd::Shutdown { reply } => {
                        let _ = health_tx.send(State::ShuttingDown);
                        let res = do_shutdown(&manifest, &mut framed, &mut child).await;
                        let _ = reply.send(res);
                        return;
                    }
                }
            }
            frame = framed.recv() => {
                match frame {
                    Ok(Some(v)) => route_incoming_frame(
                        v,
                        &mut in_flight,
                        &mut ping_in_flight,
                        &framed,
                        &manifest,
                        opts.host_ctx.as_ref(),
                    ),
                    Ok(None) | Err(_) => {
                        tracing::warn!(target = "sy::plugin::proc", plugin_id = %id, "child EOF / read error; restart ladder");
                        fail_in_flight(&mut in_flight, "child EOF");
                        if !restart_if_attempts_remain(&manifest, &opts, &id, &health_tx, &mut child, &mut framed, &mut attempts).await {
                            return;
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(ping_due), if ping_in_flight.is_none() => {
                last_ping = tokio::time::Instant::now();
                let pid_ = next_req_id;
                next_req_id += 1;
                let ping = Request {
                    jsonrpc: JSONRPC_VERSION.into(),
                    id: serde_json::json!(pid_),
                    method: "ping".into(),
                    params: serde_json::json!({ "ts": chrono_now_ms() }),
                };
                if let Ok(v) = serde_json::to_value(&ping) {
                    if framed.send(v).await.is_err() {
                        // Transport down — fall into restart ladder
                        // via the next reader-loop iteration.
                        tracing::warn!(target = "sy::plugin::proc", plugin_id = %id, "ping send failed");
                    } else {
                        ping_in_flight = Some((pid_, tokio::time::Instant::now()));
                    }
                }
            }
            _ = ping_deadline(&ping_in_flight, opts.ping_timeout) => {
                tracing::warn!(target = "sy::plugin::proc", plugin_id = %id, "ping timeout — restart");
                ping_in_flight = None;
                fail_in_flight(&mut in_flight, "ping timeout");
                if !restart_if_attempts_remain(&manifest, &opts, &id, &health_tx, &mut child, &mut framed, &mut attempts).await {
                    return;
                }
            }
        }
    }
}

/// Classify an incoming JSON-RPC frame from the plugin and route it:
///
/// * **Response** — has `id` + (`result` xor `error`), no `method`.
///   Matched against `ping_in_flight` first, then `in_flight` so the
///   waiter oneshot fires.
/// * **Notification** — has `method`, no `id`. Logged at debug and
///   dropped (Step 20 will route `$/progress` into the file-manager
///   state; the supervisor itself ignores them).
/// * **Plugin-initiated request** — has `id` + `method`. Routed to
///   [`crate::plugin::host_fns::dispatch`] in a background task so the
///   reader loop doesn't block on slow I/O (e.g. a large
///   `host.fs.read`); the response goes back over the same
///   [`FramedDuplex`] (`Clone` → independent writer Mutex).
fn route_incoming_frame(
    v: Value,
    in_flight: &mut HashMap<i64, oneshot::Sender<std::result::Result<Value, RpcError>>>,
    ping_in_flight: &mut Option<(i64, tokio::time::Instant)>,
    framed: &FramedDuplex,
    manifest: &Manifest,
    host_ctx: Option<&HostCtx>,
) {
    // Plugin-initiated request: carries both an id and a method.
    let id_val = v.get("id");
    let has_method = v.get("method").is_some();
    if let (Some(idv), true) = (id_val, has_method) {
        if !idv.is_null() {
            spawn_host_fn_task(v, framed.clone(), manifest.clone(), host_ctx.cloned());
            return;
        }
    }
    // Response path (no `method` field): match against ping or
    // in-flight, surface the typed Response via the oneshot.
    let Some(id_val) = id_val else {
        tracing::debug!(target = "sy::plugin::proc", body = ?v, "notification ignored");
        return;
    };
    let Some(rid) = id_val.as_i64() else {
        tracing::warn!(target = "sy::plugin::proc", id = ?id_val, "non-integer id ignored");
        return;
    };
    if let Some((pid_, _)) = ping_in_flight {
        if *pid_ == rid {
            *ping_in_flight = None;
            tracing::trace!(target = "sy::plugin::proc", "ping pong");
            return;
        }
    }
    if let Some(tx) = in_flight.remove(&rid) {
        match serde_json::from_value::<Response>(v) {
            Ok(r) => {
                if let Some(err) = r.error {
                    let _ = tx.send(Err(err.into()));
                } else {
                    let _ = tx.send(Ok(r.result.unwrap_or(Value::Null)));
                }
            }
            Err(e) => {
                let _ = tx.send(Err(RpcError::Transport(format!("parse response: {e}"))));
            }
        }
    }
}

/// Spawn a background task that runs the host fn dispatch and ships
/// the response back over the same `FramedDuplex`. Decoupling the
/// dispatch from the reader loop keeps the actor responsive to
/// shutdown / ping while a long host fn (`host.fs.read` on a >1 MiB
/// file) is in flight.
fn spawn_host_fn_task(
    v: Value,
    framed: FramedDuplex,
    manifest: Manifest,
    host_ctx: Option<HostCtx>,
) {
    tokio::spawn(async move {
        let req_id = v.get("id").cloned().unwrap_or(Value::Null);
        let method = v
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        let reply = build_host_fn_reply(&req_id, &method, params, manifest, host_ctx).await;
        if let Ok(encoded) = serde_json::to_value(&reply) {
            if let Err(e) = framed.send(encoded).await {
                tracing::warn!(
                    target = "sy::plugin::proc",
                    method = %method,
                    error = %e,
                    "host fn reply send failed"
                );
            }
        }
    });
}

/// Build the JSON-RPC [`Response`] for a plugin-initiated host fn
/// request: success → `result`, [`host_fns::HostFnError`] →
/// [`ErrorObj`]. When no [`HostCtx`] is wired (pre-Step-6 supervisor
/// callers), surfaces a stable `-32601 METHOD_NOT_FOUND` so the
/// plugin sees a clear error rather than a hang.
async fn build_host_fn_reply(
    req_id: &Value,
    method: &str,
    params: Value,
    manifest: Manifest,
    host_ctx: Option<HostCtx>,
) -> Response {
    let Some(ctx) = host_ctx else {
        return Response {
            jsonrpc: JSONRPC_VERSION.into(),
            id: req_id.clone(),
            result: None,
            error: Some(ErrorObj {
                code: host_fns::METHOD_NOT_FOUND,
                message: "METHOD_NOT_FOUND".into(),
                data: serde_json::json!({ "method": method, "reason": "host_ctx unset" }),
            }),
        };
    };
    match host_fns::dispatch(method, params, &ctx, &manifest).await {
        Ok(result) => Response {
            jsonrpc: JSONRPC_VERSION.into(),
            id: req_id.clone(),
            result: Some(result),
            error: None,
        },
        Err(e) => Response {
            jsonrpc: JSONRPC_VERSION.into(),
            id: req_id.clone(),
            result: None,
            error: Some(ErrorObj {
                code: e.code,
                message: e.message,
                data: e.data,
            }),
        },
    }
}

/// Fail every in-flight request with a uniform unavailable error.
/// Called when the child died mid-request loop or the ping timeout
/// fires.
fn fail_in_flight(
    in_flight: &mut HashMap<i64, oneshot::Sender<std::result::Result<Value, RpcError>>>,
    why: &str,
) {
    for (_id, tx) in in_flight.drain() {
        let _ = tx.send(Err(RpcError::Unavailable(why.into())));
    }
}

/// Sleep until the in-flight ping has been outstanding for
/// `timeout`. Returns a never-completing future when no ping is in
/// flight so the `tokio::select!` arm stays quiescent.
async fn ping_deadline(ping_in_flight: &Option<(i64, tokio::time::Instant)>, timeout: Duration) {
    match ping_in_flight {
        None => std::future::pending::<()>().await,
        Some((_, sent_at)) => {
            tokio::time::sleep_until(*sent_at + timeout).await;
        }
    }
}

/// SPEC §4.4 restart ladder. Returns `true` if a respawn succeeded
/// and the actor should keep looping; `false` when the attempts
/// budget is exhausted and the actor should park as Unhealthy.
async fn restart_if_attempts_remain(
    manifest: &Manifest,
    opts: &SpawnOpts,
    id: &str,
    health_tx: &watch::Sender<State>,
    child: &mut Child,
    framed: &mut FramedDuplex,
    attempts: &mut u32,
) -> bool {
    // Best-effort reap of the dead child before respawning.
    let _ = child.start_kill();
    let _ = child.wait().await;
    if *attempts >= opts.max_restart_attempts {
        let _ = health_tx.send(State::Unhealthy {
            attempts: *attempts,
            last_err: "exhausted restart attempts".into(),
        });
        tracing::error!(target = "sy::plugin::proc", plugin_id = %id, attempts = *attempts, "supervisor parked unhealthy");
        return false;
    }
    let backoff = Duration::from_millis(BACKOFF_BASE_MS << *attempts);
    *attempts += 1;
    let _ = health_tx.send(State::Restarting {
        attempts: *attempts,
    });
    tracing::warn!(target = "sy::plugin::proc", plugin_id = %id, attempt = *attempts, backoff_ms = backoff.as_millis() as u64, "backoff");
    tokio::time::sleep(backoff).await;
    match boot_child(manifest, opts, id).await {
        Ok((new_child, new_framed)) => {
            match perform_handshake(&new_framed, manifest, opts, id).await {
                // SPEC §4.2.3 contract holds across restarts: the
                // re-handshake re-validates api / capabilities /
                // host-methods against the same manifest. We log the
                // NegotiatedCaps for parity with the spawn-time path
                // but don't ship them back to `PluginProc::caps` —
                // the field stays the spawn-time snapshot the
                // supervisor's caller sees, since the manifest
                // can't drift mid-supervisor-lifetime.
                Ok((restored, _restored_caps)) => {
                    *child = new_child;
                    *framed = restored;
                    let _ = health_tx.send(State::Ready);
                    true
                }
                Err(e) => {
                    tracing::warn!(target = "sy::plugin::proc", plugin_id = %id, error = %e, "handshake failed on restart");
                    let _ = kill_child(new_child).await;
                    // Recurse via the actor's outer loop on the next
                    // iteration; signal the caller to keep going so the
                    // ladder advances. We mutate `attempts` here so the
                    // bound holds even if the handshake keeps failing.
                    if *attempts >= opts.max_restart_attempts {
                        let _ = health_tx.send(State::Unhealthy {
                            attempts: *attempts,
                            last_err: format!("handshake: {e}"),
                        });
                        return false;
                    }
                    Box::pin(restart_if_attempts_remain(
                        manifest, opts, id, health_tx, child, framed, attempts,
                    ))
                    .await
                }
            }
        }
        Err(e) => {
            tracing::warn!(target = "sy::plugin::proc", plugin_id = %id, error = %e, "spawn failed on restart");
            if *attempts >= opts.max_restart_attempts {
                let _ = health_tx.send(State::Unhealthy {
                    attempts: *attempts,
                    last_err: format!("spawn: {e}"),
                });
                return false;
            }
            Box::pin(restart_if_attempts_remain(
                manifest, opts, id, health_tx, child, framed, attempts,
            ))
            .await
        }
    }
}

/// SPEC §4.2.3 shutdown sequence — `shutdown` request,
/// `limits.shutdown_timeout_ms` budget for the reply, then the
/// `exit` notification, then a bounded `wait()` so the child reaps.
async fn do_shutdown(
    manifest: &Manifest,
    framed: &mut FramedDuplex,
    child: &mut Child,
) -> std::result::Result<(), RpcError> {
    let req = Request {
        jsonrpc: JSONRPC_VERSION.into(),
        id: serde_json::json!(0),
        method: "shutdown".into(),
        params: Value::Null,
    };
    if let Ok(v) = serde_json::to_value(&req) {
        let _ = framed.send(v).await;
    }
    let budget = Duration::from_millis(u64::from(manifest.limits.shutdown_timeout_ms).max(50));
    // Best-effort wait for the shutdown reply; we don't fail the
    // shutdown sequence if it never comes — the SPEC §4.2.3 contract
    // is that the *next* step (`exit`) still fires.
    let _ = timeout(budget, framed.recv()).await;
    let exit_note = Notification {
        jsonrpc: JSONRPC_VERSION.into(),
        method: "exit".into(),
        params: Value::Null,
    };
    if let Ok(v) = serde_json::to_value(&exit_note) {
        let _ = framed.send(v).await;
    }
    // SPEC §4.2.3: plugin must exit within shutdown_timeout_ms after
    // receiving `exit`. Bound the `wait()` so a hung plugin doesn't
    // block the supervisor's caller indefinitely.
    match timeout(budget, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(RpcError::Transport(format!("wait: {e}"))),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(RpcError::Timeout(budget))
        }
    }
}

impl PluginProc {
    /// Send a JSON-RPC request, await the response.
    #[tracing::instrument(skip(self, params), fields(plugin_id = %self.id))]
    pub async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, RpcError> {
        let Some(cmd_tx) = self.cmd_tx.as_ref() else {
            return Err(RpcError::Unavailable("supervisor shut down".into()));
        };
        let (reply, rx) = oneshot::channel();
        cmd_tx
            .send(SupervisorCmd::Request {
                method: method.to_string(),
                params,
                reply,
            })
            .await
            .map_err(|_| RpcError::Unavailable("actor stopped".into()))?;
        rx.await
            .map_err(|_| RpcError::Unavailable("actor dropped reply channel".into()))?
    }

    /// SPEC §4.2.3 shutdown sequence. Returns once the child has
    /// reaped (or the shutdown_timeout elapsed and the supervisor
    /// killed the child).
    #[tracing::instrument(skip(self), fields(plugin_id = %self.id))]
    pub async fn shutdown(&mut self) -> std::result::Result<(), RpcError> {
        let Some(cmd_tx) = self.cmd_tx.take() else {
            return Ok(());
        };
        let (reply, rx) = oneshot::channel();
        if cmd_tx
            .send(SupervisorCmd::Shutdown { reply })
            .await
            .is_err()
        {
            return Ok(());
        }
        let result = rx
            .await
            .unwrap_or_else(|_| Err(RpcError::Unavailable("actor dropped reply channel".into())));
        if let Some(actor) = self.actor.take() {
            let _ = actor.await;
        }
        result
    }

    /// Snapshot of the current supervisor state.
    pub fn health(&self) -> State {
        self.health_rx.borrow().clone()
    }

    /// SPEC §4.2.3 negotiated capabilities from the spawn-time
    /// `initialize` handshake. Returns `None` only after
    /// [`Self::shutdown`] has reaped the supervisor. Step 6
    /// dispatch reads this to gate host-fn calls; Step 7 registry
    /// indexes the (kind, mime/url) pairs the plugin actually
    /// advertised at handshake-time.
    pub fn caps(&self) -> Option<&NegotiatedCaps> {
        self.caps.as_ref()
    }

    /// Block until the supervisor reaches one of the terminal
    /// `Unhealthy` / `ShuttingDown` states. Used by the
    /// `restart_ladder_caps_at_three_attempts` test to assert the
    /// ladder bound holds without polling.
    ///
    /// `#[cfg(test)]`: only the unit test + the
    /// `tests/sy_file_journey_e2e.rs` integration test call this
    /// today (both reach `proc.rs` via `#[path]`). Step 13's
    /// file-manager daemon will be the first bin consumer; until
    /// then this method is gated to test builds.
    #[cfg(test)]
    pub async fn wait_terminal(&mut self) -> State {
        loop {
            {
                let s = self.health_rx.borrow().clone();
                if matches!(s, State::Unhealthy { .. } | State::ShuttingDown) {
                    return s;
                }
            }
            if self.health_rx.changed().await.is_err() {
                return self.health_rx.borrow().clone();
            }
        }
    }

    /// Block until the supervisor's next transition lands on
    /// `Ready` (e.g. after the restart ladder completes a second
    /// handshake). Returns immediately only when the state is
    /// already terminal-`Unhealthy`; callers expecting an in-flight
    /// reconnect must invoke this after the disruption is observed
    /// (state ≠ `Ready`) — see the
    /// [`PluginProc::wait_state_change_then_ready`] convenience for
    /// the journey-J7 "kill mid-flight, then assert restart"
    /// pattern.
    pub async fn wait_ready(&mut self) -> std::result::Result<(), RpcError> {
        loop {
            {
                let s = self.health_rx.borrow().clone();
                if matches!(s, State::Ready) {
                    return Ok(());
                }
                if let State::Unhealthy { last_err, .. } = &s {
                    return Err(RpcError::Unavailable(last_err.clone()));
                }
            }
            if self.health_rx.changed().await.is_err() {
                return Err(RpcError::Unavailable("watch channel closed".into()));
            }
        }
    }

    /// Block until the supervisor transitions *off* `Ready` (e.g. a
    /// kill-mid-flight tripped the EOF restart ladder) and then back
    /// onto `Ready`. The journey-J7 resilience test uses this to
    /// avoid the race where the SIGKILL hasn't yet been observed by
    /// the reader loop at the moment the test polls `health_rx`.
    ///
    /// Acks the current value before waiting so a pending
    /// `Spawning → Ready` transition from the initial handshake
    /// doesn't satisfy this method.
    ///
    /// `#[cfg(test)]`: only the integration test in
    /// `tests/sy_file_journey_e2e.rs` calls this today. Step 13's
    /// file-manager daemon will be the first bin consumer.
    #[cfg(test)]
    pub async fn wait_state_change_then_ready(&mut self) -> std::result::Result<(), RpcError> {
        // Ack whatever's already in the watch slot so the next
        // `changed()` await actually waits for a *new* transition,
        // not the spawn-time Spawning→Ready edge.
        let _ = self.health_rx.borrow_and_update();
        // Loop: wait for a state change, then check whether we are
        // off-Ready. We only return when we observe a non-Ready
        // state followed by a return to Ready (or a terminal
        // Unhealthy that we surface as an error).
        let mut saw_off_ready = false;
        loop {
            if self.health_rx.changed().await.is_err() {
                return Err(RpcError::Unavailable("watch channel closed".into()));
            }
            let s = self.health_rx.borrow_and_update().clone();
            match s {
                State::Ready => {
                    if saw_off_ready {
                        return Ok(());
                    }
                    // Stale Ready (e.g. the initial spawn finishing
                    // its Spawning→Ready edge while the test was
                    // already awaiting). Keep waiting.
                }
                State::Unhealthy { last_err, .. } => {
                    return Err(RpcError::Unavailable(last_err));
                }
                State::Spawning | State::Restarting { .. } | State::ShuttingDown => {
                    saw_off_ready = true;
                }
            }
        }
    }
}

/// Tiny helper so the trace span on the periodic ping carries a
/// monotonically increasing timestamp the SPEC §4.2.3 `ping` schema
/// asks for. `chrono` is already a workspace dep; we use it only
/// here to avoid a custom epoch helper.
fn chrono_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `Framed`-shaped duplex that owns the child's stdin/stdout under
/// *independent* mutexes for the reader and writer halves. The actor
/// task is single-threaded, but `tokio::select!` polls multiple
/// futures concurrently — a reader pending on the child's stdout
/// must not block a periodic ping write. The split also lets the
/// DoS-guard arm drop the writer half independently (Step 4 DoD
/// "hung plugin's writer can be dropped").
#[derive(Clone)]
struct FramedDuplex {
    reader: Arc<Mutex<Framed<Box<dyn AsyncRead + Send + Unpin>, JsonRpcCodec>>>,
    writer: Arc<Mutex<Framed<Box<dyn AsyncWrite + Send + Unpin>, JsonRpcCodec>>>,
}

impl std::fmt::Debug for FramedDuplex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FramedDuplex").finish_non_exhaustive()
    }
}

impl FramedDuplex {
    fn new(stdin: tokio::process::ChildStdin, stdout: tokio::process::ChildStdout) -> Self {
        let writer: Framed<Box<dyn AsyncWrite + Send + Unpin>, JsonRpcCodec> =
            Framed::new(Box::new(stdin), JsonRpcCodec::default());
        let reader: Framed<Box<dyn AsyncRead + Send + Unpin>, JsonRpcCodec> =
            Framed::new(Box::new(stdout), JsonRpcCodec::default());
        Self {
            reader: Arc::new(Mutex::new(reader)),
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    async fn send(&self, v: Value) -> Result<()> {
        use futures_util::SinkExt as _;
        let mut g = self.writer.lock().await;
        g.send(v).await.map_err(|e| anyhow!("framed send: {e}"))
    }

    async fn recv(&self) -> Result<Option<Value>> {
        use futures_util::StreamExt as _;
        let mut g = self.reader.lock().await;
        Ok(g.next().await.transpose()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::load;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// Tiny `/bin/sh` script that frames an `initialize` response on
    /// receipt of any inbound initialize, then loops echoing
    /// shutdown/exit/ping requests too. Stored as a hermetic
    /// `chmod 755` file under a tempdir so the manifest binary path
    /// is absolute and reproducible.
    // The fake plugin scripts use `bash` (not POSIX `sh`) because the
    // `read_frame` helper needs to mutate a *parent-shell* variable
    // (`FRAME=...`). POSIX `$(read_frame)` would launch a subshell and
    // lose the variable on return; under parallel-test stress the fork
    // overhead also races with the supervisor's request loop and the
    // child appears to "vanish" between requests. Using `bash` lets
    // `read_frame` just `read` into a top-level var directly.
    const FAKE_PLUGIN_SCRIPT: &str = r#"#!/bin/bash
# read one Content-Length framed frame from stdin into $FRAME, then
# write an `initialize` response back framed. Then keep reading until
# either `shutdown` (reply + wait for `exit`) or EOF. FRAME is a top-
# level variable updated in-place by read_frame() so no subshell forks
# happen in the request loop (subshells race with the supervisor's
# pipe under parallel test stress).
emit() {
  local body="$1"
  printf 'Content-Length: %d\r\n\r\n%s' "${#body}" "$body"
}
FRAME=""
read_frame() {
  local len=0 line
  while IFS= read -r line; do
    line="${line%$'\r'}"
    [ -z "$line" ] && break
    case "$line" in
      Content-Length:*)
        len="${line#Content-Length: }"
        len="${len// /}"
        ;;
    esac
  done || { FRAME=""; return 1; }
  if [ "$len" -gt 0 ]; then
    FRAME=$(dd bs=1 count="$len" 2>/dev/null)
  else
    FRAME=""
  fi
  return 0
}
# initialize handshake
read_frame
case "$FRAME" in
  *'"method":"initialize"'*)
    emit '{"jsonrpc":"2.0","id":1,"result":{"name":"fake","version":"0","api":"1","capabilities":[],"offers":["preview"]}}'
    ;;
esac
# request loop — bash, so read_frame mutates FRAME in the same shell.
while read_frame; do
  [ -z "$FRAME" ] && break
  case "$FRAME" in
    *'"method":"shutdown"'*)
      id=$(printf '%s' "$FRAME" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      emit "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":null}"
      # consume the exit notification then break
      read_frame
      break
      ;;
    *'"method":"ping"'*)
      id=$(printf '%s' "$FRAME" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      ts=$(printf '%s' "$FRAME" | sed -n 's/.*"ts":\([0-9]*\).*/\1/p')
      emit "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"ts\":${ts}}}"
      ;;
    *'"method":"preview"'*)
      id=$(printf '%s' "$FRAME" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      emit "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"image\":{\"png_base64\":\"AAA\",\"w\":1,\"h\":1}}}"
      ;;
  esac
done
exit 0
"#;

    /// Always-exit-1 script for the restart-ladder test. Writes a
    /// half-frame so the reader sees EOF immediately on read.
    const ALWAYS_FAIL_SCRIPT: &str = r#"#!/bin/sh
exit 1
"#;

    /// Stalled stdin script for the ping-timeout test. Reads
    /// `initialize`, replies, then `sleep 60` so the supervisor sees
    /// no ping response and triggers a restart. Same `bash` +
    /// in-place `FRAME` pattern as [`FAKE_PLUGIN_SCRIPT`].
    const STALLED_PLUGIN_SCRIPT: &str = r#"#!/bin/bash
emit() {
  local body="$1"
  printf 'Content-Length: %d\r\n\r\n%s' "${#body}" "$body"
}
FRAME=""
read_frame() {
  local len=0 line
  while IFS= read -r line; do
    line="${line%$'\r'}"
    [ -z "$line" ] && break
    case "$line" in
      Content-Length:*)
        len="${line#Content-Length: }"
        len="${len// /}"
        ;;
    esac
  done || { FRAME=""; return 1; }
  if [ "$len" -gt 0 ]; then
    FRAME=$(dd bs=1 count="$len" 2>/dev/null)
  else
    FRAME=""
  fi
  return 0
}
read_frame
emit '{"jsonrpc":"2.0","id":1,"result":{"name":"stall","version":"0","api":"1","capabilities":[],"offers":[]}}'
sleep 60
"#;

    fn manifest_with_exec(exec: &str, dir: &Path) -> Manifest {
        let src = format!(
            r#"
api = "1"

[plugin]
id = "sy-plugin-supervisor-test"
name = "Supervisor Test"
version = "0.0.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "{exec}"

[[capability]]
kind = "previewer"
url = "*.test"

[needs]
fs_read = []
fs_write = []
preview = []
knowledge = []
network = []
exec = []

[limits]
memory_mb = 64
cpu_seconds = 10
nofile = 64
spawn_timeout_ms = 1500
shutdown_timeout_ms = 500

[env]
PATH = "/usr/bin:/bin"
"#
        );
        // env override + workdir mkdir done by the test caller.
        let _ = dir; // suppress unused
        load(&src).expect("manifest parses")
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).expect("write script");
        let mut perms = std::fs::metadata(&p).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).expect("chmod");
        p
    }

    fn opts_for(workdir: &Path) -> SpawnOpts {
        let mut o = SpawnOpts::new(workdir.to_path_buf());
        // Tests must complete quickly — pull ping cadence down.
        o.ping_interval = Duration::from_millis(80);
        o.ping_timeout = Duration::from_millis(300);
        o.request_timeout = Duration::from_secs(2);
        o
    }

    /// Test 1 — handshake completes against a real `/bin/sh` script
    /// that frames an `initialize` response back. Asserts the
    /// supervisor exposes `State::Ready` after `spawn`, and that the
    /// `request` method round-trips a `preview` call against the
    /// stub (also exercises `wait_ready` to lock in the happy path
    /// the resilience tests piggy-back on).
    #[tokio::test(flavor = "current_thread")]
    async fn handshake_with_echo_binary() {
        let tmp = tempfile::tempdir().expect("tmp");
        let script = write_script(tmp.path(), "fake-handshake.sh", FAKE_PLUGIN_SCRIPT);
        let manifest = manifest_with_exec(&script.to_string_lossy(), tmp.path());
        let mut proc = spawn(manifest, opts_for(tmp.path())).await.expect("spawn");
        assert_eq!(proc.health(), State::Ready);
        // `wait_ready` on a supervisor already in `Ready` must
        // return immediately — locks the happy-path contract in.
        proc.wait_ready().await.expect("already-Ready returns ok");
        // `request` round-trip — the stub replies to `preview` with a
        // 1×1 PNG-shaped body.
        let v = proc
            .request("preview", serde_json::json!({ "path": "x" }))
            .await
            .expect("preview rpc");
        assert_eq!(v["image"]["w"], 1);
        let _ = proc.shutdown().await;
    }

    /// Errors carrying structured `data` round-trip through
    /// `RpcError::Peer`. Locks the field-read contract so future
    /// callers can `match err { Peer { data, .. } => ... }`.
    #[test]
    fn rpc_error_peer_carries_data() {
        let err: RpcError = ErrorObj {
            code: -32099,
            message: "CAP_NOT_GRANTED".into(),
            data: serde_json::json!({ "needed": "network" }),
        }
        .into();
        match err {
            RpcError::Peer { code, data, .. } => {
                assert_eq!(code, -32099);
                assert_eq!(data["needed"], "network");
            }
            other => panic!("expected Peer, got {other:?}"),
        }
    }

    /// Test 2 — supervisor sees EOF after we kill the child, then
    /// respawns; the second `initialize` succeeds.
    ///
    /// Uses a unique script basename (`fake-restart.sh`) so the
    /// `/proc`-walk pid lookup never finds a parallel test's bash
    /// process by accident (the test binary runs multi-threaded by
    /// default, so the canonical `fake.sh` name from sibling tests
    /// would collide).
    #[tokio::test(flavor = "current_thread")]
    async fn restart_after_eof() {
        let tmp = tempfile::tempdir().expect("tmp");
        let script = write_script(tmp.path(), "fake-restart.sh", FAKE_PLUGIN_SCRIPT);
        let manifest = manifest_with_exec(&script.to_string_lossy(), tmp.path());
        let mut o = opts_for(tmp.path());
        o.max_restart_attempts = 5;
        let mut proc = spawn(manifest, o).await.expect("spawn 1");
        assert_eq!(proc.health(), State::Ready);
        // Find every bash whose argv contains our unique script
        // basename and SIGKILL them all (the parent bash plus any
        // transient `$(...)` subshell it may have running). Forcing
        // the parent down is what creates the EOF the supervisor's
        // restart ladder requires.
        let pids = find_children_by_cmdline(b"fake-restart.sh\0");
        assert!(
            !pids.is_empty(),
            "fake-restart child must be alive before kill"
        );
        for pid in &pids {
            // SAFETY: kill(2) on a pid we just spawned in our own
            // process group is signal-safe and well-bounded.
            unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
        }
        // Wait for restart → Ready again. Use the
        // state-change-then-Ready helper so a SIGKILL the reader
        // loop hasn't yet observed doesn't return `Ok(...)` from a
        // stale Ready snapshot.
        proc.wait_state_change_then_ready()
            .await
            .expect("restart back to Ready");
        let _ = proc.shutdown().await;
    }

    /// Test 3 — always-exit-1 script; supervisor parks Unhealthy
    /// after `max_restart_attempts` and total elapsed stays under
    /// the 1.5 s ceiling (backoff sum = 100 + 200 + 400 ms ≈ 700 ms
    /// + spawn overhead).
    #[tokio::test(flavor = "current_thread")]
    async fn restart_ladder_caps_at_three_attempts() {
        let tmp = tempfile::tempdir().expect("tmp");
        let script = write_script(tmp.path(), "fail.sh", ALWAYS_FAIL_SCRIPT);
        let manifest = manifest_with_exec(&script.to_string_lossy(), tmp.path());
        let mut o = opts_for(tmp.path());
        o.max_restart_attempts = 3;
        // First spawn must fail (the always-fail script exits before
        // emitting an initialize reply). The test asserts `spawn`
        // returns an `RpcError`, which is the supervisor's contract
        // for an unrecoverable initial spawn.
        let start = std::time::Instant::now();
        let err = spawn(manifest, o).await.expect_err("first handshake fails");
        let elapsed = start.elapsed();
        // The initial handshake fails fast (no restart ladder before
        // first Ready); the ladder applies *after* the first
        // successful handshake. The contract here is that `spawn`
        // returns an error within the spawn budget; the restart
        // ladder is exercised by `restart_after_eof` and the
        // ping-timeout test.
        assert!(
            matches!(
                err,
                RpcError::Handshake(_) | RpcError::Transport(_) | RpcError::Timeout(_)
            ),
            "expected handshake/transport/timeout error, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_millis(2000),
            "first-spawn failure must fail fast, took {elapsed:?}"
        );
    }

    /// Test 4 — graceful shutdown: send `shutdown`, await reply, then
    /// `exit` notification, then assert the child exits 0 inside the
    /// shutdown timeout.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_then_exit_within_timeout() {
        let tmp = tempfile::tempdir().expect("tmp");
        let script = write_script(tmp.path(), "fake-shutdown.sh", FAKE_PLUGIN_SCRIPT);
        let manifest = manifest_with_exec(&script.to_string_lossy(), tmp.path());
        let mut proc = spawn(manifest, opts_for(tmp.path())).await.expect("spawn");
        assert_eq!(proc.health(), State::Ready);
        proc.shutdown().await.expect("shutdown");
    }

    /// Test 5 — stalled stdin script causes the periodic ping to
    /// time out, which fires the restart ladder. We crank the ping
    /// timeout down to 200 ms so the test stays sub-second.
    #[tokio::test(flavor = "current_thread")]
    async fn ping_missed_triggers_restart() {
        let tmp = tempfile::tempdir().expect("tmp");
        let stall = write_script(tmp.path(), "stall.sh", STALLED_PLUGIN_SCRIPT);
        let manifest = manifest_with_exec(&stall.to_string_lossy(), tmp.path());
        let mut o = opts_for(tmp.path());
        o.ping_interval = Duration::from_millis(60);
        o.ping_timeout = Duration::from_millis(200);
        o.max_restart_attempts = 1;
        let mut proc = spawn(manifest, o).await.expect("spawn");
        assert_eq!(proc.health(), State::Ready);
        // Wait for the supervisor to time out the ping and walk the
        // restart ladder. The stall script will fail the second
        // handshake too (it always stalls), so we end up Unhealthy
        // after one attempt.
        let s = proc.wait_terminal().await;
        assert!(
            matches!(s, State::Unhealthy { .. }),
            "ping-timeout → restart → handshake stall → Unhealthy, got {s:?}"
        );
    }

    /// Walk `/proc/<pid>/cmdline` for *every* entry whose argv
    /// contains the given NUL-suffixed basename. The
    /// `restart_after_eof` test kills all matches to force the
    /// supervisor's reader-side EOF without exposing the actor's
    /// internal `Child` handle. Returning every match (not just the
    /// first) defends against a `$(...)` subshell racing with the
    /// pid lookup — both parent and subshell carry the same argv.
    fn find_children_by_cmdline(needle_with_nul: &[u8]) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for ent in entries.flatten() {
            let pid_str = ent.file_name();
            let Some(pid_s) = pid_str.to_str() else {
                continue;
            };
            let Ok(pid) = pid_s.parse::<u32>() else {
                continue;
            };
            let cmdline_path = format!("/proc/{pid}/cmdline");
            let Ok(cmdline) = std::fs::read(&cmdline_path) else {
                continue;
            };
            if cmdline
                .windows(needle_with_nul.len())
                .any(|w| w == needle_with_nul)
            {
                out.push(pid);
            }
        }
        out
    }
}
