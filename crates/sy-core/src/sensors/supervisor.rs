//! Read-only adapter over the `crate::supervision` module's
//! per-plane status records. Per the sy-mon SPEC §4 `SystemSnapshot`
//! JSON example, the `supervisor` panel surfaces one row per plane:
//!
//! ```json
//! "supervisor": {"planes": [
//!   {"name": "aiplane", "state": "active", "restarts": 0},
//!   ...
//! ]}
//! ```
//!
//! The state strings match the `ServiceStatus` enum's serialised form
//! in `src/supervision/status.rs` (`ready`, `stopped`, `starting`,
//! `failed`, `not_installed`) plus systemd's `ActiveState` values
//! (`active`, `inactive`, ...) for SPEC parity. We deliberately keep
//! `state` as `String` rather than re-importing the binary crate's
//! enum: sy-core sits below the binary in the dep graph and cannot
//! pull `ServiceStatus` in. The caller in the binary (Step 11) maps
//! its enum to a stable token before handing the projection to this
//! adapter.
//!
//! Like `sensors::power`, nothing here shells out, calls systemctl,
//! or reads `/run/systemd/`. The read path that produces the
//! per-plane status lives in the binary's supervision module; this
//! adapter is a pure projection.

use serde::{Deserialize, Serialize};

/// One supervised plane's state snapshot. Mirrors the SPEC §4
/// `SystemSnapshot.supervisor.planes[*]` shape exactly.
///
/// `state` is intentionally a free-form token (the snake_case
/// `ServiceStatus` form from `src/supervision/status.rs`, or systemd's
/// `ActiveState`) so this struct does not couple sy-core to the
/// binary's enum hierarchy. Step 6's `SystemSnapshot` golden test
/// pins the set of expected tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaneState {
    pub name: String,
    pub state: String,
    pub restarts: u32,
}

/// One sensor tick of supervisor state. Wraps the per-plane Vec so
/// Step 6's `SystemSnapshot` can embed `{ "planes": [...] }` directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorSample {
    pub planes: Vec<PlaneState>,
}

impl SupervisorSample {
    /// Project a slice of pre-built plane records into a sample.
    /// Pure copy — the caller decides ordering (typically the order
    /// units appear in `sy.target.wants/`). Empty input yields an
    /// empty `planes` Vec rather than `None`.
    pub fn from_records(records: &[PlaneState]) -> Self {
        Self {
            planes: records.to_vec(),
        }
    }
}

/// Known plane-state tokens. Mirrors the `ServiceStatus` snake_case
/// serde set in `src/supervision/status.rs` plus systemd's
/// `ActiveState` values. Emitting `0.0` for every non-matching state
/// keeps `sy_supervisor_plane_state` clean across restart spikes — a
/// `failed → ready` transition pulls the `failed` series back to 0
/// in the same tick the `ready` series goes to 1.
const KNOWN_STATES: &[&str] = &[
    "ready",
    "stopped",
    "starting",
    "failed",
    "not_installed",
    "active",
    "inactive",
    "activating",
];

/// sy-mon Step 20: emit one tick's worth of
/// `sy_supervisor_plane_state{plane, state}` gauges. Each `(plane,
/// state)` pair becomes one labelled time series; the gauge is `1.0`
/// for the *current* state of each plane and `0.0` for every other
/// known state — the "indicator" pattern Prometheus dashboards
/// expect (`max by (plane) (sy_supervisor_plane_state)` yields the
/// current state, and `state{state="failed"}` alerts fire as soon
/// as any plane flips).
///
/// `records` is a slice of `(plane_name, state_token)` pairs — the
/// caller (typically `sy mon collect`'s tick) derives the tokens
/// from [`crate::sensors::supervisor::PlaneState::state`] (which is
/// already the snake_case `ServiceStatus` form). The gauge is
/// `String`-keyed for stability so the SystemSnapshot golden test
/// pins the same tokens.
///
/// This function is the single emission site so the
/// `supervisor_emits_plane_state` integration test (sy-mon Step 20)
/// can drive it directly with a fake plane record and observe the
/// gauge via `metrics_util::debugging::DebuggingRecorder`.
pub fn emit_plane_state(records: &[(&str, &str)]) {
    use metrics::gauge;
    for (plane, current) in records {
        for state in KNOWN_STATES {
            let value = if state == current { 1.0 } else { 0.0 };
            gauge!(
                "sy_supervisor_plane_state",
                "plane" => plane.to_string(),
                "state" => state.to_string(),
            )
            .set(value);
        }
        // The caller's `current` token may be one we don't have in
        // KNOWN_STATES (a future systemd ActiveState we haven't
        // pinned). Always emit a row for it at 1.0 so the dashboard
        // still shows the live state.
        if !KNOWN_STATES.contains(current) {
            gauge!(
                "sy_supervisor_plane_state",
                "plane" => plane.to_string(),
                "state" => current.to_string(),
            )
            .set(1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roadmap Step 4 DoD test: the adapter lists every supplied
    /// plane in order with its name, state token, and restart count
    /// intact. Stand-in for the binary's real
    /// `status_record(name, unit)` call site — we hand-build the
    /// records and assert the projection is byte-identical.
    #[test]
    fn lists_planes_with_restarts() {
        const AIPLANE_RESTARTS: u32 = 0;
        const KNOWLEDGE_RESTARTS: u32 = 2;
        let records = vec![
            PlaneState {
                name: "aiplane".to_string(),
                state: "active".to_string(),
                restarts: AIPLANE_RESTARTS,
            },
            PlaneState {
                name: "knowledge".to_string(),
                state: "failed".to_string(),
                restarts: KNOWLEDGE_RESTARTS,
            },
        ];
        let sample = SupervisorSample::from_records(&records);
        assert_eq!(sample.planes.len(), 2);
        assert_eq!(sample.planes[0].name, "aiplane");
        assert_eq!(sample.planes[0].state, "active");
        assert_eq!(sample.planes[0].restarts, AIPLANE_RESTARTS);
        assert_eq!(sample.planes[1].name, "knowledge");
        assert_eq!(sample.planes[1].state, "failed");
        assert_eq!(sample.planes[1].restarts, KNOWLEDGE_RESTARTS);
    }
}
