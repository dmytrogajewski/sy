//! `sy aiplane` subcommands. Thin surface over the workload registry
//! and ipc layer; the heavy lifting lives in `daemon.rs` (future) and
//! the workload impls.
//!
//! As of the scaffold commit, only `status`, `list`, and `run` are
//! wired. `install-service` and `bench` land with the daemon migration.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;
use sy_core::Priority;

use super::ipc;
use super::registry::{cache_root, WorkloadInput, WorkloadKind, WorkloadOutput};
use super::session::SessionPool;
use super::workloads;

#[derive(Debug, Subcommand)]
pub enum AiplaneCmd {
    /// Show daemon status: registered workloads, hardware backend,
    /// recent NPU activity. Reads `$XDG_STATE_HOME/sy/aiplane/status.json`
    /// (or `…/sy/knowledge/status.json` during the migration window).
    Status {
        #[arg(long)]
        json: bool,
    },

    /// List every workload kind the daemon would register on this
    /// host, with the on-disk cache directory and whether the
    /// prepared ONNX is present.
    List {
        #[arg(long)]
        json: bool,
    },

    /// One-shot dispatch. Sends `Req::Run { workload, input }` over
    /// IPC if the daemon is up; falls back to in-process invocation
    /// otherwise.
    Run {
        /// Workload kind: `embed | rerank | vad | stt | tts | ocr |
        /// clip | denoise | eye-track`.
        #[arg(long, value_name = "KIND")]
        workload: String,
        /// JSON `WorkloadInput` literal. Example:
        /// `'{"kind":"text","text":"hello"}'`. Mutually exclusive with
        /// `--in-file`; one of the two is required.
        #[arg(long, value_name = "JSON")]
        input: Option<String>,
        /// Read the JSON `WorkloadInput` from a file instead of `--input`.
        /// Required for large inputs (e.g. `audio` PCM) that exceed the
        /// shell argument-length limit.
        #[arg(long, value_name = "PATH")]
        in_file: Option<std::path::PathBuf>,
        /// QoS class for the scheduler (SPEC §4.7). Case-sensitive
        /// PascalCase: `Realtime | Interactive | Background | Batch`.
        /// CLI defaults to `Interactive` (foreground user). Override
        /// via `--priority Background` or `SY_PRIORITY=Background`.
        #[arg(
            long,
            value_name = "CLASS",
            env = "SY_PRIORITY",
            default_value = "Interactive"
        )]
        priority: Priority,
        /// Soft deadline (e.g. `200ms`, `5s`, `1m`). Surfaces as
        /// `CallOpts.deadline_ms` to the daemon. Omitted = no deadline.
        #[arg(
            long,
            value_name = "DURATION",
            env = "SY_DEADLINE",
            value_parser = parse_deadline_ms,
        )]
        deadline: Option<u64>,
        /// Caller-supplied trace id, propagated end-to-end so logs
        /// across `sy` + the daemon + the workers share a key.
        #[arg(long, value_name = "ID", env = "SY_TRACE_ID")]
        trace_id: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Cooperatively abort an inflight `aiplane.run`. Resolves the
    /// target workload from the daemon's inflight registry (no
    /// `--workload` needed). Returns immediately once the daemon
    /// ACKs the cancel; the running request returns
    /// `ErrorCode::Cancelled` to its original caller.
    Cancel {
        /// Ulid printed by `aiplane.run`'s response (or carried in
        /// the v1 envelope's `request_id` field).
        request_id: String,
        #[arg(long)]
        json: bool,
    },

    /// Worker child entrypoint. Spawned by the daemon supervisor —
    /// not for direct human use. Hosts one `Workload` on its own
    /// /dev/accel/accel0 HW context and exposes `WorkerReq` on the
    /// passed Unix socket.
    #[command(hide = true)]
    Worker {
        /// Workload kind this worker hosts.
        #[arg(long, value_name = "KIND")]
        kind: String,
        /// Unix socket path to bind. Supervisor passes the
        /// deterministic per-kind path (`sy-aiplane-worker-<K>.sock`).
        #[arg(long, value_name = "PATH")]
        socket: std::path::PathBuf,
    },
}

