//! Unix-socket IPC between `sy stack <cli>` and the `sy stack bar` daemon.
//!
//! Wire format: IPC v1 (SPEC §4.2) — length-delimited JSON envelopes
//! via `sy-ipc`. Client→daemon ops are fire-and-forget; the daemon
//! still writes an empty `Response::Ok` ack but the CLI never reads
//! it (CLI commands must work without the bar running — missing
//! socket = silent no-op).

use std::{
    env,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sy_core::{ErrorCode, Priority};
use sy_ipc::{
    blocking::{build_request, write_request},
    BuildInfo, Capabilities, ErrorBody, Handler, HealthFn, HealthSnapshot, HealthState, Request,
    Response, Server, SystemMethods, SCHEMA_VERSION,
};

/// Wire method namespaces for the stack-bar daemon. Matches SPEC §4.2
/// "Method naming" — `<daemon>.<verb>` Crockford-friendly identifiers.
pub const METHOD_REFRESH: &str = "stack.refresh";
pub const METHOD_TOGGLE: &str = "stack.toggle";
pub const METHOD_RELOAD_THEME: &str = "stack.reload_theme";

/// Fire-and-forget op the bar consumes off its main-loop mpsc. The
/// source-level enum is retained because the bar's render loop reads
/// it post-decoding; on the wire this travels as a method string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    /// Reload items.json from disk and repaint.
    Refresh,
    /// Hide if visible, show if hidden.
    Toggle,
    /// Re-read theme (sy.toml + `themes/<name>.toml`) and repaint.
    ReloadTheme,
}

impl Op {
    fn as_method(&self) -> &'static str {
        match self {
            Op::Refresh => METHOD_REFRESH,
            Op::Toggle => METHOD_TOGGLE,
            Op::ReloadTheme => METHOD_RELOAD_THEME,
        }
    }

    fn from_method(method: &str) -> Option<Self> {
        match method {
            METHOD_REFRESH => Some(Op::Refresh),
            METHOD_TOGGLE => Some(Op::Toggle),
            METHOD_RELOAD_THEME => Some(Op::ReloadTheme),
            _ => None,
        }
    }
}

pub fn socket_path() -> PathBuf {
    if let Ok(d) = env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            let dir = PathBuf::from(d).join("sy");
            let _ = std::fs::create_dir_all(&dir);
            return dir.join("stackbar.sock");
        }
    }
    let uid = unsafe { libc_getuid() };
    let dir = PathBuf::from(format!("/run/user/{uid}/sy"));
    let _ = std::fs::create_dir_all(&dir);
    dir.join("stackbar.sock")
}

extern "C" {
    fn getuid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    getuid()
}

/// UI-repaint deadline for stack-bar ops. 500 ms matches the SPEC §4.2
/// CallOpts default for "interactive UI tick"; the bar prefers stale
/// state over a stalled repaint.
const STACK_DEADLINE_MS: u64 = 500;

/// Send a v1 envelope to the bar daemon. Silently succeeds if the
/// daemon is not running (CLI commands must work standalone).
pub fn send(op: &Op) -> Result<()> {
    let p = socket_path();
    let mut stream = match UnixStream::connect(&p) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(STACK_DEADLINE_MS)));
    let req = build_request(
        op.as_method(),
        serde_json::json!({}),
        Priority::Interactive,
        Some(STACK_DEADLINE_MS),
        None,
        None,
        None,
    );
    // Fire-and-forget per SPEC §4.7: write the frame, don't wait for
    // the ack. The daemon enqueues the op onto its mpsc before sending
    // the ack so the bar repaint races correctly even if we close
    // here.
    write_request(&mut stream, &req)?;
    Ok(())
}

