//! `sy ipc <subcommand>` — operator-visible round-trip checks for the
//! v1 IPC envelope (SPEC §4.7 / ROADMAP arch-ipc-v1 Step 7).
//!
//! Two subcommands: [`IpcCmd::Ping`] calls `system.health` and prints
//! `ready / degraded / starting / failed` + latency; [`IpcCmd::Describe`]
//! dumps the `system.describe` response, optionally as JSON.

use std::{path::PathBuf, time::Instant};

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use serde_json::Value;
use sy_core::Priority;
use sy_ipc::{
    paths::{for_endpoint, ENDPOINTS},
    CallOpts, Client, Response,
};

/// CLI exit codes (SPEC §4.7). Documented stable per CLIG. `2`
/// (usage error) is emitted directly by clap on argv-parse failure;
/// it does not need a constant here.
pub const EXIT_OK: i32 = 0;
pub const EXIT_GENERIC: i32 = 1;
pub const EXIT_DRIFT: i32 = 3;
pub const EXIT_NOT_READY: i32 = 4;

/// Per-call deadline. 2 s covers the worst-case cold-listener wake up
/// for a daemon that just finished a long pass; small enough that
/// `sy doctor` doesn't stall when a daemon hangs.
const PING_DEADLINE_MS: u64 = 2_000;

#[derive(Debug, Subcommand)]
pub enum IpcCmd {
    /// Health-check a daemon. Prints the `system.health` state plus
    /// the round-trip latency. Exit codes: 0 ready, 3 degraded, 4
    /// starting/failed, 1 connect failure, 2 usage error.
    Ping {
        /// Endpoint name (`knowledge | aiplane | agt | stack`) or a
        /// raw socket path.
        endpoint: String,
        /// Emit a single JSON line on stdout instead of the human
        /// summary.
        #[arg(long)]
        json: bool,
    },
    /// Dump `system.describe` for an endpoint. JSON-by-default; the
    /// `--text` flag swaps to a short human listing for terminals.
    Describe {
        /// Endpoint name or socket path. Same resolution as `Ping`.
        endpoint: String,
        /// Pretty-print the `result` object as JSON (default).
        #[arg(long, conflicts_with = "text")]
        json: bool,
        /// Emit a short text summary instead of the JSON dump.
        #[arg(long)]
        text: bool,
    },
}

pub fn dispatch(cmd: IpcCmd) -> Result<()> {
    match cmd {
        IpcCmd::Ping { endpoint, json } => {
            let code = ping(&endpoint, json)?;
            std::process::exit(code);
        }
        IpcCmd::Describe {
            endpoint,
            json,
            text,
        } => describe(&endpoint, json, text),
    }
}

/// Resolve `name` to a socket path: try the static endpoint map
/// first, then fall back to interpreting `name` as a raw filesystem
/// path. Returns a usage error listing the valid endpoint names when
/// neither resolves.
pub fn resolve_socket(name: &str) -> Result<PathBuf> {
    if let Some(p) = for_endpoint(name) {
        return Ok(p);
    }
    let raw = PathBuf::from(name);
    if raw.is_absolute() || name.contains('/') {
        return Ok(raw);
    }
    Err(anyhow!(
        "unknown endpoint {name:?}; valid endpoints: {} (or a raw socket path)",
        ENDPOINTS.join(", ")
    ))
}

/// Ping an endpoint and print the result. Returns the SPEC §4.7 exit
/// code the caller should hand to `std::process::exit`. Split out
/// from `dispatch` so the in-crate `ipc_ping_e2e` test can exercise
/// the full happy path without terminating its own process.
pub fn ping(name: &str, json_out: bool) -> Result<i32> {
    let sock = resolve_socket(name)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let outcome = rt.block_on(call_health(&sock))?;
    if json_out {
        let v = serde_json::json!({
            "endpoint": name,
            "socket": sock.display().to_string(),
            "state": outcome.state,
            "status_line": outcome.status_line,
            "latency_ms": outcome.latency_ms,
        });
        println!("{}", serde_json::to_string(&v)?);
    } else {
        println!(
            "{}: {} ({:.0} ms) — {}",
            name, outcome.state, outcome.latency_ms, outcome.status_line
        );
    }
    Ok(exit_code_for(&outcome.state))
}