pub fn dispatch(cmd: AiplaneCmd) -> Result<()> {
    match cmd {
        AiplaneCmd::Status { json } => status(json),
        AiplaneCmd::List { json } => list(json),
        AiplaneCmd::Run {
            workload,
            input,
            in_file,
            priority,
            deadline,
            trace_id,
            json,
        } => {
            let input_json = match (input, in_file) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("pass only one of --input or --in-file")
                }
                (Some(s), None) => s,
                (None, Some(p)) => std::fs::read_to_string(&p)
                    .with_context(|| format!("read --in-file {}", p.display()))?,
                (None, None) => anyhow::bail!("one of --input or --in-file is required"),
            };
            run(&workload, &input_json, priority, deadline, trace_id, json)
        }
        AiplaneCmd::Cancel { request_id, json } => cancel(&request_id, json),
        AiplaneCmd::Worker { kind, socket } => {
            let parsed: WorkloadKind = kind.parse()?;
            super::worker::run(parsed, socket)
        }
    }
}

fn cancel(request_id: &str, json_out: bool) -> Result<()> {
    let target: ulid::Ulid = request_id
        .parse()
        .with_context(|| format!("parse request_id {request_id:?}"))?;
    let path = ipc::socket_path();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio rt")?;
    let resp = rt.block_on(async move {
        let mut client = sy_ipc::Client::connect(&path)
            .await
            .with_context(|| format!("connect {}", path.display()))?;
        client
            .call(
                "aiplane.cancel",
                json!({ "target_request_id": target }),
                sy_ipc::CallOpts::default(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("aiplane.cancel: {e}"))
    })?;
    match resp {
        sy_ipc::Response::Ok { result, .. } => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let already = result
                    .get("cancelled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                println!("cancel: ACK (token tripped: {already})");
                if let Some(err) = result.get("worker_cancel_error").and_then(|v| v.as_str()) {
                    tracing::warn!(
                        target: "sy::aiplane::cli",
                        error = %err,
                        "worker cancel surface error"
                    );
                }
            }
            Ok(())
        }
        sy_ipc::Response::Err { error, .. } => {
            anyhow::bail!("daemon: {}: {}", error.code, error.message)
        }
    }
}

/// Clap value-parser for `--deadline`. Accepts the humantime-ish
/// vocabulary used in SPEC §4.2 examples — `200ms`, `5s`, `1m` — and
/// returns milliseconds so it slots directly into
/// [`sy_ipc::CallOpts::deadline_ms`]. Rejects bare numbers (ambiguous
/// between seconds and milliseconds) with a helpful message so a
/// typo at the CLI edge surfaces immediately rather than silently
/// truncating.
pub(crate) fn parse_deadline_ms(raw: &str) -> std::result::Result<u64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num_part, unit) = if let Some(rest) = s.strip_suffix("ms") {
        (rest, "ms")
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, "s")
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, "m")
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, "h")
    } else {
        return Err(format!(
            "deadline {raw:?} needs a unit (`ms`, `s`, `m`, `h`); bare numbers are rejected to avoid second/millisecond confusion"
        ));
    };
    let n: u64 = num_part
        .trim()
        .parse()
        .map_err(|e| format!("deadline {raw:?}: bad number {num_part:?}: {e}"))?;
    let dur = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => unreachable!("unit parsing covered above"),
    };
    Ok(dur.as_millis() as u64)
}

