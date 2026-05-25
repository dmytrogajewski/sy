//! sy-agentd: long-lived daemon owning all ACP child processes and serving
//! a Unix-socket protocol used by `sy agt …` clients. Wire format is IPC
//! v1 (SPEC §4.2) via `sy-ipc`; `agt.tail` opens a streaming response per
//! ROADMAP arch-ipc-v1 Step 6.

use std::{collections::HashMap, process::Stdio, sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sy_core::ErrorCode;
use sy_ipc::{
    BuildInfo, Capabilities, ErrorBody, Event, EventCodec, HealthFn, HealthSnapshot, HealthState,
    RequestCodec, Response, ResponseCodec, SystemMethods, SCHEMA_VERSION,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, Mutex},
    time::sleep,
};
use tokio_util::codec::{FramedRead, FramedWrite};
use ulid::Ulid;

use crate::agt::{
    acp::{AcpChild, AcpInbound},
    audit::{self, AuditDecision, AuditRecord},
    permission::{ask, ask_with_policy, Decision},
    policy::{ConsentDecision, ConsentError, ConsentStore, Resolver},
    protocol::{ClientReply, ClientReq, DaemonEvent, SessionInfo, SessionStatus, TranscriptEntry},
    registry,
    session::{entry_from_update, state_dir, Completion, Session},
    socket_path, wire,
};

type SharedSession = Arc<Mutex<Session>>;
type Sessions = Arc<Mutex<HashMap<String, SharedSession>>>;

pub fn run_blocking() -> Result<()> {
    // SPEC §4.6 / arch-observability Step 1: install the daemon's
    // tracing subscriber before the tokio runtime spawns threads
    // so every worker inherits it. `_obs_guard` lives for the
    // daemon's full lifetime; dropping it could lose buffered
    // log lines.
    let _obs_guard = sy_core::obs::init(sy_core::obs::Mode::Daemon { name: "sy-agentd" })?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run())
}

async fn run() -> Result<()> {
    let sock = socket_path();
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(&sock);

    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&sock)?.permissions();
        p.set_mode(0o600);
        let _ = std::fs::set_permissions(&sock, p);
    }
    eprintln!("sy-agentd: listening on {}", sock.display());

    // sy-mon Step 20: bind the agt plane's Prometheus UDS exposition
    // surface at $XDG_RUNTIME_DIR/sy/agt/metrics.sock alongside the
    // IPC socket. The shared installer in `sy_core::obs::mon_exporter`
    // needs an active tokio runtime — we are already inside one here,
    // so install directly and hold the guard for the daemon's
    // lifetime (the `_` binding keeps it alive until `run()` returns
    // or the process exits via the SIGTERM handler below). Bind
    // failure is non-fatal: the aggregator (Step 12) tolerates a
    // missing per-plane socket and `sy mon doctor` (Step 21) is the
    // alarm surface.
    #[cfg(feature = "mon-exporter")]
    let _mon_exporter = match install_mon_exporter().await {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!(
                target: "sy::agt::daemon",
                error = %format!("{e:#}"),
                "agt mon-exporter failed to bind; continuing without metrics socket"
            );
            None
        }
    };

    // SPEC §4.5 / arch-supervision Step 4: announce `READY=1` after
    // the listener is bound so `systemctl --user status sy-agentd`
    // flips from `activating` to `active (running)`. On non-systemd
    // hosts this is a no-op (no `NOTIFY_SOCKET`). The watchdog ping
    // task is spawned unconditionally — it returns `None` when
    // `WATCHDOG_USEC` isn't set, so dev runs don't burn a task.
    sy_core::notify::ready();
    let _watchdog = sy_core::notify::spawn_watchdog();

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    rehydrate_persisted(&sessions).await;
    let bridge = Arc::new(AgtBridge::new(sessions.clone()));

    // Graceful shutdown on SIGTERM / SIGINT.
    let shutdown_sessions = sessions.clone();
    let shutdown_sock = sock.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM");
        let mut intr = signal(SignalKind::interrupt()).expect("install SIGINT");
        tokio::select! {
            _ = term.recv() => {},
            _ = intr.recv() => {},
        }
        // SPEC §4.5 Step 4: emit `STOPPING=1 STATUS="draining"` before
        // the cleanup pass so siblings depending on us via `BindsTo=`
        // see a clean shutdown rather than a `Result=signal` failure.
        sy_core::notify::stopping();
        eprintln!("sy-agentd: shutting down");
        // Best-effort cancel of all sessions.
        let map = shutdown_sessions.lock().await;
        for (_, s) in map.iter() {
            let s = s.clone();
            tokio::spawn(async move {
                let s = s.lock().await;
                let acp_id = s.acp_session_id.clone();
                let child = s.child.clone();
                drop(s);
                let _ = child
                    .lock()
                    .await
                    .notify("session/cancel", json!({"sessionId": acp_id}))
                    .await;
            });
        }
        let _ = std::fs::remove_file(&shutdown_sock);
        std::process::exit(0);
    });

    let euid = rustix::process::geteuid().as_raw();
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("sy-agentd: accept error: {e}");
                sleep(Duration::from_millis(100)).await;
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
        let bridge = Arc::clone(&bridge);
        tokio::spawn(async move {
            handle_client(stream, bridge).await;
        });
    }
}

