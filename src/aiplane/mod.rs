//! `aiplane` — multi-workload NPU plane.
//!
//! Generalises what used to be the embedding-only knowledge daemon into
//! a substrate that can host any number of NPU-eligible workloads
//! (embedding, reranking, VAD, STT, TTS, OCR, CLIP, denoise, eye-track).
//! One daemon process owns `/dev/accel/accel0`; everyone else
//! (`sy knowledge search`, the MCP server, ad-hoc CLI invocations)
//! sends work over a Unix socket via `ipc::request(Req::Run { … })`.
//!
//! ## Module surface
//!
//! - `registry` — `WorkloadKind` enum + `Workload` trait + `Registry`
//!   dispatch.
//! - `session` — shared NPU mutex + `RunCtx` (cancellation/throughput).
//! - `reexec` — the AMD venv re-exec dance (called from `main()` before
//!   any thread spawn).
//! - `status` — `Status` JSON snapshot at
//!   `$XDG_STATE_HOME/sy/aiplane/status.json` + waybar refresh signal.
//! - `ipc` — Unix-socket protocol: fire-and-forget `Op` + request-
//!   response `Req`/`Resp`.
//! - `workloads` — per-workload `Workload` impls + `register_all`.
//!
//! ## Status during the migration
//!
//! As of the aiplane-scaffold commit, the daemon (`sy knowledge
//! daemon`, `sy-knowledge.service`) still lives under
//! `src/knowledge/daemon.rs` and uses the in-tree `knowledge::ipc` /
//! `knowledge::embed`. This `aiplane::` module compiles in parallel
//! and is exercised by unit tests. A follow-up commit lifts the
//! daemon and renames the systemd unit to `sy-aiplane.service`.

pub mod cli;
pub mod ipc;
pub mod reexec;
pub mod registry;
pub mod session;
pub mod status;
pub mod supervisor;
pub mod worker;
pub mod worker_ipc;
pub mod workloads;

/// Shared process-wide mutex for tests that mutate `XDG_RUNTIME_DIR`.
/// All daemon-in-thread / worker-in-thread tests acquire this so they
/// don't cross-route requests when cargo runs them in parallel.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
