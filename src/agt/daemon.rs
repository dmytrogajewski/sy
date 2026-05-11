//! sy-agentd: long-lived daemon owning all ACP child processes and serving
//! a Unix-socket protocol used by `sy agt …` clients.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, Mutex},
    time::sleep,
};

use crate::agt::{
    acp::{AcpChild, AcpInbound},
    permission::{ask, Decision},
    protocol::{ClientReply, ClientReq, DaemonEvent, SessionInfo, SessionStatus, TranscriptEntry},
    registry,
    session::{entry_from_update, state_dir, Completion, Session},
    socket_path,
};

type SharedSession = Arc<Mutex<Session>>;
type Sessions = Arc<Mutex<HashMap<String, SharedSession>>>;

pub fn run_blocking() -> Result<()> {
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

    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("bind {}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&sock)?.permissions();
        p.set_mode(0o600);
        let _ = std::fs::set_permissions(&sock, p);
    }
    eprintln!("sy-agentd: listening on {}", sock.display());

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    rehydrate_persisted(&sessions).await;

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

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("sy-agentd: accept error: {e}");
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let sessions = sessions.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, sessions).await {
                eprintln!("sy-agentd: client error: {e}");
            }
        });
    }
}

async fn handle_client(stream: UnixStream, sessions: Sessions) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        let req: ClientReq = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_line(
                    &mut wr,
                    &ClientReply::Error {
                        message: format!("bad request: {e}"),
                        code: 3,
                    },
                )
                .await;
                continue;
            }
        };
        match req {
            ClientReq::Run { agent, cwd, prompt } => {
                match start_session(&sessions, &agent, &cwd, &prompt).await {
                    Ok(id) => write_line(&mut wr, &ClientReply::RunReply { session_id: id }).await?,
                    Err(e) => {
                        write_line(
                            &mut wr,
                            &ClientReply::Error {
                                message: e.to_string(),
                                code: 4,
                            },
                        )
                        .await?
                    }
                }
                signal_waybar();
            }
            ClientReq::List => {
                let map = sessions.lock().await;
                let mut infos = Vec::with_capacity(map.len());
                for s in map.values() {
                    infos.push(s.lock().await.info.clone());
                }
                drop(map);
                infos.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                write_line(&mut wr, &ClientReply::ListReply { sessions: infos }).await?;
            }
            ClientReq::Prompt { session_id, text } => {
                match send_prompt(&sessions, &session_id, &text).await {
                    Ok(()) => write_line(&mut wr, &ClientReply::Ack).await?,
                    Err(e) => {
                        write_line(
                            &mut wr,
                            &ClientReply::Error {
                                message: e.to_string(),
                                code: 4,
                            },
                        )
                        .await?
                    }
                }
            }
            ClientReq::Stop { session_id } => {
                match stop_session(&sessions, &session_id).await {
                    Ok(()) => write_line(&mut wr, &ClientReply::Ack).await?,
                    Err(e) => {
                        write_line(
                            &mut wr,
                            &ClientReply::Error {
                                message: e.to_string(),
                                code: 4,
                            },
                        )
                        .await?
                    }
                }
                signal_waybar();
            }
            ClientReq::Tail {
                session_id,
                follow,
                replay,
            } => {
                stream_tail(&sessions, &session_id, follow, replay, &mut wr).await?;
                return Ok(()); // tail consumes the connection
            }
            ClientReq::PermissionDecision { .. } => {
                // v1: not wired (notify-send is the canonical decision channel).
                write_line(&mut wr, &ClientReply::Ack).await?;
            }
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
                write_line(&mut wr, &ClientReply::DiagReply { agents: entries }).await?;
            }
            ClientReq::Shutdown => {
                write_line(&mut wr, &ClientReply::Ack).await?;
                std::process::exit(0);
            }
        }
    }
    Ok(())
}

async fn write_line(wr: &mut tokio::net::unix::OwnedWriteHalf, reply: &ClientReply) -> Result<()> {
    let mut buf = serde_json::to_vec(reply)?;
    buf.push(b'\n');
    wr.write_all(&buf).await?;
    wr.flush().await?;
    Ok(())
}

async fn start_session(
    sessions: &Sessions,
    agent: &str,
    cwd: &PathBuf,
    prompt: &str,
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
        cwd.clone(),
        summary,
        child,
        acp_session_id.clone(),
    )?;
    session.append(TranscriptEntry::UserText { text: prompt.to_string() });
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
    tokio::spawn(async move {
        run_inbound_loop(task_session, task_acp_id, inbound).await;
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
        let _ = prompt_session.lock().await.set_status(SessionStatus::Working);
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

async fn run_inbound_loop(session: SharedSession, acp_session_id: String, mut inbound: mpsc::Receiver<AcpInbound>) {
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
            AcpInbound::Request { id, method, params } if method == "session/request_permission" => {
                handle_permission(session.clone(), id, params).await;
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

async fn handle_permission(session: SharedSession, req_id: Value, params: Value) {
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
    {
        let mut s = session.lock().await;
        let _ = s.set_status(SessionStatus::Awaiting);
        let session_id = s.info.id.clone();
        s.broadcast(DaemonEvent::Permission {
            session_id,
            request_id: req_uuid.clone(),
            summary: summary.clone(),
            body: body.clone(),
        });
    }
    signal_waybar();

    let decision = ask(&summary, &body, Duration::from_secs(8)).await;

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
        Decision::Deny => pick(&["reject", "deny", "cancel"]).unwrap_or_else(|| "reject_once".into()),
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
                if matches!(decision, Decision::Allow) { "allowed" } else { "denied" },
                summary
            ),
        });
    }
    signal_waybar();
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
        s.append(TranscriptEntry::UserText { text: text.to_string() });
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

async fn stream_tail(
    sessions: &Sessions,
    session_id: &str,
    follow: bool,
    replay: bool,
    wr: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    let shared = match sessions.lock().await.get(session_id).cloned() {
        Some(s) => s,
        None => {
            return write_line(
                wr,
                &ClientReply::Error {
                    message: format!("no such session: {session_id}"),
                    code: 2,
                },
            )
            .await
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
        write_line(wr, &ClientReply::Event { event: e }).await?;
    }
    if let Some(ref mut rx) = rx {
        while let Some(e) = rx.recv().await {
            if write_line(wr, &ClientReply::Event { event: e }).await.is_err() {
                break;
            }
        }
    }
    Ok(())
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