/// Bridge between the v1 IPC envelope and the in-process session
/// state. Composes `SystemMethods` for the reserved `system.*` surface
/// and dispatches `agt.*` methods into the existing handlers.
struct AgtBridge {
    sessions: Sessions,
    system: SystemMethods,
    /// arch-agent-sandbox Step 6: tokens parked here by
    /// `Decision::ConsentRequired` and resolved by the `agt.approve`
    /// IPC handler. Shared between every incoming connection so a
    /// `sy approve` from one connection wakes the original tool call
    /// blocked on another.
    consent: Arc<ConsentStore>,
}

impl AgtBridge {
    fn new(sessions: Sessions) -> Self {
        let cancel_registry = Arc::new(sy_ipc::CancelRegistry::new());
        let health_fn: HealthFn = Arc::new(|| HealthSnapshot {
            state: HealthState::Ready,
            status_line: "agentd ready".into(),
            queue_depth: 0,
            warm_models: Vec::new(),
        });
        let mut capabilities = Capabilities::baseline();
        capabilities.streaming = true; // `agt.tail` streams events.
        let build_info = BuildInfo {
            name: "sy-agentd".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            git_sha: option_env!("SY_GIT_SHA").unwrap_or("dev").into(),
        };
        let system = SystemMethods::new(
            build_info,
            health_fn,
            cancel_registry,
            capabilities,
            wire::ALL_METHODS.iter().map(|s| (*s).to_string()).collect(),
        );
        Self {
            sessions,
            system,
            consent: Arc::new(ConsentStore::new()),
        }
    }

    /// SPEC §4.4 step 2 (a): resolve a pending consent token. The
    /// approver's pid/uid come from the IPC connection's `SO_PEERCRED`
    /// — passed in so the audit record carries them alongside the
    /// original tool call's metadata.
    fn handle_approve(
        &self,
        token_str: &str,
        approver_pid: Option<u32>,
        approver_uid: Option<u32>,
        request_id: Option<Ulid>,
    ) -> std::result::Result<Value, (ErrorCode, String)> {
        let token = uuid::Uuid::parse_str(token_str).map_err(|e| {
            (
                ErrorCode::BadRequest,
                format!("invalid token {token_str:?}: {e}"),
            )
        })?;
        let snapshot = self.consent.snapshot(token);
        match self.consent.decide(token, ConsentDecision::Allow) {
            Ok(()) => {
                if let Some(snap) = snapshot {
                    emit_approve_audit(
                        snap,
                        approver_pid,
                        approver_uid,
                        request_id,
                        &audit::default_audit_dir(),
                    );
                }
                Ok(json!({ "approved": token.to_string() }))
            }
            Err(ConsentError::NotFound) => {
                Err((ErrorCode::BadRequest, format!("token {token} not found")))
            }
            Err(ConsentError::Expired) => {
                Err((ErrorCode::ConsentRequired, format!("token {token} expired")))
            }
        }
    }

    async fn handle_unary(&self, req: ClientReq, request_id: Option<Ulid>) -> Result<ClientReply> {
        match req {
            ClientReq::Run { agent, cwd, prompt } => {
                let id = start_session(
                    &self.sessions,
                    &agent,
                    &cwd,
                    &prompt,
                    Arc::clone(&self.consent),
                    request_id,
                )
                .await?;
                signal_waybar();
                Ok(ClientReply::RunReply { session_id: id })
            }
            ClientReq::List => {
                let map = self.sessions.lock().await;
                let mut infos = Vec::with_capacity(map.len());
                for s in map.values() {
                    infos.push(s.lock().await.info.clone());
                }
                drop(map);
                infos.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                Ok(ClientReply::ListReply { sessions: infos })
            }
            ClientReq::Prompt { session_id, text } => {
                send_prompt(&self.sessions, &session_id, &text).await?;
                Ok(ClientReply::Ack)
            }
            ClientReq::Stop { session_id } => {
                stop_session(&self.sessions, &session_id).await?;
                signal_waybar();
                Ok(ClientReply::Ack)
            }
            ClientReq::PermissionDecision { .. } => Ok(ClientReply::Ack),
            ClientReq::Diag => {
                let agents = registry::load().unwrap_or_default();
                let mut entries = Vec::new();
                for a in &agents {
                    let r = std::process::Command::new(&a.command)
                        .args(&a.version_args)
                        .stderr(Stdio::null())
                        .output();
                    entries.push(crate::agt::protocol::DiagEntry {
                        name: a.name.clone(),
                        command: a.command.clone(),
                        found: r.as_ref().map(|o| o.status.success()).unwrap_or(false),
                        version: r
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .map(|s| s.trim().to_string())
                            .unwrap_or_default(),
                    });
                }
                Ok(ClientReply::DiagReply { agents: entries })
            }
            ClientReq::Shutdown => Ok(ClientReply::Ack),
            ClientReq::Tail { .. } => unreachable!("tail flows through the streaming branch"),
        }
    }
}

