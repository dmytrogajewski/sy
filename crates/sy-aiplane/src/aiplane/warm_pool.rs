//! Per-workload warm-pool tier semantics (SPEC §4.3 / ROADMAP
//! arch-aiplane-scheduler Step 3).
//!
//! Three tiers, each with their own warm-vs-cold rule:
//!   * **Always warm** — VAD, EyeTrack. Stay loaded across idle
//!     periods because their callers can't tolerate the cold-load
//!     latency (sub-frame budgets).
//!   * **TTL warm** — STT, Embed. Loaded on first call; stay warm
//!     for [`TTL_WARM_DURATION`] after the last [`WarmPool::touch`];
//!     drop out of the warm set when the TTL elapses.
//!   * **LRU warm** — Rerank, OCR, CLIP, TTS, Denoise. The
//!     most-recently-touched [`LRU_MAX`] of this group stay warm;
//!     any older one falls out. Aligned with SPEC §4.3 "LRU, max-3
//!     concurrent warm".
//!
//! Step 3 just lands the bookkeeping + the `warm_models` status
//! surface. Actually freeing the device on eviction (sending
//! `WorkerReq::Shutdown` to the child + reaping its `Workload::unload`)
//! lands alongside Step 4's cancellation machinery, which needs the
//! same child-process lifecycle hooks.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sy_core::WorkloadKind;

/// Maximum number of LRU-tier workloads kept warm simultaneously
/// (SPEC §4.3). Rerank/OCR/CLIP/TTS/Denoise compete for these slots.
pub const LRU_MAX: usize = 3;

/// Idle window before a TTL-tier workload (STT, Embed) is considered
/// cold (SPEC §4.3 "Warm-on-activity, 15-min idle TTL").
pub const TTL_WARM_DURATION: Duration = Duration::from_secs(15 * 60);

/// Which warm-tier policy applies to a given workload (SPEC §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    AlwaysWarm,
    Ttl,
    Lru,
}

/// Static tier classification — derived from SPEC §4.3's tier table.
/// Public so callers (status writer, tests) can interrogate without
/// re-encoding the table.
pub fn tier_for(kind: WorkloadKind) -> Tier {
    match kind {
        WorkloadKind::Vad | WorkloadKind::EyeTrack => Tier::AlwaysWarm,
        WorkloadKind::Stt | WorkloadKind::Embed => Tier::Ttl,
        WorkloadKind::Rerank
        | WorkloadKind::Ocr
        | WorkloadKind::Clip
        | WorkloadKind::Tts
        | WorkloadKind::Denoise => Tier::Lru,
    }
}

/// Clock abstraction so `warm_pool_ttl_idle_eviction` can advance
/// time without sleeping 15 minutes. The production clock is
/// [`Instant::now`].
pub type ClockFn = Arc<dyn Fn() -> Instant + Send + Sync>;

/// In-memory tier state. Construct via [`WarmPool::new`] (production)
/// or [`WarmPool::with_clock`] (tests with an injectable clock).
pub struct WarmPool {
    ttl_last_touched: HashMap<WorkloadKind, Instant>,
    /// LRU-tier ring buffer; front = most recently touched.
    lru: VecDeque<WorkloadKind>,
    ttl_duration: Duration,
    max_lru: usize,
    clock: ClockFn,
}

impl WarmPool {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }

    pub fn with_clock(clock: ClockFn) -> Self {
        Self {
            ttl_last_touched: HashMap::new(),
            lru: VecDeque::new(),
            ttl_duration: TTL_WARM_DURATION,
            max_lru: LRU_MAX,
            clock,
        }
    }

    /// Record activity on `kind`. Returns the workload kind that just
    /// fell out of the LRU window (if any) so the caller can issue a
    /// `WorkerReq::Shutdown` and free the device — Step 4 wires that
    /// follow-up; Step 3's caller can `let _ = ...` until then.
    pub fn touch(&mut self, kind: WorkloadKind) -> Option<WorkloadKind> {
        match tier_for(kind) {
            Tier::AlwaysWarm => None,
            Tier::Ttl => {
                self.ttl_last_touched.insert(kind, (self.clock)());
                None
            }
            Tier::Lru => {
                self.lru.retain(|k| *k != kind);
                self.lru.push_front(kind);
                if self.lru.len() > self.max_lru {
                    self.lru.pop_back()
                } else {
                    None
                }
            }
        }
    }

    /// Workloads currently considered warm. Always-warm kinds are
    /// included unconditionally; TTL kinds appear while inside the
    /// idle window; LRU kinds appear in MRU-first order capped at
    /// [`LRU_MAX`]. Sorted alphabetically (by `as_str`) for stable
    /// `Status.warm_models` output.
    pub fn warm_kinds(&self) -> Vec<WorkloadKind> {
        let now = (self.clock)();
        let mut out: Vec<WorkloadKind> = vec![WorkloadKind::Vad, WorkloadKind::EyeTrack];
        for (k, t) in &self.ttl_last_touched {
            if now.saturating_duration_since(*t) < self.ttl_duration {
                out.push(*k);
            }
        }
        out.extend(self.lru.iter().copied());
        out.sort_by_key(|k| k.as_str());
        out.dedup();
        out
    }

    /// Convenience wrapper around [`Self::warm_kinds`] that yields
    /// the kind name strings the daemon's `Status.warm_models`
    /// snapshot consumes.
    pub fn warm_model_names(&self) -> Vec<String> {
        self.warm_kinds()
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect()
    }
}