/// Listen for ops on the socket. Drops them into the supplied std
/// mpsc; the bar's main loop polls the receiver each tick. Spawns a
/// tokio runtime in a dedicated thread to host the `sy_ipc::Server`
/// while the bar itself stays sync (iced has its own event loop).
pub fn serve(tx: mpsc::Sender<Op>) -> Result<()> {
    let p = socket_path();
    if p.exists() {
        let _ = std::fs::remove_file(&p);
    }
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let std_listener = std::os::unix::net::UnixListener::bind(&p)
        .with_context(|| format!("bind {}", p.display()))?;
    std_listener
        .set_nonblocking(true)
        .context("set_nonblocking on stack listener")?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));

    let bridge = StackBridge::new(tx);
    thread::Builder::new()
        .name("sy-stackbar-ipc-v1".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("stackbar tokio runtime");
            rt.block_on(async move {
                let listener = match tokio::net::UnixListener::from_std(std_listener) {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("sy stack bar: convert listener: {e}");
                        return;
                    }
                };
                let server = Server::new(bridge);
                if let Err(e) = server.serve(listener).await {
                    eprintln!("sy stack bar: ipc serve: {e}");
                }
            });
        })
        .context("spawn stack-bar v1 listener thread")?;
    Ok(())
}

struct StackBridge {
    tx: Mutex<mpsc::Sender<Op>>,
    system: SystemMethods,
    healthy: Arc<AtomicBool>,
}

impl StackBridge {
    fn new(tx: mpsc::Sender<Op>) -> Self {
        let cancel_registry = Arc::new(sy_ipc::CancelRegistry::new());
        let healthy = Arc::new(AtomicBool::new(true));
        let healthy_for_fn = Arc::clone(&healthy);
        let health_fn: HealthFn = Arc::new(move || HealthSnapshot {
            state: if healthy_for_fn.load(Ordering::SeqCst) {
                HealthState::Ready
            } else {
                HealthState::Degraded
            },
            status_line: "stack-bar ready".into(),
            queue_depth: 0,
            warm_models: Vec::new(),
        });
        let build_info = BuildInfo {
            name: "sy-stack-bar".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            git_sha: option_env!("SY_GIT_SHA").unwrap_or("dev").into(),
        };
        let methods = vec![
            METHOD_REFRESH.into(),
            METHOD_TOGGLE.into(),
            METHOD_RELOAD_THEME.into(),
        ];
        let system = SystemMethods::new(
            build_info,
            health_fn,
            cancel_registry,
            Capabilities::baseline(),
            methods,
        );
        Self {
            tx: Mutex::new(tx),
            system,
            healthy,
        }
    }
}

impl Handler for StackBridge {
    async fn handle(&self, req: Request) -> Response {
        if let Some(resp) = self.system.try_handle(&req) {
            return resp;
        }
        let Some(op) = Op::from_method(&req.method) else {
            return err(
                req.request_id,
                ErrorCode::BadRequest,
                format!("unknown method: {}", req.method),
            );
        };
        // Hold the std::sync::MutexGuard only inside this synchronous
        // scope so the surrounding async fn stays `Send`.
        let send_result = {
            match self.tx.lock() {
                Ok(g) => g.send(op),
                Err(_) => {
                    self.healthy.store(false, Ordering::SeqCst);
                    return err(
                        req.request_id,
                        ErrorCode::Internal,
                        "stack bridge sender lock poisoned".into(),
                    );
                }
            }
        };
        if send_result.is_err() {
            self.healthy.store(false, Ordering::SeqCst);
            return err(
                req.request_id,
                ErrorCode::Internal,
                "stack bar event loop is gone".into(),
            );
        }
        ok(req.request_id)
    }
}

fn ok(request_id: ulid::Ulid) -> Response {
    Response::Ok {
        schema_version: SCHEMA_VERSION,
        request_id,
        result: serde_json::json!({}),
        blob: None,
    }
}