async fn handle_client(stream: UnixStream, bridge: Arc<AgtBridge>) {
    let peer = stream
        .peer_cred()
        .ok()
        .map(|c| (c.pid().map(|p| p as u32), Some(c.uid())));
    let (reader, writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let initial = match buf_reader.fill_buf().await {
        Ok(b) if !b.is_empty() => b[0],
        _ => return,
    };
    if initial == b'{' || initial == b'[' || initial.is_ascii_whitespace() {
        reject_legacy_envelope(buf_reader, writer).await;
        return;
    }
    let mut req_stream = FramedRead::new(buf_reader, RequestCodec::default());
    let mut resp_sink = FramedWrite::new(writer, ResponseCodec::default());
    let req = match req_stream.next().await {
        Some(Ok(r)) => r,
        _ => return,
    };
    if let Some(resp) = bridge.system.try_handle(&req) {
        let _ = resp_sink.send(resp).await;
        return;
    }
    if req.method == wire::METHOD_APPROVE {
        let token = req
            .params
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or("");
        let (pid, uid) = peer.unwrap_or((None, None));
        let resp = match bridge.handle_approve(token, pid, uid, Some(req.request_id)) {
            Ok(result) => ok_response(req.request_id, result),
            Err((code, msg)) => err_response(req.request_id, code, msg),
        };
        let _ = resp_sink.send(resp).await;
        return;
    }
    let parsed = match wire::from_request(&req.method, &req.params) {
        Ok(r) => r,
        Err(e) => {
            let _ = resp_sink
                .send(err_response(
                    req.request_id,
                    ErrorCode::BadRequest,
                    e.to_string(),
                ))
                .await;
            return;
        }
    };
    match parsed {
        ClientReq::Tail {
            session_id,
            follow,
            replay,
        } => {
            if resp_sink
                .send(ok_response(
                    req.request_id,
                    serde_json::json!({ "streaming": true }),
                ))
                .await
                .is_err()
            {
                return;
            }
            let writer = resp_sink.into_inner();
            let mut event_sink = FramedWrite::new(writer, EventCodec::default());
            stream_tail_v1(
                &bridge.sessions,
                &session_id,
                follow,
                replay,
                &mut event_sink,
                req.request_id,
            )
            .await;
            let _ = event_sink.send(Event::closed(req.request_id)).await;
        }
        ClientReq::Shutdown => {
            let resp = match bridge
                .handle_unary(ClientReq::Shutdown, Some(req.request_id))
                .await
            {
                Ok(reply) => match wire::reply_to_result(&reply) {
                    Ok(result) => ok_response(req.request_id, result),
                    Err(e) => err_response(req.request_id, ErrorCode::Internal, e.to_string()),
                },
                Err(e) => err_response(req.request_id, ErrorCode::Internal, e.to_string()),
            };
            let _ = resp_sink.send(resp).await;
            let _ = resp_sink.flush().await;
            std::process::exit(0);
        }
        other => {
            let resp = match bridge.handle_unary(other, Some(req.request_id)).await {
                Ok(reply) => match wire::reply_to_result(&reply) {
                    Ok(result) => ok_response(req.request_id, result),
                    Err(e) => err_response(req.request_id, ErrorCode::Internal, e.to_string()),
                },
                Err(e) => err_response(req.request_id, ErrorCode::Internal, e.to_string()),
            };
            let _ = resp_sink.send(resp).await;
        }
    }
}

/// sy-mon Step 20: install the agt plane's Prometheus UDS exporter at
/// `$XDG_RUNTIME_DIR/sy/agt/metrics.sock`. Runs inside the daemon's
/// existing tokio runtime so the shared installer's accept task can
/// `tokio::spawn` onto it. Returns the `UdsGuard` that the daemon
/// holds for its lifetime — Drop unlinks the socket on shutdown.
#[cfg(feature = "mon-exporter")]
async fn install_mon_exporter() -> anyhow::Result<sy_core::obs::mon_exporter::UdsGuard> {
    let path = crate::mon_exporter::socket_path_for("agt")?;
    let guard = sy_core::obs::mon_exporter::install(path.clone())
        .map_err(|e| anyhow!("install agt mon-exporter at {}: {e}", path.display()))?;
    // SPEC §4 Security: tighten the socket file to 0600. Best-effort —
    // the parent directory's 0700 already restricts to the user, and
    // a chmod failure shouldn't kill daemon startup.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(guard.path()) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(guard.path(), perms);
        }
    }
    tracing::info!(
        target: "sy::agt::daemon",
        path = %guard.path().display(),
        "agt mon-exporter bound"
    );
    Ok(guard)
}

fn ok_response(request_id: Ulid, result: Value) -> Response {
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
            details: Value::Null,
        },
    }
}

