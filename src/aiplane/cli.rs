//! `sy aiplane` subcommands. Thin surface over the workload registry
//! + ipc layer; the heavy lifting lives in `daemon.rs` (future) and the
//! workload impls.
//!
//! As of the scaffold commit, only `status`, `list`, and `run` are
//! wired. `install-service` and `bench` land with the daemon migration.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;

use super::ipc::{self, IpcError, Req, Resp};
use super::registry::{cache_root, WorkloadInput, WorkloadKind};
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
        /// `'{"kind":"text","text":"hello"}'`.
        #[arg(long, value_name = "JSON")]
        input: String,
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
            json,
        } => run(&workload, &input, json),
        AiplaneCmd::Worker { kind, socket } => {
            let parsed: WorkloadKind = kind.parse()?;
            super::worker::run(parsed, socket)
        }
    }
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

fn run(workload: &str, input: &str, json_out: bool) -> Result<()> {
    let kind: WorkloadKind = workload.parse()?;
    let input: WorkloadInput =
        serde_json::from_str(input).with_context(|| format!("parse input JSON: {input:?}"))?;
    // Try IPC first.
    let output = match ipc::request(&Req::Run {
        workload: kind,
        input: input.clone(),
    }) {
        Ok(Resp::Run { output }) => output,
        Ok(Resp::Error { msg }) => anyhow::bail!("daemon: {msg}"),
        Ok(other) => anyhow::bail!("daemon: unexpected response {other:?}"),
        Err(IpcError::DaemonDown) => {
            // Daemon down → run in-process. Useful for offline debug
            // and as a smoke path before the daemon is migrated.
            let pool = Arc::new(SessionPool::new());
            let registry = workloads::register_all(pool);
            registry.run(kind, input)?
        }
        Err(IpcError::Wire(e)) => return Err(e.context("ipc request")),
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&output)?);
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
