//! sy-mon Step 20: when the supervision layer reports a plane is
//! `Failed`, the `sy_supervisor_plane_state{plane,state}` gauge must
//! reflect the change within 1 s — that is the load-bearing
//! invariant for the supervisor panel in `sy mon` (SPEC §4
//! "SystemSnapshot.supervisor.planes[*]").
//!
//! The test drives `crate::supervision::emit_plane_state` directly
//! with a fake plane record, then snapshots the recorder via
//! `metrics_util::debugging::DebuggingRecorder`. The 1-second budget
//! is a real wall-clock deadline so a future regression that
//! introduced a tick-rate dependency (e.g. accidentally driving the
//! emission off the 30 s aiplane health-poll thread) would surface
//! here instead of in production.

#![cfg(feature = "mon-exporter")]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use metrics::Key;
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use metrics_util::CompositeKey;

/// Budget for the gauge transition from `ready` to `failed`. The
/// emission path is synchronous so this is a generous ceiling — but
/// the SPEC pins the visible delay at 1 s, so we test the spec.
const PROPAGATION_BUDGET: Duration = Duration::from_secs(1);

/// One-shot install of the debugging recorder. `metrics::
/// set_global_recorder` is process-global and first-writer-wins; we
/// must install before any prom-exporter `install` lands on the same
/// binary. This integration test gets its own test binary so the
/// recorder slot is fresh.
fn install_debug_recorder() -> Snapshotter {
    let recorder = DebuggingRecorder::new();
    let snap = recorder.snapshotter();
    // First-writer-wins: ignore the `Err` if a prior test in this
    // binary already installed (the test binary is its own process,
    // so this is defensive against future test-add ordering).
    let _ = recorder.install();
    snap
}

/// Find the `sy_supervisor_plane_state` sample matching `plane` +
/// `state` in the recorder snapshot, returning its current value.
/// Returns `None` when the (plane, state) pair hasn't been emitted
/// at all (which is itself a failure for this test: every emission
/// pass writes one row per known state).
fn current_gauge_value(
    snap: &HashMap<
        CompositeKey,
        (
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            DebugValue,
        ),
    >,
    plane: &str,
    state: &str,
) -> Option<f64> {
    for (ck, (_unit, _desc, value)) in snap {
        let key: &Key = ck.key();
        if key.name() != "sy_supervisor_plane_state" {
            continue;
        }
        let labels: HashMap<&str, &str> = key.labels().map(|l| (l.key(), l.value())).collect();
        if labels.get("plane") == Some(&plane) && labels.get("state") == Some(&state) {
            return match value {
                DebugValue::Gauge(v) => Some(v.into_inner()),
                _ => None,
            };
        }
    }
    None
}

/// Roadmap Step 20 contract: `supervision::emit_plane_state` is the
/// canonical emission site for `sy_supervisor_plane_state{plane,state}`.
/// A `ready → failed` transition must be visible on the
/// `state="failed"` series within [`PROPAGATION_BUDGET`] and the
/// `state="ready"` series must fall back to 0 in the same window.
#[test]
fn fake_plane_failed_transition_propagates_within_one_second() {
    const PLANE: &str = "agt";
    let snap = install_debug_recorder();

    // Initial state: plane is healthy. The aggregator's tick maps
    // scrape-success to `ready`.
    sy_core::sensors::supervisor::emit_plane_state(&[(PLANE, "ready")]);
    let frame = snap.snapshot().into_hashmap();
    let initial_ready = current_gauge_value(&frame, PLANE, "ready");
    let initial_failed = current_gauge_value(&frame, PLANE, "failed");
    assert_eq!(
        initial_ready,
        Some(1.0),
        "ready indicator must be 1.0 after first emit; frame={frame:?}"
    );
    assert_eq!(
        initial_failed,
        Some(0.0),
        "failed indicator must be 0.0 while plane is ready; frame={frame:?}"
    );

    // Flip the fake plane to `Failed`. The transition is synchronous —
    // every emission pass overwrites every known-state row for the
    // plane, so the failed→ready series flips on the *next* emission
    // tick. Loop with a small budget so a future regression that
    // introduced an async hop would still surface here.
    let start = Instant::now();
    sy_core::sensors::supervisor::emit_plane_state(&[(PLANE, "failed")]);
    let mut final_frame = snap.snapshot().into_hashmap();
    while start.elapsed() < PROPAGATION_BUDGET {
        let ready = current_gauge_value(&final_frame, PLANE, "ready");
        let failed = current_gauge_value(&final_frame, PLANE, "failed");
        if ready == Some(0.0) && failed == Some(1.0) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
        final_frame = snap.snapshot().into_hashmap();
    }
    let final_ready = current_gauge_value(&final_frame, PLANE, "ready");
    let final_failed = current_gauge_value(&final_frame, PLANE, "failed");
    panic!(
        "supervisor plane-state gauge did not flip to failed within {} ms; \
         got ready={final_ready:?} failed={final_failed:?}",
        PROPAGATION_BUDGET.as_millis()
    );
}