async fn reject_legacy_envelope(
    mut reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
) {
    let mut sink = [0u8; 4096];
    let _ = reader.read(&mut sink).await;
    let err = Response::Err {
        schema_version: SCHEMA_VERSION,
        request_id: Ulid::new(),
        error: ErrorBody {
            code: ErrorCode::IncompatibleSchema,
            message: "legacy line-JSON IPC is no longer accepted; speak sy-ipc v1".into(),
            retry_after_ms: None,
            details: Value::Null,
        },
    };
    if let Ok(line) = serde_json::to_string(&err) {
        let _ = writer.write_all(line.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
        let _ = writer.flush().await;
    }
    let _ = writer.shutdown().await;
}

async fn start_session(
    sessions: &Sessions,
    agent: &str,
    cwd: &std::path::Path,
    prompt: &str,
    consent: Arc<ConsentStore>,
    request_id: Option<Ulid>,
) -> Result<String> {
    let spec = registry::find(agent)?;
    let child = AcpChild::spawn(&spec, cwd).await?;

    // initialize → session/new → session/prompt
    let _ = child
        .request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false
                }
            }),
        )
        .await
        .context("acp initialize")?;

    let new = child
        .request(
            "session/new",
            json!({
                "cwd": cwd.display().to_string(),
                "mcpServers": []
            }),
        )
        .await
        .context("acp session/new")?;
    let acp_session_id = new
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("session/new missing sessionId: {new}"))?
        .to_owned();

    let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let summary = prompt.chars().take(80).collect::<String>();
    let mut session = Session::new(
        id.clone(),
        agent.to_string(),
        cwd.to_path_buf(),
        summary,
        child,
        acp_session_id.clone(),
    )?;
    session.originating_request_id = request_id;
    session.append(TranscriptEntry::UserText {
        text: prompt.to_string(),
    });
    let _ = session.set_status(SessionStatus::Running);
    let shared: SharedSession = Arc::new(Mutex::new(session));
    sessions.lock().await.insert(id.clone(), shared.clone());
    write_index(sessions).await;

    // Move the inbound channel out of the AcpChild so we can drive it from
    // the per-session task without holding the Session lock continuously.
    let inbound = {
        let s = shared.lock().await;
        let mut c = s.child.lock().await;
        std::mem::replace(&mut c.inbound, mpsc::channel(1).1)
    };

    // Per-session inbound dispatch task.
    let task_session = shared.clone();
    let task_acp_id = acp_session_id.clone();
    let task_consent = Arc::clone(&consent);
    tokio::spawn(async move {
        run_inbound_loop(task_session, task_acp_id, inbound, task_consent).await;
    });

    // Fire the initial prompt.
    let prompt_text = prompt.to_string();
    let prompt_session = shared.clone();
    let prompt_acp_id = acp_session_id;
    tokio::spawn(async move {
        let child = {
            let s = prompt_session.lock().await;
            s.child.clone()
        };
        let _ = prompt_session
            .lock()
            .await
            .set_status(SessionStatus::Working);
        let res = child
            .lock()
            .await
            .request(
                "session/prompt",
                json!({
                    "sessionId": prompt_acp_id,
                    "prompt": [{"type": "text", "text": prompt_text}]
                }),
            )
            .await;
        let mut s = prompt_session.lock().await;
        let completion = match res {
            Ok(_) => s.set_status(SessionStatus::Running),
            Err(e) => s.set_status(SessionStatus::Error { msg: e.to_string() }),
        };
        if let Some(kind) = completion {
            notify_completion(&s.info, kind);
        }
        drop(s);
        signal_waybar();
    });

    Ok(id)
}

async fn run_inbound_loop(
    session: SharedSession,
    acp_session_id: String,
    mut inbound: mpsc::Receiver<AcpInbound>,
    consent: Arc<ConsentStore>,
) {
    while let Some(msg) = inbound.recv().await {
        match msg {
            AcpInbound::Notification { method, params } if method == "session/update" => {
                let update = params.get("update").cloned().unwrap_or(Value::Null);
                if let Some(entry) = entry_from_update(&update) {
                    session.lock().await.append(entry);
                    signal_waybar();
                }
            }
            AcpInbound::Notification { .. } => { /* ignore */ }
            AcpInbound::Request { id, method, params }
                if method == "session/request_permission" =>
            {
                handle_permission(session.clone(), id, params, Arc::clone(&consent)).await;
            }
            AcpInbound::Request { id, .. } => {
                let child = { session.lock().await.child.clone() };
                let _ = child
                    .lock()
                    .await
                    .respond(id, Err(anyhow!("method not implemented")))
                    .await;
            }
            AcpInbound::Closed => {
                let mut s = session.lock().await;
                let id = s.info.id.clone();
                let completion = s.set_status(SessionStatus::Stopped { code: 0 });
                s.broadcast(DaemonEvent::Closed {
                    session_id: id,
                    reason: "agent process exited".into(),
                });
                if let Some(kind) = completion {
                    notify_completion(&s.info, kind);
                }
                signal_waybar();
                break;
            }
        }
    }
    let _ = acp_session_id;
}

