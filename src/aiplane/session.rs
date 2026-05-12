//! Shared NPU mutex for the aiplane.
//!
//! `SessionPool` is the *only* place that creates ORT sessions inside
//! the daemon. Workloads borrow it during `load()` to build their
//! session via the VitisAI EP (or CPU EP as fallback). The pool
//! holds a global `Mutex<()>` (`npu_lock`) that workload `run()`
//! implementations acquire before calling `session.run(...)` — XDNA
//! is single-context and concurrent NPU inference from two threads
//! would EAGAIN one of them.
//!
//! Per-pass cancellation + throttling is *not* a generic-aiplane
//! concern: knowledge's `RunCtx` carries an adaptive CPU-cap throttle
//! tied to `sources::cpu_max_percent`. Workloads that want a similar
//! ctx build their own or pass `Arc<AtomicBool>` directly.

use std::sync::Mutex;

/// Acquired by every NPU-bound `Workload::run` before dispatching to
/// the ORT session. The lock is non-reentrant — workloads that need
/// to chain multiple inferences (e.g. OCR detector → recogniser)
/// should chain inside one lock acquisition, not nest.
pub struct SessionPool {
    npu_lock: Mutex<()>,
}

impl SessionPool {
    pub fn new() -> Self {
        Self {
            npu_lock: Mutex::new(()),
        }
    }

    /// Run a closure while holding the NPU mutex. CPU-EP workloads
    /// should NOT call this — they don't contend for /dev/accel.
    pub fn with_npu<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.npu_lock.lock().expect("npu lock poisoned");
        f()
    }
}

impl Default for SessionPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn npu_lock_serialises_concurrent_use() {
        let pool = Arc::new(SessionPool::new());
        let counter = Arc::new(AtomicUsize::new(0));
        let p1 = pool.clone();
        let c1 = counter.clone();
        let p2 = pool.clone();
        let c2 = counter.clone();
        let t1 = thread::spawn(move || {
            p1.with_npu(|| {
                let prev = c1.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
                // While holding the lock, c2's `with_npu` is parked,
                // so the second thread can't have incremented yet.
                let after = c1.load(Ordering::SeqCst);
                assert_eq!(after, prev + 1, "second thread leaked into NPU section");
            });
        });
        let t2 = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            p2.with_npu(|| {
                c2.fetch_add(1, Ordering::SeqCst);
            });
        });
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }
}
