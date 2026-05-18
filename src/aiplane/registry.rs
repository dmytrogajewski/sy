//! Workload registry: enumerates every NPU-eligible workload sy can
//! host inside the `sy-aiplane.service` daemon and dispatches typed
//! input/output between the IPC layer and concrete `Workload` impls.
//!
//! The registry is **the** generalisation point of the aiplane crate:
//! adding a new workload is a Workload-trait impl + one line in
//! `workloads::register_all()` + (optionally) new variants in
//! `WorkloadInput`/`WorkloadOutput`. Everything else — IPC ser/de,
//! session pool, status snapshot, CLI dispatch — picks it up by
//! enumerating `WorkloadKind`.

use std::{path::PathBuf, sync::Mutex, time::Instant};

use anyhow::Result;

use super::session::SessionPool;

// Data-shape vocabulary lives in `sy-core`. Re-exported here so the
// `super::registry::<Type>` import path consumers use throughout the
// aiplane / knowledge / IPC modules keeps working unchanged. The
// `Workload` trait itself stays local because it depends on
// `SessionPool`.
pub use sy_core::workload::{
    WorkloadHealth, WorkloadInput, WorkloadKind, WorkloadOutput, WorkloadState,
};

/// Anything that can serve an NPU-eligible workload through the
/// shared `SessionPool`. The trait is intentionally non-async:
/// concurrency is handled by the registry's worker thread which
/// dispatches one request at a time per `WorkloadKind`. NPU
/// serialisation is enforced by the `SessionPool`'s NPU mutex.
pub trait Workload: Send + Sync {
    fn kind(&self) -> WorkloadKind;

    /// Human-readable model identifier surfaced to status / logs
    /// (e.g. `"multilingual-e5-base"`). Used as the on-disk
    /// directory name under `~/.cache/sy/aiplane/<stem>/`.
    fn model_stem(&self) -> &'static str;

    /// Idempotent. The pool calls this before the first `run()`.
    /// Cached state lives behind `&self` (a `Mutex<Option<...>>`
    /// inside the impl) so subsequent loads are cheap no-ops.
    fn load(&self, pool: &SessionPool) -> Result<()>;

    /// Run one inference. Implementations validate the input
    /// variant matches what they expect; mismatched variants
    /// return a clear error rather than panicking.
    fn run(&self, input: WorkloadInput) -> Result<WorkloadOutput>;

    /// Run a batched inference. Default impl loops over `run`; the
    /// supervisor calls this from `run_batch` so a workload that
    /// supports session-level batching (e.g. a rerank model exported
    /// at `(B, 512)`) can override and turn N kernel launches into
    /// one. Returns one output per input, in order, or `Err` on the
    /// first failure.
    fn run_batch(&self, inputs: Vec<WorkloadInput>) -> Result<Vec<WorkloadOutput>> {
        let mut out = Vec::with_capacity(inputs.len());
        for input in inputs {
            out.push(self.run(input)?);
        }
        Ok(out)
    }

    /// Best-effort release of the loaded ORT session. Called by the
    /// pool's LRU eviction when memory pressure forces it. Workloads
    /// that hold extra state (tokenizers, image preprocessors) drop
    /// them here too.
    fn unload(&self);

    fn health(&self) -> WorkloadHealth;

    /// Cooperatively abort the currently-inflight [`run`](Self::run) /
    /// [`run_batch`](Self::run_batch). Returns `true` when the
    /// workload understood the request and started winding down (the
    /// in-flight call will surface `Err`); `false` means the workload
    /// has no cooperative cancel path and the supervisor's 500 ms
    /// SIGKILL guard (SPEC §4.3 "Cancellation pattern") will fall
    /// back to terminating the child process.
    ///
    /// Default impl returns `false`. Real ORT-backed workloads will
    /// override this in a follow-up step to call
    /// `RunOptions::SetTerminate(true)` from a side thread.
    fn try_cancel(&self) -> bool {
        false
    }
}

/// The registry the daemon's req worker dispatches through. Owns one
/// boxed `Workload` per kind plus the shared `SessionPool` they share.
pub struct Registry {
    pub pool: std::sync::Arc<SessionPool>,
    workloads: std::collections::HashMap<WorkloadKind, std::sync::Arc<dyn Workload>>,
    stats: Mutex<std::collections::HashMap<WorkloadKind, WorkloadHealth>>,
}