fn err(request_id: ulid::Ulid, code: ErrorCode, message: String) -> Response {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use sy_ipc::{CallOpts, Client};

    fn temp_socket() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("stackbar.sock");
        (dir, sock)
    }

    async fn spawn_test_server(
        sock: std::path::PathBuf,
        tx: mpsc::Sender<Op>,
    ) -> tokio::task::JoinHandle<()> {
        // Hermetic variant of `serve` that binds at a caller-chosen
        // socket path (production `serve` always uses
        // `$XDG_RUNTIME_DIR/sy/stackbar.sock`).
        let std_listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        std_listener.set_nonblocking(true).expect("nonblock");
        let listener = tokio::net::UnixListener::from_std(std_listener).expect("from_std");
        let bridge = StackBridge::new(tx);
        let server = Server::new(bridge);
        tokio::spawn(async move {
            let _ = server.serve(listener).await;
        })
    }

    #[tokio::test]
    async fn stack_ipc_v1_toggle() {
        // Step 6 DoD: a v1 `stack.toggle` envelope from the client
        // surfaces as `Op::Toggle` on the bar's mpsc within 500 ms.
        let (_tmp, sock) = temp_socket();
        let (op_tx, op_rx) = mpsc::channel();
        let server = spawn_test_server(sock.clone(), op_tx).await;

        let mut client = Client::connect(&sock).await.expect("client connect");
        let resp = client
            .call(
                METHOD_TOGGLE,
                serde_json::json!({}),
                CallOpts {
                    priority: Priority::Interactive,
                    deadline_ms: Some(STACK_DEADLINE_MS),
                    ..CallOpts::default()
                },
            )
            .await
            .expect("call stack.toggle");
        assert!(
            matches!(resp, Response::Ok { .. }),
            "ack must be Ok, got {resp:?}"
        );

        let op = op_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("op delivered to mpsc");
        assert!(matches!(op, Op::Toggle), "got {op:?}");

        server.abort();
    }

    #[tokio::test]
    async fn stack_ipc_unknown_method_returns_bad_request() {
        let (_tmp, sock) = temp_socket();
        let (op_tx, _op_rx) = mpsc::channel();
        let server = spawn_test_server(sock.clone(), op_tx).await;

        let mut client = Client::connect(&sock).await.expect("client connect");
        let resp = client
            .call("stack.nonsense", serde_json::json!({}), CallOpts::default())
            .await
            .expect("call");
        match resp {
            Response::Err { error, .. } => {
                assert_eq!(error.code, ErrorCode::BadRequest);
            }
            other => panic!("expected Err, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn all_daemons_describe() {
        // ROADMAP arch-ipc-v1 Step 6 DoD: every daemon answers
        // `system.describe` with `schema_version: 1` and a non-empty
        // methods array. Stack is non-streaming (unary fire-and-
        // forget), so this asserts the baseline shape; agt has its
        // own streaming-capability test in `src/agt/daemon.rs` and
        // the aiplane+knowledge pair is covered by
        // `aiplane_ipc_v1_describe_capabilities` in
        // `src/aiplane/ipc.rs`. Together those three cover all four
        // daemons.
        let (_tmp, sock) = temp_socket();
        let (op_tx, _op_rx) = mpsc::channel();
        let server = spawn_test_server(sock.clone(), op_tx).await;

        let mut client = Client::connect(&sock).await.expect("client connect");
        let resp = client
            .call(
                "system.describe",
                serde_json::json!({}),
                CallOpts::default(),
            )
            .await
            .expect("describe");
        match resp {
            Response::Ok {
                schema_version,
                result,
                ..
            } => {
                assert_eq!(schema_version, SCHEMA_VERSION);
                let methods: Vec<&str> = result["methods"]
                    .as_array()
                    .expect("methods array")
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect();
                assert!(methods.contains(&METHOD_REFRESH));
                assert!(methods.contains(&METHOD_TOGGLE));
                assert!(methods.contains(&METHOD_RELOAD_THEME));
                assert!(methods.contains(&"system.describe"));
                // Stack daemon is fire-and-forget — streaming must be
                // off (only agt flips it on).
                assert_eq!(
                    result["capabilities"]["streaming"],
                    serde_json::Value::Bool(false)
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        server.abort();
    }
}
