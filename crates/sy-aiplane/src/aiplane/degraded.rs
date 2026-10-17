//! Degraded-worker gate: which NPU workloads the daemon could not bring
//! up, and why.
//!
//! Before BUG-20260927-0119 an unloadable worker was fatal: the knowledge
//! daemon refused to start, so `status.json` froze, the bar tile collapsed
//! to zero width, the IPC socket never bound, and 112k already-indexed
//! points became unqueryable — a missing `bf16.onnx` took down *every*
//! consumer of the shared socket, including the ones that need no
//! inference at all.
//!
//! The gate is the replacement for that `exit(1)`: the daemon keeps
//! serving, records each worker it failed to raise together with the
//! loader's own message (which already names the missing path and the
//! exact `prep_npu_workload.py` command), and surfaces it as
//! `status.last_error` so the applet turns red instead of vanishing. A
//! recovery thread re-`ensure`s the recorded kinds, and `clear`s them on
//! success, so rebuilding the artifact heals the plane without a human
//! touching `systemctl reset-failed`.
//!
//! `BTreeMap` keyed by `WorkloadKind::as_str()` keeps [`DegradedWorkers::summary`]
//! byte-stable, which matters because the summary lands in `status.json`
//! and is asserted on by tests.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use sy_core::workload::WorkloadKind;

/// Recorded worker-availability failures. Empty means "fully healthy".
#[derive(Debug, Default)]
pub struct DegradedWorkers {
    failures: Mutex<BTreeMap<&'static str, String>>,
}

impl DegradedWorkers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note that `kind` is unavailable, replacing any earlier reason.
    pub fn record(&self, kind: WorkloadKind, reason: impl Into<String>) {
        self.failures
            .lock()
            .expect("degraded gate poisoned")
            .insert(kind.as_str(), reason.into());
    }

    /// Forget `kind` after a successful reload. Returns whether it was
    /// degraded, so callers can log a recovery line exactly once.
    pub fn clear(&self, kind: WorkloadKind) -> bool {
        self.failures
            .lock()
            .expect("degraded gate poisoned")
            .remove(kind.as_str())
            .is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.failures
            .lock()
            .expect("degraded gate poisoned")
            .is_empty()
    }

    /// Degraded kinds, sorted ascending.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.failures
            .lock()
            .expect("degraded gate poisoned")
            .keys()
            .copied()
            .collect()
    }

    /// One-line reason for `status.last_error`, or `None` when healthy.
    /// Kind names lead so a reader (and the waybar tooltip) learns *what*
    /// is missing before reading *why*.
    pub fn summary(&self) -> Option<String> {
        let failures = self.failures.lock().expect("degraded gate poisoned");
        if failures.is_empty() {
            return None;
        }
        let parts: Vec<String> = failures
            .iter()
            .map(|(kind, reason)| format!("{kind} worker unavailable: {reason}"))
            .collect();
        Some(parts.join("; "))
    }
}

/// Process-wide gate. The status builders that need it are free functions
/// called from a dozen lifecycle sites (heartbeat, pass boundaries,
/// shutdown), all of which describe one daemon process; threading an
/// `Arc` through every signature to model a genuine singleton would be
/// noise. Tests use [`DegradedWorkers::new`] directly.
pub fn current() -> &'static DegradedWorkers {
    static GATE: OnceLock<DegradedWorkers> = OnceLock::new();
    GATE.get_or_init(DegradedWorkers::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_gate_has_no_error_to_surface() {
        let gate = DegradedWorkers::new();
        assert!(gate.is_empty());
        assert!(gate.kinds().is_empty());
        assert_eq!(gate.summary(), None);
    }

    #[test]
    fn record_then_clear_tracks_availability() {
        let gate = DegradedWorkers::new();
        gate.record(WorkloadKind::Embed, "embed model not found");
        assert!(!gate.is_empty());
        assert_eq!(gate.kinds(), vec!["embed"]);
        assert!(gate.clear(WorkloadKind::Embed));
        assert!(gate.is_empty());
        // Clearing a kind that was never degraded is a no-op.
        assert!(!gate.clear(WorkloadKind::Embed));
    }

    #[test]
    fn summary_names_kind_before_the_loader_reason() {
        let gate = DegradedWorkers::new();
        gate.record(
            WorkloadKind::Embed,
            "embed model not found at /home/x/.cache/sy/aiplane/multilingual-e5-base/multilingual-e5-base.bf16.onnx",
        );
        let s = gate.summary().expect("degraded");
        assert!(s.starts_with("embed worker unavailable: "), "{s}");
        assert!(s.contains("prep") || s.contains(".onnx"), "keeps hint: {s}");
    }

    #[test]
    fn summary_is_sorted_and_joined_across_kinds() {
        // Rerank fails first, embed second: the summary must still list
        // embed first so status.json is byte-stable across restarts.
        let gate = DegradedWorkers::new();
        gate.record(WorkloadKind::Rerank, "rerank model not found");
        gate.record(WorkloadKind::Embed, "embed model not found");
        assert_eq!(gate.kinds(), vec!["embed", "rerank"]);
        let s = gate.summary().expect("degraded");
        assert_eq!(
            s,
            "embed worker unavailable: embed model not found; \
             rerank worker unavailable: rerank model not found"
        );
    }

    #[test]
    fn re_recording_replaces_the_reason() {
        let gate = DegradedWorkers::new();
        gate.record(WorkloadKind::Embed, "first");
        gate.record(WorkloadKind::Embed, "second");
        assert_eq!(
            gate.summary().expect("degraded"),
            "embed worker unavailable: second"
        );
    }
}
