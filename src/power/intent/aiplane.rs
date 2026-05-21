//! Aiplane registry tap — zero-IPC, in-process intent channel that
//! reads `aiplane::Registry::current_queue_depth()` and emits
//! `IntentEvent::NpuQueue` whenever the depth changes.
//!
//! SPEC §2 lists "NPU queue depth" as one of the 12 panel signals
//! the forecaster + bandit use to disambiguate "user kicked off a
//! batch transcription" from "user is just typing". Because the
//! `Registry` lives in the same process as the power daemon (sy
//! aiplane runs the registry; sy power daemon will be folded in once
//! Step 10 lands), this channel is a pair of `Arc` clones — no
//! socket, no thread.
//!
//! Construction takes the two pieces the registry exposes via
//! `in_flight_counter()` + `last_workload_slot()`. The first `poll()`
//! after construction always emits (so the daemon has a non-stale
//! baseline); subsequent polls only emit when `depth` changed,
//! matching how `LogindChannel` dedupes a sustained `CallActive`.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use super::{IntentChannel, IntentEvent};

/// Polling tap of the aiplane registry. Holds only `Arc`s — no
/// reference into the `Registry` itself, no lifetime entanglement.
pub struct AiplaneIntentChannel {
    in_flight: Arc<AtomicUsize>,
    last_workload: Arc<Mutex<Option<String>>>,
    /// `None` before the first `poll()` (so the first call always
    /// emits); `Some(prev)` afterwards for change-detection.
    last_depth: Option<usize>,
}

impl AiplaneIntentChannel {
    /// Borrow the registry's atomic counter + last-workload slot.
    /// Returned by `Registry::in_flight_counter()` /
    /// `Registry::last_workload_slot()` — both `Arc` clones, cheap.
    pub fn new(in_flight: Arc<AtomicUsize>, last_workload: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            in_flight,
            last_workload,
            last_depth: None,
        }
    }
}

impl IntentChannel for AiplaneIntentChannel {
    fn poll(&mut self) -> Option<IntentEvent> {
        let depth = self.in_flight.load(Ordering::SeqCst);
        // Dedup: only emit when the depth changed since last poll.
        // First-ever poll has `last_depth == None` so it always emits.
        if self.last_depth == Some(depth) {
            return None;
        }
        self.last_depth = Some(depth);
        let head_workload = self.last_workload.lock().ok().and_then(|g| g.clone());
        Some(IntentEvent::NpuQueue {
            depth,
            head_workload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty registry (no workload has ever dispatched) yields
    /// `NpuQueue { depth: 0, head_workload: None }` on the first poll
    /// and then deduplicates on subsequent polls — matches the
    /// "always emit the first sample, then change-detect" contract.
    #[test]
    fn queue_depth_zero_when_empty() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(None));
        let mut ch = AiplaneIntentChannel::new(Arc::clone(&in_flight), Arc::clone(&last));
        let first = ch.poll();
        assert_eq!(
            first,
            Some(IntentEvent::NpuQueue {
                depth: 0,
                head_workload: None,
            })
        );
        // Depth unchanged → dedupe.
        assert_eq!(ch.poll(), None);
    }

    /// Bumping the counter between polls fires a fresh event with the
    /// new depth + the head_workload that was set by the registry's
    /// most recent dispatch.
    #[test]
    fn depth_change_emits_with_head_workload() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(None));
        let mut ch = AiplaneIntentChannel::new(Arc::clone(&in_flight), Arc::clone(&last));
        let _ = ch.poll(); // burn the first-emit slot

        // Simulate a registry dispatch.
        in_flight.store(1, Ordering::SeqCst);
        if let Ok(mut g) = last.lock() {
            *g = Some("rerank".into());
        }
        assert_eq!(
            ch.poll(),
            Some(IntentEvent::NpuQueue {
                depth: 1,
                head_workload: Some("rerank".into()),
            })
        );
        // Same depth → suppress.
        assert_eq!(ch.poll(), None);
    }
}