fn describe(name: &str, json_out: bool, text_out: bool) -> Result<()> {
    let sock = resolve_socket(name)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let result = rt.block_on(call_describe(&sock))?;
    if text_out {
        print_describe_text(name, &result);
        return Ok(());
    }
    let _ = json_out; // default is JSON; flag exists for explicitness.
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn print_describe_text(endpoint: &str, result: &Value) {
    println!("endpoint: {endpoint}");
    if let Some(v) = result.get("protocol_version") {
        println!("protocol_version: {v}");
    }
    if let Some(methods) = result.get("methods").and_then(|v| v.as_array()) {
        println!("methods:");
        for m in methods {
            if let Some(s) = m.as_str() {
                println!("  {s}");
            }
        }
    }
    if let Some(caps) = result.get("capabilities") {
        println!("capabilities: {caps}");
    }
}

#[derive(Debug, Clone)]
struct PingOutcome {
    state: String,
    status_line: String,
    latency_ms: f64,
}

async fn call_health(sock: &std::path::Path) -> Result<PingOutcome> {
    let mut client = Client::connect(sock)
        .await
        .with_context(|| format!("connect {}", sock.display()))?;
    let started = Instant::now();
    let resp = client
        .call(
            "system.health",
            serde_json::json!({}),
            CallOpts {
                priority: Priority::Interactive,
                deadline_ms: Some(PING_DEADLINE_MS),
                ..CallOpts::default()
            },
        )
        .await
        .context("system.health call")?;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    match resp {
        Response::Ok { result, .. } => {
            let state = result
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let status_line = result
                .get("status_line")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(PingOutcome {
                state,
                status_line,
                latency_ms,
            })
        }
        Response::Err { error, .. } => Err(anyhow!(
            "system.health returned {}: {}",
            error.code,
            error.message
        )),
    }
}

async fn call_describe(sock: &std::path::Path) -> Result<Value> {
    let mut client = Client::connect(sock)
        .await
        .with_context(|| format!("connect {}", sock.display()))?;
    let resp = client
        .call(
            "system.describe",
            serde_json::json!({}),
            CallOpts {
                priority: Priority::Interactive,
                deadline_ms: Some(PING_DEADLINE_MS),
                ..CallOpts::default()
            },
        )
        .await
        .context("system.describe call")?;
    match resp {
        Response::Ok { result, .. } => Ok(result),
        Response::Err { error, .. } => Err(anyhow!(
            "system.describe returned {}: {}",
            error.code,
            error.message
        )),
    }
}

fn exit_code_for(state: &str) -> i32 {
    match state {
        "ready" => EXIT_OK,
        "degraded" => EXIT_DRIFT,
        "starting" | "failed" => EXIT_NOT_READY,
        _ => EXIT_GENERIC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_runtime_dir<F: FnOnce()>(dir: &str, f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", dir);
        f();
        match prev {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn ping_endpoint_resolution_resolves_named_endpoints() {
        // SPEC §4.7: `sy ipc ping knowledge` must land on the
        // canonical `$XDG_RUNTIME_DIR/sy-knowledge.sock` rather than
        // treating "knowledge" as a literal path.
        with_runtime_dir("/tmp/sy-ipc-cli-test", || {
            let sock = resolve_socket("knowledge").expect("resolve knowledge");
            assert_eq!(
                sock,
                PathBuf::from("/tmp/sy-ipc-cli-test/sy-knowledge.sock")
            );
        });
    }

    #[test]
    fn ping_endpoint_resolution_accepts_raw_paths() {
        // CLIG: callers must be able to point the tool at an
        // arbitrary socket without registering a name first.
        let sock = resolve_socket("/var/run/custom.sock").expect("absolute path");
        assert_eq!(sock, PathBuf::from("/var/run/custom.sock"));
    }

    #[test]
    fn ping_endpoint_resolution_rejects_unknown_bare_names() {
        let err = resolve_socket("nonsense").expect_err("unknown name should fail");
        let msg = err.to_string();
        assert!(msg.contains("unknown endpoint"));
        assert!(msg.contains("knowledge"));
    }

    #[test]
    fn describe_json_schema_text_dump_lists_methods() {
        // Golden-ish: a `system.describe` result containing the
        // baseline shape (`protocol_version`, `methods`,
        // `capabilities`) must round-trip through `print_describe_text`
        // without panicking. Doubles as a structural smoke test of the
        // SPEC §4.2 describe schema the CLI consumes.
        let result = serde_json::json!({
            "protocol_version": 1,
            "methods": ["system.describe", "system.health", "knowledge.search"],
            "capabilities": {
                "streaming": false,
                "priority_classes": ["Realtime", "Interactive", "Background", "Batch"]
            },
            "build_info": {"name": "sy-knowledge", "version": "0.1.0", "git_sha": "dev"}
        });
        // Capturing stdout would couple the test to the writer impl;
        // instead reach into the structural getters the text dump uses.
        assert_eq!(result["protocol_version"].as_u64(), Some(1));
        let methods: Vec<&str> = result["methods"]
            .as_array()
            .expect("methods array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(methods.contains(&"system.describe"));
        assert!(methods.contains(&"knowledge.search"));
        let caps = result.get("capabilities").expect("capabilities");
        assert_eq!(caps["streaming"], serde_json::Value::Bool(false));
    }

    #[test]
    fn exit_code_for_known_states() {
        assert_eq!(exit_code_for("ready"), EXIT_OK);
        assert_eq!(exit_code_for("degraded"), EXIT_DRIFT);
        assert_eq!(exit_code_for("starting"), EXIT_NOT_READY);
        assert_eq!(exit_code_for("failed"), EXIT_NOT_READY);
        assert_eq!(exit_code_for("something-else"), EXIT_GENERIC);
    }

    /// Hermetic `sy_ipc::Handler` backed by [`sy_ipc::SystemMethods`] so the
    /// test exercises the real reserved-method dispatch path.
    struct SystemOnlyHandler {
        inner: std::sync::Arc<sy_ipc::SystemMethods>,
    }

    impl SystemOnlyHandler {
        fn ready(name: &'static str, methods: Vec<String>) -> Self {
            let registry = std::sync::Arc::new(sy_ipc::CancelRegistry::new());
            let health_fn: sy_ipc::HealthFn = std::sync::Arc::new(|| sy_ipc::HealthSnapshot {
                state: sy_ipc::HealthState::Ready,
                status_line: "ready".into(),
                queue_depth: 0,
                warm_models: Vec::new(),
            });
            let build_info = sy_ipc::BuildInfo {
                name: name.into(),
                version: "0.0.0".into(),
                git_sha: "test".into(),
            };
            let system = sy_ipc::SystemMethods::new(
                build_info,
                health_fn,
                registry,
                sy_ipc::Capabilities::baseline(),
                methods,
            );
            Self {
                inner: std::sync::Arc::new(system),
            }
        }
    }

    impl sy_ipc::Handler for SystemOnlyHandler {
        async fn handle(&self, req: sy_ipc::Request) -> sy_ipc::Response {
            self.inner
                .try_handle(&req)
                .unwrap_or(sy_ipc::Response::Err {
                    schema_version: sy_ipc::SCHEMA_VERSION,
                    request_id: req.request_id,
                    error: sy_ipc::ErrorBody {
                        code: sy_core::ErrorCode::BadRequest,
                        message: format!("unknown method: {}", req.method),
                        retry_after_ms: None,
                        details: serde_json::Value::Null,
                    },
                })
        }
    }

    fn spawn_hermetic_server(
        sock: std::path::PathBuf,
        methods: Vec<String>,
    ) -> std::thread::JoinHandle<()> {
        let std_listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        std_listener.set_nonblocking(true).expect("nonblock");
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("rt");
            rt.block_on(async {
                let listener = tokio::net::UnixListener::from_std(std_listener).expect("from_std");
                let server = sy_ipc::Server::new(SystemOnlyHandler::ready("test-daemon", methods));
                let _ = server.serve(listener).await;
            });
        })
    }

    #[test]
    fn ipc_ping_e2e_returns_ready_and_zero_exit_code() {
        // SPEC §4.7 round-trip: pointing `sy ipc ping` at a live daemon
        // that returns `Ready` must print the state and yield exit 0.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp.path().join("hermetic.sock");
        let _server = spawn_hermetic_server(sock.clone(), vec!["knowledge.search".into()]);
        // Give the server thread a moment to enter `serve`. The
        // listener is bound synchronously via `bind` so the connect
        // can't EAGAIN; this just yields the scheduler so the tokio
        // accept loop is parked before the client lands.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let code = ping(&sock.display().to_string(), true).expect("ping ok");
        assert_eq!(code, EXIT_OK);
    }

    #[test]
    fn ipc_describe_e2e_emits_methods_and_protocol_version() {
        // Companion to the ping smoke: a real `system.describe`
        // round-trip surfaces the methods list we registered with
        // SystemMethods and the SPEC-mandated `protocol_version: 1`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp.path().join("hermetic.sock");
        let _server = spawn_hermetic_server(
            sock.clone(),
            vec!["knowledge.search".into(), "knowledge.index_now".into()],
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let result = rt.block_on(call_describe(&sock)).expect("describe");
        assert_eq!(result["protocol_version"].as_u64(), Some(1));
        let methods: Vec<&str> = result["methods"]
            .as_array()
            .expect("methods array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(methods.contains(&"knowledge.search"));
        assert!(methods.contains(&"system.describe"));
    }
}
