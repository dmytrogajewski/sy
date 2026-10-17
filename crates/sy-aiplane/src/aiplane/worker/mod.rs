//! NPU worker subprocess: hosts exactly one `Workload` and answers
//! `WorkerReq` from the supervisor.
//!
//! Spawned by the daemon's supervisor (`aiplane::supervisor`). One
//! worker process per NPU workload that should be live: a fresh
//! XDNA HW context per process side-steps the single-context-per-
//! process limit and gives us failure isolation across workloads.
//!
//! Lifecycle:
//!
//! ```text
//! argv  ──►  build_workload(kind)
//!            │
//!            ├─► bind worker_ipc socket
//!            ├─► spawn background load thread
//!            │     state: NotPrepared → Loading → Ready{backend} | Failed
//!            └─► serve loop:
//!                 Health     → reply from shared state
//!                 RunBatch   → require Ready, then dispatch
//!                 Shutdown   → reply ack, flip shutdown flag, exit 0
//! ```
//!
//! See `src/aiplane/worker_ipc.rs` for the wire format and
//! `src/aiplane/supervisor/` for the parent-side child management.

pub mod runner;

pub use runner::run;