impl Registry {
    pub fn new(pool: std::sync::Arc<SessionPool>) -> Self {
        Self {
            pool,
            workloads: std::collections::HashMap::new(),
            stats: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Register one `Workload`. Panics if the kind is already
    /// registered — registration happens once at daemon startup,
    /// double-registration is a bug.
    pub fn register(&mut self, w: std::sync::Arc<dyn Workload>) {
        let k = w.kind();
        if self.workloads.contains_key(&k) {
            panic!("workload {k} registered twice");
        }
        self.workloads.insert(k, w);
    }

    /// Sorted list of every registered workload kind. Currently only
    /// the dispatch tests in this module consume this; the daemon
    /// status snapshot uses the supervisor's `all_health` directly.
    /// When (if) per-worker process registries gain their own status
    /// surface, this moves out of `#[cfg(test)]`.
    #[cfg(test)]
    pub fn kinds(&self) -> Vec<WorkloadKind> {
        let mut v: Vec<_> = self.workloads.keys().copied().collect();
        v.sort_by_key(|k| k.as_str());
        v
    }

    pub fn run(&self, kind: WorkloadKind, input: WorkloadInput) -> Result<WorkloadOutput> {
        let w = self
            .workloads
            .get(&kind)
            .ok_or_else(|| anyhow::anyhow!("workload {kind} not registered"))?
            .clone();
        // Lazy load on first call.
        w.load(&self.pool)?;
        let t0 = Instant::now();
        let res = w.run(input);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut stats = self.stats.lock().expect("stats poisoned");
        let entry = stats.entry(kind).or_default();
        match &res {
            Ok(_) => {
                entry.calls += 1;
                entry.last_call_unix = unix_now();
                // EMA with alpha=0.2.
                entry.ema_ms = if entry.ema_ms == 0.0 {
                    elapsed_ms
                } else {
                    0.2 * elapsed_ms + 0.8 * entry.ema_ms
                };
            }
            Err(_) => {
                entry.errors += 1;
            }
        }
        res
    }

    #[cfg(test)]
    pub fn health(&self, kind: WorkloadKind) -> WorkloadHealth {
        let mut h = self
            .stats
            .lock()
            .expect("stats poisoned")
            .get(&kind)
            .cloned()
            .unwrap_or_default();
        if let Some(w) = self.workloads.get(&kind) {
            let from_workload = w.health();
            h.state = from_workload.state;
            h.loaded = from_workload.loaded;
            h.backend = from_workload.backend;
        }
        h
    }

    /// Snapshot of every registered workload's health, keyed by
    /// `WorkloadKind::as_str()`. Today's daemon status writer goes
    /// through the supervisor's `all_health`; this in-process variant
    /// is kept for the dispatch tests below and will move out of
    /// `#[cfg(test)]` if a future in-daemon registry resurfaces.
    #[cfg(test)]
    pub fn all_health(&self) -> std::collections::HashMap<String, WorkloadHealth> {
        self.kinds()
            .into_iter()
            .map(|k| (k.as_str().to_string(), self.health(k)))
            .collect()
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `~/.cache/sy/aiplane/` (overridable via `SY_AIPLANE_CACHE_DIR` for
/// tests). All workload caches live under this.
pub fn cache_root() -> PathBuf {
    if let Some(v) = std::env::var_os("SY_AIPLANE_CACHE_DIR") {
        return PathBuf::from(v);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/sy/aiplane")
}

#[cfg(test)]
mod tests {
    use super::*;

    // WorkloadKind round-trip + unknown-rejection tests moved to
    // `sy-core::workload::tests` along with the type itself.

    #[test]
    fn cache_root_respects_override() {
        std::env::set_var("SY_AIPLANE_CACHE_DIR", "/tmp/sy-test-cache");
        assert_eq!(cache_root(), PathBuf::from("/tmp/sy-test-cache"));
        std::env::remove_var("SY_AIPLANE_CACHE_DIR");
    }

    #[test]
    fn registry_dispatches_to_registered_workload_via_trait_object() {
        // Exercises the full path: register `dyn Workload`, dispatch
        // through Registry::run, observe the trait's run/load are
        // both invoked. Also pulls WorkloadHealth + the EMA counter
        // through enough code that the compiler stops calling them
        // dead.
        use super::super::session::SessionPool;
        use super::super::workloads::fake::FakeWorkload;
        use std::sync::Arc;

        let pool = Arc::new(SessionPool::new());
        let mut reg = Registry::new(pool);
        reg.register(Arc::new(FakeWorkload::embed()));

        assert_eq!(reg.kinds(), vec![WorkloadKind::Embed]);

        let out = reg
            .run(
                WorkloadKind::Embed,
                WorkloadInput::Text {
                    text: "hello".into(),
                },
            )
            .expect("dispatch");
        match out {
            WorkloadOutput::Vector { vector } => assert!(!vector.is_empty()),
            _ => panic!("expected Vector"),
        }

        let h = reg.health(WorkloadKind::Embed);
        assert!(h.loaded);
        assert!(h.calls >= 1);
        assert_eq!(h.backend, "fake");
    }

    #[test]
    fn all_health_enumerates_every_registered_kind() {
        // The status snapshot must list a row for every kind in
        // the registry, even those that have never been called —
        // that's how `NotPrepared` and `Failed` states become
        // visible to the user without first triggering a request.
        use super::super::session::SessionPool;
        use super::super::workloads::fake::FakeWorkload;
        use std::sync::Arc;
        let pool = Arc::new(SessionPool::new());
        let mut reg = Registry::new(pool);
        reg.register(Arc::new(FakeWorkload::new(WorkloadKind::Embed)));
        reg.register(Arc::new(FakeWorkload::new(WorkloadKind::Rerank)));
        let all = reg.all_health();
        assert_eq!(all.len(), 2);
        assert!(all.contains_key("embed"));
        assert!(all.contains_key("rerank"));
        // Both unloaded → NotPrepared, not Ready.
        for h in all.values() {
            assert_eq!(h.state, WorkloadState::NotPrepared);
            assert!(!h.loaded);
        }
    }

    // WorkloadState ser/default tests moved to `sy-core::workload::tests`.

    #[test]
    fn registry_rejects_unregistered_kind() {
        use super::super::session::SessionPool;
        use std::sync::Arc;
        let reg = Registry::new(Arc::new(SessionPool::new()));
        let res = reg.run(
            WorkloadKind::Vad,
            WorkloadInput::Audio {
                pcm: vec![],
                sr: 16_000,
            },
        );
        assert!(res.is_err());
    }
}
