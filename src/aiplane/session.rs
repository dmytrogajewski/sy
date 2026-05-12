//! Shared ORT environment + NPU mutex + per-pass run context.
//!
//! `SessionPool` is the *only* place that creates ORT sessions inside
//! the daemon. Workloads borrow it during `load()` to build their
//! session via the VitisAI EP (or CPU EP as fallback). The pool
//! holds a global `Mutex<()>` (`npu_lock`) that workload `run()`
//! implementations acquire before calling `session.run(...)` — XDNA
//! is single-context and concurrent NPU inference from two threads
//! would EAGAIN one of them.
//!
//! `RunCtx` is the per-invocation cancellation + throughput plumbing
//! lifted verbatim from the old `knowledge::runctx` module. It's
//! generic enough that any long-running pass (knowledge indexing,
//! batched STT, future denoise of a long audio file) can carry it.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

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

/// Per-pass cancellation + throughput context. Cloned into worker
/// threads; `cancel.store(true)` is observed cooperatively at chunk
/// boundaries inside long-running passes (indexing, batched embed).
#[derive(Clone)]
pub struct RunCtx {
    pub cancel: Arc<AtomicBool>,
    /// Optional inter-batch throttle. The knowledge indexer uses
    /// this to spread CPU/NPU load when running a scheduled
    /// background pass.
    pub throttle: Duration,
    /// EMA throughput counter (chunks-or-items per second).
    counter: Arc<AtomicUsize>,
    started_at: Arc<Mutex<Instant>>,
}

impl RunCtx {
    pub fn interactive() -> Self {
        Self::new(Duration::ZERO)
    }

    pub fn new(throttle: Duration) -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            throttle,
            counter: Arc::new(AtomicUsize::new(0)),
            started_at: Arc::new(Mutex::new(Instant::now())),
        }
    }

    pub fn for_daemon_pass(cancel: Arc<AtomicBool>, throttle: Duration) -> Self {
        Self {
            cancel,
            throttle,
            counter: Arc::new(AtomicUsize::new(0)),
            started_at: Arc::new(Mutex::new(Instant::now())),
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn record(&self, n: usize) {
        self.counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn after_batch(&self) {
        if !self.throttle.is_zero() {
            std::thread::sleep(self.throttle);
        }
    }

    /// Items per second since context creation. Returns None if
    /// fewer than 10 ms have elapsed (avoids div-by-near-zero).
    pub fn throughput(&self) -> Option<f32> {
        let elapsed = self
            .started_at
            .lock()
            .expect("started_at poisoned")
            .elapsed();
        if elapsed.as_millis() < 10 {
            return None;
        }
        let n = self.counter.load(Ordering::Relaxed);
        Some((n as f32) / elapsed.as_secs_f32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

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
            // Tiny delay to make sure t1 gets the lock first.
            thread::sleep(Duration::from_millis(10));
            p2.with_npu(|| {
                c2.fetch_add(1, Ordering::SeqCst);
            });
        });
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn runctx_records_throughput() {
        let ctx = RunCtx::interactive();
        ctx.record(5);
        thread::sleep(Duration::from_millis(20));
        let tps = ctx.throughput().expect("non-zero elapsed");
        assert!(tps > 0.0, "throughput must be positive after recording");
    }

    #[test]
    fn runctx_cancel_is_visible() {
        let ctx = RunCtx::interactive();
        assert!(!ctx.cancelled());
        ctx.cancel.store(true, Ordering::SeqCst);
        assert!(ctx.cancelled());
    }
}