async fn handle_permission(
    session: SharedSession,
    req_id: Value,
    params: Value,
    consent: Arc<ConsentStore>,
) {
    let summary = params
        .get("toolCall")
        .and_then(|t| t.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("permission request")
        .to_string();
    let body = params
        .get("toolCall")
        .and_then(|t| t.get("kind").or_else(|| t.get("rawInput")))
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_else(|| "approve tool call?".into());

    let req_uuid = uuid::Uuid::new_v4().simple().to_string();
    let (session_cwd, originating_request_id) = {
        let mut s = session.lock().await;
        let _ = s.set_status(SessionStatus::Awaiting);
        let session_id = s.info.id.clone();
        s.broadcast(DaemonEvent::Permission {
            session_id,
            request_id: req_uuid.clone(),
            summary: summary.clone(),
            body: body.clone(),
        });
        (s.info.cwd.clone(), s.originating_request_id)
    };
    signal_waybar();

    // Step 1 of arch-agent-sandbox: consult the policy resolver before
    // notify-send. Policy is loaded from `<cwd>/configs/policy/`
    // (the daemon's session-level cwd doubles as the `$REPO` root).
    // If the policy tree isn't present, behaviour stays identical to
    // the pre-resolver flow — a bare `ask()`. Step 6 routes `EveryCall`
    // consent through the in-daemon `ConsentStore` (TTY-driven
    // `sy approve <token>`); `OncePerSession` keeps the notify-send
    // fallback inside `ask_with_policy`.
    let (tool, argv) = extract_tool_and_argv(&params);
    let decision = resolve_permission(
        &session_cwd,
        &tool,
        &argv,
        &summary,
        &body,
        consent.as_ref(),
        originating_request_id,
    )
    .await;

    // Pick optionId based on decision and the options the agent offered.
    let options = params
        .get("options")
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();
    let pick = |needles: &[&str]| -> Option<String> {
        for o in &options {
            let id = o.get("optionId").and_then(|v| v.as_str()).unwrap_or("");
            let kind = o.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            for n in needles {
                if id.contains(n) || kind.contains(n) {
                    return Some(id.to_string());
                }
            }
        }
        None
    };
    let option_id = match decision {
        Decision::Allow => pick(&["allow", "accept"]).unwrap_or_else(|| "allow_once".into()),
        Decision::Deny => {
            pick(&["reject", "deny", "cancel"]).unwrap_or_else(|| "reject_once".into())
        }
    };

    let outcome = json!({
        "outcome": {
            "outcome": "selected",
            "optionId": option_id
        }
    });
    let child = { session.lock().await.child.clone() };
    let _ = child.lock().await.respond(req_id, Ok(outcome)).await;
    {
        let mut s = session.lock().await;
        let _ = s.set_status(SessionStatus::Working);
        s.append(TranscriptEntry::Status {
            msg: format!(
                "permission {}: {}",
                if matches!(decision, Decision::Allow) {
                    "allowed"
                } else {
                    "denied"
                },
                summary
            ),
        });
    }
    signal_waybar();
}

/// SPEC §4.4 step 2: the operator's wall-clock budget for approving a
/// strict-profile consent token. Long enough to switch terminals,
/// short enough that a forgotten request doesn't pin the agent for
/// hours.
const CONSENT_TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
const NOTIFY_SEND_TIMEOUT: Duration = Duration::from_secs(8);

/// Emit the consent-approval audit record for `agt.approve`. Extracted
/// from [`AgtBridge::handle_approve`] so the unit test can target a
/// tempdir without env-mutation; production calls pass
/// [`audit::default_audit_dir`]. `request_id` is the IPC v1 envelope
/// of the `agt.approve` call itself, threaded through so journald
/// (`SY_REQUEST_ID`) and JSONL correlate the consent decision back to
/// the approving call.
fn emit_approve_audit(
    snap: crate::agt::policy::consent::PendingSnapshot,
    approver_pid: Option<u32>,
    approver_uid: Option<u32>,
    request_id: Option<Ulid>,
    audit_dir: &std::path::Path,
) {
    let trace_id = sy_core::obs::current_trace_ctx().map(|c| c.trace_id.0);
    let remaining_ms = snap
        .expires_at
        .saturating_duration_since(std::time::Instant::now())
        .as_millis();
    let reason = Some(format!(
        "approver pid={} uid={} ttl_remaining_ms={}",
        approver_pid.unwrap_or(0),
        approver_uid.unwrap_or(0),
        remaining_ms,
    ));
    audit::emit(
        &AuditRecord::now(
            snap.tool,
            snap.policy_diff,
            AuditDecision::Consent,
            snap.argv,
        )
        .with_reason(reason)
        .with_trace_id(trace_id)
        .with_request_id(request_id),
        audit_dir,
    );
}

/// Three-way orchestration for a permission verdict — SPEC §4.4 step 2.
///
/// 1. No resolver → bare `notify-send` (legacy fallback).
/// 2. Resolver returns `Allow`/`Deny` → audit + short-circuit.
/// 3. Resolver returns `ConsentRequired`:
///    * `EveryCall` → mint a token in the consent store and await
///      the operator's `sy approve <token>` (TTY-driven).
///    * `OncePerSession` / `Never` → existing `notify-send` flow via
///      [`ask_with_policy`].
async fn resolve_permission(
    cwd: &std::path::Path,
    tool: &str,
    argv: &[String],
    summary: &str,
    body: &str,
    consent: &ConsentStore,
    request_id: Option<Ulid>,
) -> Decision {
    let Some(resolver) = load_session_resolver(cwd, tool) else {
        return ask(summary, body, NOTIFY_SEND_TIMEOUT).await;
    };
    let profile = resolver.effective(tool);
    let every_call = matches!(
        profile.require_consent,
        crate::agt::policy::schema::ConsentMode::EveryCall
    );
    if !every_call {
        return ask_with_policy(
            &resolver,
            tool,
            argv,
            summary,
            body,
            NOTIFY_SEND_TIMEOUT,
            request_id,
        )
        .await;
    }
    // EveryCall (strict default): bypass notify-send and require a
    // TTY-driven `sy approve <token>`. Audit the consent request
    // before parking so a crash mid-wait still records that the call
    // paused.
    let verdict = resolver.decide(tool, argv);
    if !matches!(
        verdict,
        crate::agt::policy::Decision::ConsentRequired { .. }
    ) {
        return ask_with_policy(
            &resolver,
            tool,
            argv,
            summary,
            body,
            NOTIFY_SEND_TIMEOUT,
            request_id,
        )
        .await;
    }
    let policy_diff = resolver.fingerprint();
    let trace_id = sy_core::obs::current_trace_ctx().map(|c| c.trace_id.0);
    let audit_dir = audit::default_audit_dir();
    audit::emit(
        &AuditRecord::now(tool, &policy_diff, AuditDecision::Consent, argv.to_vec())
            .with_reason(Some(
                "EveryCall consent: awaiting sy approve <token>".into(),
            ))
            .with_trace_id(trace_id)
            .with_request_id(request_id),
        &audit_dir,
    );
    let (token, rx) = consent.issue(tool, argv, policy_diff, CONSENT_TOKEN_TTL);
    tracing::info!(
        target: "sy::agt::consent",
        token = %token,
        tool = tool,
        "consent token issued; run `sy approve {}`",
        token
    );
    match rx.await {
        Ok(ConsentDecision::Allow) => Decision::Allow,
        // Receiver error → sender dropped (token expired and was
        // swept by `cleanup_expired`, or the daemon shut down).
        // Treat as deny so the agent doesn't proceed.
        Err(_) => Decision::Deny,
    }
}

/// Pull the tool name + argv out of the ACP `session/request_permission`
/// params for policy resolution. ACP passes the tool kind/name in
/// `toolCall.kind` (e.g. `"execute"`) and the structured arguments in
/// `toolCall.rawInput` — we serialise the rawInput JSON into a single
/// argv slot so glob patterns like `"*"` match the way the SPEC
/// examples expect. Best-effort: missing fields fall back to empty.
fn extract_tool_and_argv(params: &Value) -> (String, Vec<String>) {
    let tool_call = params.get("toolCall");
    let tool = tool_call
        .and_then(|t| t.get("kind").or_else(|| t.get("title")))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let raw_input = tool_call.and_then(|t| t.get("rawInput"));
    let argv = match raw_input {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
            .collect(),
        Some(other) => vec![other.to_string()],
        None => Vec::new(),
    };
    (tool, argv)
}

/// Try to load the `normal` profile for this session. Returns `None`
/// if the policy tree isn't present alongside the session's cwd —
/// keeping Step 1 a strictly additive change (no behaviour drift for
/// users without `configs/policy/`).
fn load_session_resolver(cwd: &std::path::Path, tool: &str) -> Option<Resolver> {
    let policy_root = cwd.join("configs").join("policy");
    if !policy_root.join("profiles").join("normal.toml").is_file() {
        return None;
    }
    let tool_key = std::path::Path::new(tool)
        .file_stem()
        .and_then(|s| s.to_str());
    let resolver = Resolver::load(&policy_root, "normal", tool_key, cwd).ok()?;
    // SPEC §4.4 step 2: log the resolved policy fingerprint so audit
    // consumers can correlate a decision with the exact policy bytes
    // in effect. Step 5 lifts this onto the journald audit record.
    tracing::debug!(
        policy_sha = resolver.fingerprint().as_str(),
        tool = tool,
        "policy resolver loaded"
    );
    Some(resolver)
}

async fn send_prompt(sessions: &Sessions, session_id: &str, text: &str) -> Result<()> {
    let shared = sessions
        .lock()
        .await
        .get(session_id)
        .cloned()
        .ok_or_else(|| anyhow!("no such session: {session_id}"))?;
    let (child, acp_id) = {
        let s = shared.lock().await;
        (s.child.clone(), s.acp_session_id.clone())
    };
    {
        let mut s = shared.lock().await;
        s.append(TranscriptEntry::UserText {
            text: text.to_string(),
        });
        let _ = s.set_status(SessionStatus::Working);
    }
    let session_for_status = shared.clone();
    let text = text.to_string();
    tokio::spawn(async move {
        let res = child
            .lock()
            .await
            .request(
                "session/prompt",
                json!({
                    "sessionId": acp_id,
                    "prompt": [{"type": "text", "text": text}]
                }),
            )
            .await;
        let mut s = session_for_status.lock().await;
        let completion = match res {
            Ok(_) => s.set_status(SessionStatus::Running),
            Err(e) => s.set_status(SessionStatus::Error { msg: e.to_string() }),
        };
        if let Some(kind) = completion {
            notify_completion(&s.info, kind);
        }
        drop(s);
        signal_waybar();
    });
    Ok(())
}

async fn stop_session(sessions: &Sessions, session_id: &str) -> Result<()> {
    let shared = sessions
        .lock()
        .await
        .get(session_id)
        .cloned()
        .ok_or_else(|| anyhow!("no such session: {session_id}"))?;
    let (child, acp_id) = {
        let s = shared.lock().await;
        (s.child.clone(), s.acp_session_id.clone())
    };
    let _ = child
        .lock()
        .await
        .notify("session/cancel", json!({"sessionId": acp_id}))
        .await;
    sleep(Duration::from_millis(200)).await;
    {
        let mut s = shared.lock().await;
        let id = s.info.id.clone();
        let completion = s.set_status(SessionStatus::Stopped { code: 0 });
        s.broadcast(DaemonEvent::Closed {
            session_id: id,
            reason: "stopped".into(),
        });
        if let Some(kind) = completion {
            notify_completion(&s.info, kind);
        }
    }
    sessions.lock().await.remove(session_id);
    write_index(sessions).await;
    Ok(())
}

async fn stream_tail_v1(
    sessions: &Sessions,
    session_id: &str,
    follow: bool,
    replay: bool,
    sink: &mut FramedWrite<tokio::net::unix::OwnedWriteHalf, EventCodec>,
    request_id: Ulid,
) {
    let shared = match sessions.lock().await.get(session_id).cloned() {
        Some(s) => s,
        None => {
            // The initial `Response::Ok` ack already went out, so we
            // can't downgrade to a `Response::Err`. Surface the lookup
            // failure as a `kind: error` event the caller decodes
            // before the `closed` sentinel.
            let _ = sink
                .send(Event {
                    schema_version: SCHEMA_VERSION,
                    request_id,
                    kind: "error".into(),
                    params: serde_json::json!({
                        "code": "NotFound",
                        "message": format!("no such session: {session_id}"),
                    }),
                })
                .await;
            return;
        }
    };
    let (events, mut rx) = if replay {
        let mut s = shared.lock().await;
        let events = s.replay();
        let rx = if follow { Some(s.subscribe()) } else { None };
        (events, rx)
    } else {
        let mut s = shared.lock().await;
        let rx = if follow { Some(s.subscribe()) } else { None };
        (Vec::new(), rx)
    };
    for e in events {
        if sink.send(event_to_stream(&e, request_id)).await.is_err() {
            return;
        }
    }
    if let Some(ref mut rx) = rx {
        while let Some(e) = rx.recv().await {
            if sink.send(event_to_stream(&e, request_id)).await.is_err() {
                return;
            }
        }
    }
}

fn event_to_stream(event: &DaemonEvent, request_id: Ulid) -> Event {
    let (kind, params) = wire::event_to_stream_payload(event);
    Event {
        schema_version: SCHEMA_VERSION,
        request_id,
        kind: kind.to_string(),
        params,
    }
}

fn signal_waybar() {
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg("pkill -RTMIN+9 waybar 2>/dev/null")
        .status();
}

fn notify_completion(info: &SessionInfo, kind: Completion) {
    let (urgency, summary) = match kind {
        Completion::TurnDone => ("normal", format!("agt {} idle", info.agent)),
        Completion::Stopped => ("normal", format!("agt {} stopped", info.agent)),
        Completion::Errored => ("critical", format!("agt {} error", info.agent)),
    };
    let body = match &info.status {
        SessionStatus::Error { msg } => format!("[{}] {msg}", info.id),
        _ => format!("[{}] {}", info.id, info.summary),
    };
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "sy", "-u", urgency, &summary, &body])
        .spawn();
}

