//! Unix-socket IPC for sy-aiplane: CLI/MCP ↔ daemon.
//!
//! Two flavours share the same socket:
//!
//!   * Fire-and-forget `Op` (existing): write a JSON line, close. The
//!     daemon owns the work; the client never sees a response. Used
//!     for IndexNow, FullResync, Pause, etc.
//!   * Request-response `Req`/`Resp`: write a JSON line, then read
//!     one back. Used for Embed / Search / generic `Run` so the CLI
//!     and MCP server can offload all NPU inference to the daemon —
//!     the only process with the device bound — instead of spinning
//!     up their own ORT session.
//!
//! Missing socket = daemon not running; `send()` is a silent no-op,
//! `request()` returns `IpcError::DaemonDown` so the caller can fall
//! back to in-process embedding.

use std::{
    env,
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::registry::{WorkloadInput, WorkloadKind, WorkloadOutput};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// Re-read sy.toml [knowledge] sources + schedule.
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
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "kebab-case")]
pub enum Resp {
    Run { output: WorkloadOutput },
    Search { hits: Vec<HitRow> },
    Error { msg: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HitRow {
    /// Final score callers should rank by. Cosine similarity for
    /// `Req::Search`; rerank sigmoid for `Req::SearchRerank`.
    pub score: f32,
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

/// Fire-and-forget op. Silently succeeds if the daemon isn't listening.
pub fn send(op: &Op) -> Result<()> {
    let p = socket_path();
    let mut stream = match UnixStream::connect(&p) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let line = serde_json::to_string(op)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}

/// Synchronous request-response. Connects, writes one JSON line,
/// reads one back. Errors disambiguate "no daemon" (so callers can
/// fall back) from "wire failure" (which they should surface).
///
/// 30 s timeout: first request after the daemon has idled out of D3
/// takes a beat to wake the NPU + a few hundred ms compile cache
/// hit; under load + a long input (STT of a multi-second audio)
/// the upper bound is real.
pub fn request(req: &Req) -> std::result::Result<Resp, IpcError> {
    let p = socket_path();
    let stream = UnixStream::connect(&p).map_err(|_| IpcError::DaemonDown)?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));

    let mut writer = stream.try_clone().map_err(|e| IpcError::Wire(e.into()))?;
    let line = serde_json::to_string(req).map_err(|e| IpcError::Wire(e.into()))?;
    writer
        .write_all(line.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .map_err(|e| IpcError::Wire(e.into()))?;
    // Half-close the write side so the daemon's BufReader sees EOF on
    // the write end specifically; the read end stays open for the
    // response.
    let _ = writer.shutdown(std::net::Shutdown::Write);

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader
        .read_line(&mut buf)
        .map_err(|e| IpcError::Wire(e.into()))?;
    if buf.trim().is_empty() {
        return Err(IpcError::Wire(anyhow::anyhow!(
            "daemon closed connection without responding"
        )));
    }
    serde_json::from_str::<Resp>(buf.trim()).map_err(|e| IpcError::Wire(e.into()))
}

/// Bind the listener and dispatch incoming JSON lines:
///
///   * `Op` (fire-and-forget) → forwarded on `ops_tx`. Connection
///     can carry multiple ops and is read until EOF.
///   * `Req` (request-response) → forwarded on `req_tx` as
///     `(Req, UnixStream)`. The receiver owns the stream, writes
///     the `Resp` JSON line on it, and drops to close.
pub fn serve(ops_tx: mpsc::Sender<Op>, req_tx: mpsc::Sender<(Req, UnixStream)>) -> Result<()> {
    let p = socket_path();
    if p.exists() {
        let _ = std::fs::remove_file(&p);
    }
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&p).with_context(|| format!("bind {}", p.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let ops_tx = ops_tx.clone();
            let req_tx = req_tx.clone();
            thread::spawn(move || handle_conn(stream, ops_tx, req_tx));
        }
    });
    Ok(())
}

fn handle_conn(
    stream: UnixStream,
    ops_tx: mpsc::Sender<Op>,
    req_tx: mpsc::Sender<(Req, UnixStream)>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let Ok(reader_fd) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_fd);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(op) = serde_json::from_str::<Op>(trimmed) {
            let _ = ops_tx.send(op);
            continue;
        }
        if let Ok(req) = serde_json::from_str::<Req>(trimmed) {
            // Hand off the *original* stream so the worker can
            // write the response on the still-open write half.
            let _ = req_tx.send((req, stream));
            return;
        }
        // Malformed line → swallow and keep reading.
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
    fn malformed_request_returns_serde_err_not_panic() {
        let r: Result<Req, _> = serde_json::from_str("not json");
        assert!(r.is_err());
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir_when_set() {
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
        use std::io::Write as _;
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
        let (req_tx, req_rx) = mpsc::channel::<(Req, UnixStream)>();
        serve(ops_tx, req_tx).expect("serve");

        let registry: Arc<Registry> = {
            let pool = Arc::new(SessionPool::new());
            let mut r = Registry::new(pool);
            r.register(Arc::new(FakeWorkload::embed()));
            Arc::new(r)
        };
        let registry_for_worker = registry.clone();
        thread::spawn(move || {
            while let Ok((req, mut stream)) = req_rx.recv() {
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
                };
                let _ = writeln!(stream, "{}", serde_json::to_string(&resp).unwrap());
            }
        });

        // Drive the client side.
        let resp = request(&Req::Run {
            workload: WorkloadKind::Embed,
            input: WorkloadInput::Text {
                text: "hello daemon".into(),
            },
        })
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
        let r1 = request(&Req::Run {
            workload: WorkloadKind::Embed,
            input: WorkloadInput::Text { text: "x".into() },
        })
        .unwrap();
        let r2 = request(&Req::Run {
            workload: WorkloadKind::Embed,
            input: WorkloadInput::Text { text: "x".into() },
        })
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
        use std::io::Write as _;
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
        let (req_tx, req_rx) = mpsc::channel::<(Req, UnixStream)>();
        serve(ops_tx, req_tx).expect("serve");

        let registry: Arc<Registry> = {
            let pool = Arc::new(SessionPool::new());
            let mut r = Registry::new(pool);
            r.register(Arc::new(FakeWorkload::new(WorkloadKind::Rerank)));
            Arc::new(r)
        };
        let reg_w = registry.clone();
        thread::spawn(move || {
            while let Ok((req, mut stream)) = req_rx.recv() {
                let resp = match req {
                    Req::SearchRerank {
                        query,
                        limit,
                        candidates,
                        prefix: _,
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
                                file_path: path,
                                chunk_index: i as u32,
                                chunk_text: doc,
                                embed_score: Some(cos),
                            })
                            .collect();
                        Resp::Search { hits }
                    }
                    other => Resp::Error {
                        msg: format!("unexpected variant: {other:?}"),
                    },
                };
                let _ = writeln!(stream, "{}", serde_json::to_string(&resp).unwrap());
            }
        });

        let resp = request(&Req::SearchRerank {
            query: "what gifts does Anna Lu like".into(),
            limit: 3,
            prefix: None,
            candidates: 10,
        })
        .expect("request");

        match resp {
            Resp::Search { hits } => {
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
}