impl Default for WarmPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mock clock for TTL test. Caller bumps `advance` to fast-forward
    /// the clock; reads pull the current `(base + advance)` value.
    struct MockClock {
        base: Instant,
        advance: Mutex<Duration>,
    }

    impl MockClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                advance: Mutex::new(Duration::ZERO),
            })
        }

        fn now(self: &Arc<Self>) -> Instant {
            self.base + *self.advance.lock().unwrap()
        }

        fn advance_by(self: &Arc<Self>, by: Duration) {
            *self.advance.lock().unwrap() += by;
        }
    }

    fn mock_pool() -> (WarmPool, Arc<MockClock>) {
        let mc = MockClock::new();
        let mc_for_fn = Arc::clone(&mc);
        let clock: ClockFn = Arc::new(move || mc_for_fn.now());
        (WarmPool::with_clock(clock), mc)
    }

    #[test]
    fn warm_pool_keeps_always_warm() {
        // SPEC §4.3: VAD + EyeTrack are warm even without any touch.
        // No matter how many LRU workloads churn through, they stay
        // in the warm set.
        let mut pool = WarmPool::new();
        for k in [
            WorkloadKind::Rerank,
            WorkloadKind::Ocr,
            WorkloadKind::Clip,
            WorkloadKind::Tts,
            WorkloadKind::Denoise,
        ] {
            pool.touch(k);
        }
        let warm = pool.warm_kinds();
        assert!(
            warm.contains(&WorkloadKind::Vad),
            "vad must always be warm: {warm:?}"
        );
        assert!(
            warm.contains(&WorkloadKind::EyeTrack),
            "eye-track must always be warm: {warm:?}"
        );
    }

    #[test]
    fn warm_pool_evicts_lru_first() {
        // Push four LRU-tier workloads in sequence; the oldest
        // (Rerank) must be evicted on the fourth touch since
        // LRU_MAX=3.
        let mut pool = WarmPool::new();
        let evicted_after_first = pool.touch(WorkloadKind::Rerank);
        assert!(evicted_after_first.is_none());
        assert!(pool.touch(WorkloadKind::Ocr).is_none());
        assert!(pool.touch(WorkloadKind::Clip).is_none());
        let evicted = pool.touch(WorkloadKind::Tts);
        assert_eq!(evicted, Some(WorkloadKind::Rerank));
        let warm = pool.warm_kinds();
        assert!(!warm.contains(&WorkloadKind::Rerank));
        assert!(warm.contains(&WorkloadKind::Ocr));
        assert!(warm.contains(&WorkloadKind::Clip));
        assert!(warm.contains(&WorkloadKind::Tts));
    }

    #[test]
    fn warm_pool_ttl_idle_eviction() {
        // STT touched; immediately warm; advance the mock clock past
        // TTL_WARM_DURATION; STT must drop out of warm_kinds.
        let (mut pool, clock) = mock_pool();
        pool.touch(WorkloadKind::Stt);
        assert!(
            pool.warm_kinds().contains(&WorkloadKind::Stt),
            "STT should be warm immediately after touch"
        );
        clock.advance_by(TTL_WARM_DURATION + Duration::from_secs(1));
        assert!(
            !pool.warm_kinds().contains(&WorkloadKind::Stt),
            "STT should fall out of warm set past TTL"
        );
    }

    #[test]
    fn warm_pool_re_touch_resets_ttl() {
        // Touching an already-TTL-warm kind inside its window must
        // refresh the timer — otherwise an active session would
        // suddenly evict at the original 15-min mark.
        let (mut pool, clock) = mock_pool();
        pool.touch(WorkloadKind::Embed);
        clock.advance_by(TTL_WARM_DURATION / 2);
        pool.touch(WorkloadKind::Embed); // re-touch refreshes
        clock.advance_by(TTL_WARM_DURATION - Duration::from_secs(1));
        assert!(
            pool.warm_kinds().contains(&WorkloadKind::Embed),
            "re-touch must refresh the TTL window"
        );
    }

    #[test]
    fn warm_pool_lru_re_touch_avoids_eviction() {
        // Re-touching the LRU head must NOT count as a fresh
        // insertion that would evict the oldest. Order check: touch
        // Rerank, Ocr, Clip, Rerank — after these four, Rerank stays
        // at the head and no eviction happens.
        let mut pool = WarmPool::new();
        pool.touch(WorkloadKind::Rerank);
        pool.touch(WorkloadKind::Ocr);
        pool.touch(WorkloadKind::Clip);
        let evicted = pool.touch(WorkloadKind::Rerank);
        assert!(evicted.is_none(), "re-touch must not evict");
        let warm = pool.warm_kinds();
        for k in [WorkloadKind::Rerank, WorkloadKind::Ocr, WorkloadKind::Clip] {
            assert!(warm.contains(&k), "{k:?} expected warm: {warm:?}");
        }
    }

    #[test]
    fn warm_model_names_are_alphabetical_strings() {
        // Status snapshot is wire-stable: callers compare against
        // sorted lowercase names. Drift here breaks the doctor
        // recipe.
        let mut pool = WarmPool::new();
        pool.touch(WorkloadKind::Embed);
        let names = pool.warm_model_names();
        // Should at minimum contain always-warm + the embed TTL.
        assert!(names.contains(&"vad".to_string()));
        assert!(names.contains(&"eye-track".to_string()));
        assert!(names.contains(&"embed".to_string()));
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "warm_model_names must be sorted");
    }
}