async fn write_index(sessions: &Sessions) {
    let dir = state_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let map = sessions.lock().await;
    let ids: Vec<String> = map.keys().cloned().collect();
    let _ = std::fs::write(
        dir.join("index.json"),
        serde_json::to_string_pretty(&ids).unwrap_or_else(|_| "[]".into()),
    );
}

/// Read existing on-disk session metadata. We surface them in `List` with
/// status flipped to `Stopped { reason: "daemon restart" }`. v1 does not
/// auto-resume — `session/load` is left for a future explicit `sy agt resume`.
async fn rehydrate_persisted(_sessions: &Sessions) {
    // Intentional no-op for v1: persisted transcripts remain on disk but
    // don't appear in `List` — keeping the live view clean. Future work
    // wires `session/load` and exposes them.
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use futures_util::StreamExt;
    use std::path::PathBuf;
    use sy_ipc::{CallOpts, Client as IpcClient, PROTOCOL_VERSION};

    /// Synthetic session that satisfies `stream_tail_v1` without
    /// involving a real ACP child. We spawn a tiny stdin-reading
    /// `cat` process so `AcpChild::spawn` (which captures stdio) has
    /// something to bind to; the daemon never sends ACP traffic
    /// because the test only exercises `agt.tail` / `agt.list` /
    /// `system.describe`.
    async fn fake_session(id: &str, dir: PathBuf, entries: Vec<TranscriptEntry>) -> SharedSession {
        use crate::agt::registry::AgentSpec;
        let spec = AgentSpec {
            name: "test".into(),
            command: "/bin/cat".into(),
            args: Vec::new(),
            env: Default::default(),
            version_args: Vec::new(),
            // Synthetic test setup: `cat` stands in for an ACP stdio
            // stream. Skipping the sandbox avoids needing a real
            // systemd-run + policy tree under the tempdir.
            sandbox_profile: None,
        };
        let child = AcpChild::spawn(&spec, &dir).await.expect("spawn cat");
        let now = Utc::now().to_rfc3339();
        let info = SessionInfo {
            id: id.into(),
            agent: "test".into(),
            cwd: dir.clone(),
            status: SessionStatus::Running,
            created_at: now.clone(),
            last_activity: now.clone(),
            summary: "fake".into(),
        };
        let transcript: Vec<(String, TranscriptEntry)> = entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| (format!("t{i}"), e))
            .collect();
        Arc::new(Mutex::new(Session {
            info,
            child: Arc::new(Mutex::new(child)),
            acp_session_id: "acp-test".into(),
            transcript,
            subscribers: Vec::new(),
            jsonl: None,
            dir,
            originating_request_id: None,
        }))
    }

    async fn spawn_test_listener(
        sock: std::path::PathBuf,
        sessions: Sessions,
    ) -> tokio::task::JoinHandle<()> {
        let std_listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        std_listener.set_nonblocking(true).expect("nonblock");
        let listener = UnixListener::from_std(std_listener).expect("from_std");
        let bridge = Arc::new(AgtBridge::new(sessions));
        tokio::spawn(async move {
            let euid = rustix::process::geteuid().as_raw();
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == euid => {}
                    _ => continue,
                }
                let bridge = Arc::clone(&bridge);
                tokio::spawn(async move {
                    handle_client(stream, bridge).await;
                });
            }
        })
    }

    #[tokio::test]
    async fn agt_ipc_v1_run_session() {
        // ROADMAP arch-ipc-v1 Step 6 DoD: streaming `agt.tail`
        // returns three `Event` frames followed by the `closed`
        // sentinel. We pre-populate a synthetic session with three
        // transcript entries so `replay()` emits exactly three
        // events; `follow=false` ensures the daemon writes the
        // sentinel as soon as the replay completes.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp.path().join("agentd.sock");
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let entries = vec![
            TranscriptEntry::AgentText { text: "1".into() },
            TranscriptEntry::AgentText { text: "2".into() },
            TranscriptEntry::AgentText { text: "3".into() },
        ];
        let shared = fake_session("abc", session_dir, entries).await;
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().await.insert("abc".into(), shared);
        let server = spawn_test_listener(sock.clone(), sessions.clone()).await;

        let mut client = IpcClient::connect(&sock).await.expect("connect");
        let (method, params) = wire::to_request(&ClientReq::Tail {
            session_id: "abc".into(),
            follow: false,
            replay: true,
        });
        let ack = client
            .call(method, params, CallOpts::default())
            .await
            .expect("tail ack");
        assert!(
            matches!(ack, Response::Ok { .. }),
            "tail must ack with Ok, got {ack:?}"
        );

        let mut events = client.into_event_stream();
        let mut transcript_count = 0;
        let mut closed = false;
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(decoded) = events.next().await {
                let evt = decoded.expect("event frame");
                if evt.is_closed() {
                    closed = true;
                    return;
                }
                if evt.kind == wire::EVENT_TRANSCRIPT {
                    transcript_count += 1;
                }
            }
        })
        .await;
        result.expect("events arrived within 2s");
        assert_eq!(transcript_count, 3, "three transcript events");
        assert!(closed, "stream ended with closed sentinel");

        server.abort();
    }

    #[tokio::test]
    async fn agt_ipc_v1_describe_streaming_capability() {
        // ROADMAP Step 6 DoD slice: `system.describe` against the agt
        // daemon advertises schema_version=1, a non-empty methods
        // array, and the streaming capability that distinguishes
        // it from the unary daemons. The cross-daemon
        // `all_daemons_describe` lives in `src/stack/ipc.rs`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp.path().join("agentd.sock");
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let server = spawn_test_listener(sock.clone(), sessions).await;

        let mut client = IpcClient::connect(&sock).await.expect("connect");
        let resp = client
            .call("system.describe", json!({}), CallOpts::default())
            .await
            .expect("system.describe call");
        match resp {
            Response::Ok { result, .. } => {
                assert_eq!(
                    result["protocol_version"].as_u64(),
                    Some(u64::from(PROTOCOL_VERSION))
                );
                let methods: Vec<&str> = result["methods"]
                    .as_array()
                    .expect("methods array")
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect();
                assert!(methods.contains(&"agt.run"));
                assert!(methods.contains(&"agt.tail"));
                assert!(methods.contains(&"system.describe"));
                assert_eq!(
                    result["capabilities"]["streaming"],
                    json!(true),
                    "agt advertises streaming capability"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn agt_list_round_trip_via_v1_envelope() {
        // Sanity: a unary `agt.list` call round-trips with an empty
        // sessions map, returning `ListReply { sessions: [] }`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp.path().join("agentd.sock");
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let server = spawn_test_listener(sock.clone(), sessions).await;

        let mut client = IpcClient::connect(&sock).await.expect("connect");
        let (method, params) = wire::to_request(&ClientReq::List);
        let resp = client
            .call(method, params, CallOpts::default())
            .await
            .expect("list call");
        match resp {
            Response::Ok { result, .. } => {
                assert_eq!(
                    result["sessions"],
                    json!([]),
                    "empty sessions map returns empty array"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        server.abort();
    }

    /// arch-ipc-v1 Step 6 / arch-agent-sandbox follow-up: the
    /// `agt.approve` audit record must carry the approving call's
    /// IPC envelope `request_id` so journald (`SY_REQUEST_ID`) and
    /// JSONL queries correlate the consent decision back to the
    /// originating call. Targets [`emit_approve_audit`] with a
    /// tempdir so the unit test stays hermetic.
    #[test]
    fn consent_decision_audit_carries_request_id() {
        use crate::agt::policy::consent::PendingSnapshot;
        let tmp = tempfile::tempdir().expect("tempdir");
        let request_id = Ulid::new();
        let snap = PendingSnapshot {
            tool: "/usr/bin/cat".into(),
            argv: vec!["/etc/shadow".into()],
            policy_diff: "deadbeef".into(),
            expires_at: std::time::Instant::now() + Duration::from_secs(60),
        };
        emit_approve_audit(snap, Some(4321), Some(1000), Some(request_id), tmp.path());
        let body = std::fs::read_to_string(tmp.path().join("audit.jsonl")).expect("audit jsonl");
        let line = body.lines().next().expect("one line");
        let v: serde_json::Value = serde_json::from_str(line).expect("parse line");
        assert_eq!(
            v["request_id"].as_str(),
            Some(request_id.to_string().as_str()),
            "approve audit JSONL must include the approving call's request_id; line={line}"
        );
    }
}
