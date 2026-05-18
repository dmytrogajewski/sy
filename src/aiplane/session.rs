//! Shared session pool for the aiplane worker process.
//!
//! Historical context: an in-process `Mutex<()>` (`npu_lock`)
//! serialised XDNA's single-context constraint when every workload
//! lived in the same daemon process. The supervisor now spawns one
//! `sy aiplane worker --kind X` child per `WorkloadKind`, so each
//! process owns its own `/dev/accel/accel0` HW context and the
//! intra-process mutex is no longer load-bearing. The struct is
//! the boot-time hand-off slot for the `load(&SessionPool)` trait
//! signature; stateful resources (shared tokenizer caches, warm-pool
//! counters) attach to it without re-plumbing every workload.

/// Per-process session-pool handle. Carries no state today; see
/// the module docs for the design rationale.
pub struct SessionPool {}

impl SessionPool {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for SessionPool {
    fn default() -> Self {
        Self::new()
    }
}
