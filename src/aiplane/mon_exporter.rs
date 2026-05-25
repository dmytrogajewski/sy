//! sy-mon Step 10 — bring up the aiplane plane's Prometheus UDS
//! exposition surface. Step 20 generalised the runtime-thread wrapper
//! into [`crate::mon_exporter`] so every plane that needs one shares
//! the same plumbing; this module is a thin shim that pins the
//! plane name to `"aiplane"` and re-exports the type for source
//! compatibility with the Step 10 daemon wiring.
//!
//! The aiplane plane physically lives inside `knowledge::daemon::run()`
//! (the supervisor is brought up by `init_aiplane_supervisor()` there).
//! Its sibling metrics-exposition surface therefore needs to bind from
//! the same place. The shared installer in `sy_core::obs::mon_exporter`
//! requires an active tokio runtime at call time; the knowledge daemon's
//! main is a synchronous mpsc-channel loop, so the generic
//! [`PlaneMonExporter`] wrapper owns a dedicated runtime thread that
//! holds the install guard for the daemon's lifetime.
//!
//! ## Path
//!
//! `$XDG_RUNTIME_DIR/sy/aiplane/metrics.sock` (SPEC §3 SCOPE item 1).

#![cfg(feature = "mon-exporter")]

use anyhow::Result;

pub use crate::mon_exporter::PlaneMonExporter;

/// Source-compatible alias preserved from Step 10. Step 20 collapsed
/// the per-plane wrappers into one generic [`PlaneMonExporter`]; the
/// daemon-wiring callers (`crate::knowledge::daemon`) continue to use
/// the historical name.
pub type AiplaneMonExporter = PlaneMonExporter;

/// Bring up the aiplane plane's Prometheus UDS exporter at
/// `$XDG_RUNTIME_DIR/sy/aiplane/metrics.sock`. Delegates to the
/// generic [`crate::mon_exporter::spawn`] with the plane name pinned
/// to `"aiplane"`.
pub fn spawn() -> Result<AiplaneMonExporter> {
    crate::mon_exporter::spawn("aiplane")
}