fn status(json_out: bool) -> Result<()> {
    let s = match super::status::load() {
        Ok(s) => s,
        Err(_) => {
            // Pre-aiplane snapshot path. Fall through gracefully so
            // `sy aiplane status` works even while the daemon still
            // writes under sy/knowledge/.
            if json_out {
                println!(r#"{{"daemon_running":false,"reason":"no status snapshot"}}"#);
            } else {
                println!("daemon: down (no status snapshot)");
            }
            return Ok(());
        }
    };
    let fresh = super::status::is_fresh(&s);
    if json_out {
        let v = json!({
            "daemon_running": s.daemon_running && fresh,
            "fresh": fresh,
            "embed_backend": s.embed_backend,
            "embed_hardware": s.embed_hardware,
            "workloads": s.workloads,
            "points": s.points,
            "indexing": s.indexing,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!(
        "daemon:    {}",
        if s.daemon_running && fresh {
            "up"
        } else {
            "down"
        }
    );
    if !s.embed_hardware.is_empty() {
        println!("hardware:  {} ({})", s.embed_hardware, s.embed_backend);
    }
    println!("points:    {}", s.points);
    if !s.workloads.is_empty() {
        println!("workloads:");
        let mut names: Vec<_> = s.workloads.keys().collect();
        names.sort();
        for n in names {
            let h = &s.workloads[n];
            println!(
                "  {n}: loaded={} backend={} calls={} ema={:.1}ms",
                h.loaded, h.backend, h.calls, h.ema_ms
            );
        }
    }
    Ok(())
}

fn list(json_out: bool) -> Result<()> {
    let root = cache_root();
    let mut rows = Vec::new();
    for k in WorkloadKind::ALL {
        let stem = stem_for_kind(k);
        let dir = root.join(stem);
        let prepared = dir.is_dir()
            && dir
                .read_dir()
                .map(|mut r| r.next().is_some())
                .unwrap_or(false);
        rows.push((k, stem, dir, prepared));
    }
    if json_out {
        let arr: Vec<_> = rows
            .iter()
            .map(|(k, stem, dir, prepared)| {
                json!({
                    "kind": k.as_str(),
                    "model_stem": stem,
                    "cache_dir": dir.display().to_string(),
                    "prepared": prepared,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    println!(
        "{:<10}  {:<24}  {:<9}  cache_dir",
        "kind", "model_stem", "prepared"
    );
    for (k, stem, dir, prepared) in rows {
        println!(
            "{:<10}  {:<24}  {:<9}  {}",
            k.as_str(),
            stem,
            if prepared { "yes" } else { "no" },
            dir.display()
        );
    }
    Ok(())
}

fn stem_for_kind(k: WorkloadKind) -> &'static str {
    match k {
        WorkloadKind::Embed => "multilingual-e5-base",
        WorkloadKind::Rerank => "bge-reranker-v2-m3",
        WorkloadKind::Vad => "silero-vad",
        WorkloadKind::Stt => "novasr",
        WorkloadKind::Tts => "piper-tts",
        WorkloadKind::Ocr => "nemotron-ocr-v2",
        WorkloadKind::Clip => "clip-vit-large-patch14",
        WorkloadKind::Denoise => "deepfilternet3",
        WorkloadKind::EyeTrack => "mediapipe-iris",
    }
}

/// Failure modes for [`call_aiplane_run`]. `DaemonDown` triggers the
/// in-process fallback; `Wire` surfaces wire-level errors; `Remote`
/// carries a daemon-reported error message for display.
enum AiplaneCallError {
    DaemonDown,
    Wire(anyhow::Error),
    Remote(String),
}

/// Synchronous v1 `aiplane.run` call. Spins a short-lived tokio
/// runtime so the rest of `aiplane::cli` stays sync; the cost is
/// dwarfed by NPU dispatch + ORT compile-cache hit on the daemon
/// side.
fn call_aiplane_run(
    kind: WorkloadKind,
    input: WorkloadInput,
    priority: Priority,
    deadline_ms: Option<u64>,
    trace_id: Option<String>,
) -> std::result::Result<WorkloadOutput, AiplaneCallError> {
    let path = ipc::socket_path();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| AiplaneCallError::Wire(e.into()))?;
    rt.block_on(async move {
        let mut client = match sy_ipc::Client::connect(&path).await {
            Ok(c) => c,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::ConnectionRefused =>
            {
                return Err(AiplaneCallError::DaemonDown);
            }
            Err(e) => return Err(AiplaneCallError::Wire(e.into())),
        };
        let resp = client
            .call(
                "aiplane.run",
                json!({ "workload": kind, "input": input }),
                sy_ipc::CallOpts {
                    priority,
                    deadline_ms,
                    trace_id: trace_id.map(sy_ipc::TraceId),
                    ..sy_ipc::CallOpts::default()
                },
            )
            .await
            .map_err(|e| AiplaneCallError::Wire(e.into()))?;
        match resp {
            sy_ipc::Response::Ok { result, .. } => {
                let output: WorkloadOutput =
                    serde_json::from_value(result.get("output").cloned().unwrap_or_default())
                        .map_err(|e| AiplaneCallError::Wire(e.into()))?;
                Ok(output)
            }
            sy_ipc::Response::Err { error, .. } => Err(AiplaneCallError::Remote(format!(
                "{}: {}",
                error.code, error.message
            ))),
        }
    })
}

fn run(
    workload: &str,
    input: &str,
    priority: Priority,
    deadline_ms: Option<u64>,
    trace_id: Option<String>,
    json_out: bool,
) -> Result<()> {
    let kind: WorkloadKind = workload.parse()?;
    let input: WorkloadInput =
        serde_json::from_str(input).with_context(|| format!("parse input JSON: {input:?}"))?;
    // Try IPC first via the v1 `aiplane.run` method. Daemon-down
    // falls back to an in-process registry — useful for offline
    // debug and before the daemon is migrated. The daemon-side
    // bridge dispatches through the supervisor exactly as the
    // legacy `Req::Run` did.
    let output = match call_aiplane_run(kind, input.clone(), priority, deadline_ms, trace_id) {
        Ok(out) => out,
        Err(AiplaneCallError::DaemonDown) => {
            let pool = Arc::new(SessionPool::new());
            let registry = workloads::register_all(pool);
            registry.run(kind, input)?
        }
        Err(AiplaneCallError::Wire(e)) => return Err(e.context("ipc request")),
        Err(AiplaneCallError::Remote(msg)) => anyhow::bail!("daemon: {msg}"),
    };
    if json_out {
        let body = json!({
            "workload": kind.as_str(),
            "priority": priority.as_str(),
            "output": output,
        });
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        // Compact human format per output variant.
        match &output {
            super::registry::WorkloadOutput::Vector { vector } => {
                println!(
                    "vector[{}]: {:?}…",
                    vector.len(),
                    &vector[..vector.len().min(6)]
                );
            }
            super::registry::WorkloadOutput::Score { score } => println!("score: {score}"),
            super::registry::WorkloadOutput::Text { text } => println!("{text}"),
            super::registry::WorkloadOutput::Spans { spans } => {
                println!("spans: {} segments", spans.len());
                for s in spans {
                    println!("  {} - {} ms (p={:.2})", s.start_ms, s.end_ms, s.prob);
                }
            }
            super::registry::WorkloadOutput::Bytes { bytes } => {
                println!("bytes: {} B", bytes.len());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Test-only wrapper so we can drive `AiplaneCmd` (a Subcommand)
    /// through `clap::Parser::try_parse_from`. The real top-level
    /// `Cli` in `main.rs` is private; this minimal harness mirrors
    /// the production parser surface for the aiplane-cli arg tests.
    #[derive(Parser)]
    #[command(name = "sy-aiplane-test", no_binary_name = true)]
    struct TestCli {
        #[command(subcommand)]
        cmd: AiplaneCmd,
    }

    fn parse_run(args: &[&str]) -> Result<AiplaneCmd, clap::Error> {
        TestCli::try_parse_from(args).map(|c| c.cmd)
    }

    /// Serialise the env-var tests below: cargo runs the test bin
    /// with multiple threads, and `SY_PRIORITY` is per-process state.
    /// Without this lock, `default_is_interactive` can race against
    /// `env_var_overrides_default` and read `Background` from the
    /// half-finished setup.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn priority_default_is_interactive() {
        // SPEC §4.7 + SPEC §5 Friction Map row 3: CLI surfaces default
        // to `Interactive`. A bare `sy aiplane run --workload embed
        // --input <json>` invocation must not silently land on
        // `Background` (which would steal latency-sensitive class
        // budget from foreground callers).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("SY_PRIORITY").ok();
        std::env::remove_var("SY_PRIORITY");
        let cmd = parse_run(&[
            "run",
            "--workload",
            "embed",
            "--input",
            r#"{"kind":"text","text":"hi"}"#,
        ])
        .expect("parse default");
        if let Some(v) = prev {
            std::env::set_var("SY_PRIORITY", v);
        }
        match cmd {
            AiplaneCmd::Run { priority, .. } => assert_eq!(priority, Priority::Interactive),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn priority_env_var_overrides_default() {
        // CLIG precedence (CLAUDE.md): env var beats the default but
        // loses to an explicit flag. Verify the env half here.
        // `try_parse_from` reads env vars via clap's `env = ...` so
        // the test sets it just for this call.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("SY_PRIORITY").ok();
        std::env::set_var("SY_PRIORITY", "Background");
        let cmd = parse_run(&[
            "run",
            "--workload",
            "embed",
            "--input",
            r#"{"kind":"text","text":"hi"}"#,
        ])
        .expect("parse env");
        if let Some(v) = prev {
            std::env::set_var("SY_PRIORITY", v);
        } else {
            std::env::remove_var("SY_PRIORITY");
        }
        match cmd {
            AiplaneCmd::Run { priority, .. } => assert_eq!(priority, Priority::Background),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn priority_flag_overrides_env_var() {
        // Flag wins over env (CLIG precedence: flag > env > config >
        // default).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("SY_PRIORITY").ok();
        std::env::set_var("SY_PRIORITY", "Background");
        let cmd = parse_run(&[
            "run",
            "--workload",
            "embed",
            "--input",
            r#"{"kind":"text","text":"hi"}"#,
            "--priority",
            "Realtime",
        ])
        .expect("parse flag-over-env");
        if let Some(v) = prev {
            std::env::set_var("SY_PRIORITY", v);
        } else {
            std::env::remove_var("SY_PRIORITY");
        }
        match cmd {
            AiplaneCmd::Run { priority, .. } => assert_eq!(priority, Priority::Realtime),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn priority_unknown_value_errors_with_valid_list() {
        // SPEC §4.7 + AGENTS.md CLI design: a typo at the CLI edge
        // must surface immediately and the error must enumerate the
        // valid alternatives. clap returns ErrorKind::ValueValidation
        // (exit code 2 per CLIG convention) and our `Priority`
        // FromStr message lists all four classes.
        let err = parse_run(&[
            "run",
            "--workload",
            "embed",
            "--input",
            r#"{"kind":"text","text":"hi"}"#,
            "--priority",
            "interactive",
        ])
        .expect_err("lowercase priority must reject");
        let rendered = err.to_string();
        assert!(
            rendered.contains("Realtime") && rendered.contains("Interactive"),
            "error should advertise the valid set, got: {rendered}"
        );
    }

    #[test]
    fn deadline_parses_humantime_units_into_ms() {
        // `--deadline 5s` → 5000 ms; `--deadline 200ms` → 200 ms.
        // The mapping must land directly in `CallOpts.deadline_ms`
        // so the dispatcher can compare against `queued_at`.
        let cmd = parse_run(&[
            "run",
            "--workload",
            "embed",
            "--input",
            r#"{"kind":"text","text":"hi"}"#,
            "--deadline",
            "5s",
        ])
        .expect("parse deadline 5s");
        match cmd {
            AiplaneCmd::Run { deadline, .. } => assert_eq!(deadline, Some(5_000)),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn cancel_subcommand_parses_request_id() {
        // SPEC §5.4 / arch-aiplane-scheduler Step 7: `sy aiplane
        // cancel <ulid>` produces an `AiplaneCmd::Cancel` with the
        // ulid stashed in `request_id` so the dispatch path can fire
        // `aiplane.cancel` against the daemon.
        let id = ulid::Ulid::new().to_string();
        let cmd = parse_run(&["cancel", &id]).expect("parse cancel");
        match cmd {
            AiplaneCmd::Cancel { request_id, json } => {
                assert_eq!(request_id, id);
                assert!(!json);
            }
            _ => panic!("expected Cancel"),
        }
    }

    #[test]
    fn cancel_subcommand_supports_json_flag() {
        // CLIG §5.2: every output command takes `--json`. The cancel
        // subcommand uses it for the machine-readable response shape
        // so MCP and agent callers can read `cancelled` deterministically.
        let id = ulid::Ulid::new().to_string();
        let cmd = parse_run(&["cancel", &id, "--json"]).expect("parse cancel --json");
        match cmd {
            AiplaneCmd::Cancel { json, .. } => assert!(json),
            _ => panic!("expected Cancel"),
        }
    }

    #[test]
    fn deadline_bare_number_rejects_explicitly() {
        // SPEC §4.2 example uses `deadline_ms`; the CLI vocabulary
        // adds units. A bare integer is ambiguous (seconds vs ms)
        // and rejects rather than silently truncating.
        let err = parse_run(&[
            "run",
            "--workload",
            "embed",
            "--input",
            r#"{"kind":"text","text":"hi"}"#,
            "--deadline",
            "500",
        ])
        .expect_err("bare number must reject");
        let rendered = err.to_string();
        assert!(
            rendered.contains("unit"),
            "error should explain the missing unit: {rendered}"
        );
    }
}
