//! Race-free cancellation registry for IPC v1 (SPEC §4.2
//! "Cancellation pattern", step 1; SPEC §2.3 SourceKit-LSP deep dive).
//!
//! The single load-bearing invariant: **a `request_id → token`
//! mapping must be registered before the worker future is spawned.**
//! Otherwise a `system.cancel{target_request_id}` that arrives before
//! the server has finished setting up cancellation would no-op the
//! mapping, and the worker would run to completion despite the
//! client's request.
//!
//! The type shape encodes the rule: [`CancelRegistry::register`]
//! returns a [`CancelGuard`] whose existence is the proof that the
//! mapping is in place. [`dispatch_with_cancel`] consumes a guard
//! by value — there is intentionally no `register_after_spawn`
//! escape hatch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use ulid::Ulid;

/// In-flight request → cancellation token map. Shared across the
/// server's accept loop and the `system.cancel` handler.
#[derive(Default, Clone)]
pub struct CancelRegistry {
    inner: Arc<Mutex<HashMap<Ulid, RegEntry>>>,
}

#[derive(Clone)]
struct RegEntry {
    token: CancellationToken,
    /// Monotonic-per-id generation counter. Lets a guard tell whether
    /// its own slot is still current at Drop time, so a stale guard
    /// from a previously-completed request does not evict the live
    /// entry of a re-registered same-id request (LSP race property).
    epoch: u64,
}

/// RAII handle returned by [`CancelRegistry::register`]. Holding the
/// guard keeps the registry mapping alive; dropping it removes the
/// mapping (but only if the slot still belongs to this guard — a
/// concurrent re-registration wins). Move the guard into
/// [`dispatch_with_cancel`] to enforce SPEC §4.2 step ordering.
pub struct CancelGuard {
    id: Ulid,
    epoch: u64,
    token: CancellationToken,
    inner: Arc<Mutex<HashMap<Ulid, RegEntry>>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve a cancellation slot for `id` and return its guard. If
    /// the id is already in the map (a recycled or concurrently-
    /// in-flight ULID), the older entry is replaced — the newer
    /// registration wins, and the older guard becomes a no-op at
    /// drop time.
    pub fn register(&self, id: Ulid) -> CancelGuard {
        let token = CancellationToken::new();
        let mut inner = self.inner.lock().expect("cancel registry poisoned");
        let epoch = inner.get(&id).map(|e| e.epoch.wrapping_add(1)).unwrap_or(0);
        inner.insert(
            id,
            RegEntry {
                token: token.clone(),
                epoch,
            },
        );
        CancelGuard {
            id,
            epoch,
            token,
            inner: Arc::clone(&self.inner),
        }
    }

    /// Cancel the token currently bound to `id`. Returns `true` when
    /// a live mapping was cancelled and `false` when the id was not
    /// registered (SPEC §4.2 "cancel before register" is a no-op,
    /// not an error).
    pub fn cancel(&self, id: Ulid) -> bool {
        let inner = self.inner.lock().expect("cancel registry poisoned");
        match inner.get(&id) {
            Some(e) => {
                e.token.cancel();
                true
            }
            None => false,
        }
    }
}

impl CancelGuard {
    /// Clone of the cancellation token owned by this guard. The
    /// returned token cancels when either the registry receives a
    /// matching `cancel(id)` or the guard's `token.cancel()` is
    /// called directly (e.g. on supervisor shutdown).
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().expect("cancel registry poisoned");
        if let Some(e) = inner.get(&self.id) {
            if e.epoch == self.epoch {
                inner.remove(&self.id);
            }
        }
    }
}

/// Outcome of [`dispatch_with_cancel`].
#[derive(Debug, PartialEq, Eq)]
pub enum Dispatched<R> {
    /// The worker future completed normally.
    Completed(R),
    /// The token cancelled before the worker returned.
    Cancelled,
}

/// Run `worker_fn` while honouring the cancellation token bound to
/// `guard`. Takes the guard by value so the registry mapping is
/// already in place — there is no `register_after_spawn` shortcut
/// the caller could reach for.
///
/// `worker_fn` receives a clone of the token so its hot path can
/// `tokio::select!` on it for fine-grained early-exit.
pub async fn dispatch_with_cancel<F, Fut, R>(guard: CancelGuard, worker_fn: F) -> Dispatched<R>
where
    F: FnOnce(CancellationToken) -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let token = guard.token();
    let worker = worker_fn(token.clone());
    let outcome = tokio::select! {
        // `biased` polls the cancellation branch first, so a race
        // where the worker's own future also wakes on the token (a
        // common pattern) resolves to `Cancelled` rather than
        // `Completed`. The fall-through `is_cancelled` check covers
        // the very narrow case where the worker returns by some
        // unrelated path on the same poll the cancel fires.
        biased;
        _ = token.cancelled() => Dispatched::Cancelled,
        r = worker => {
            if token.is_cancelled() {
                Dispatched::Cancelled
            } else {
                Dispatched::Completed(r)
            }
        }
    };
    outcome
    // `guard` drops here, removing the registry entry.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cancel_before_spawn_is_a_no_op_then_armed() {
        // SPEC §4.2: a `cancel(id)` that arrives before any
        // `register(id)` is a no-op (returns false, no panic). After
        // a register-then-drop cycle, re-registering the same id
        // produces a fresh token — proves the registry does not leak
        // a stale cancelled token across recycled ids (the LSP
        // race-prevention property).
        let reg = CancelRegistry::new();
        let id = Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");

        assert!(!reg.cancel(id), "cancel of unregistered id must be a no-op");

        let g1 = reg.register(id);
        drop(g1);

        let g2 = reg.register(id);
        assert!(
            !g2.token().is_cancelled(),
            "re-registration must hand out a fresh token"
        );
        assert!(reg.cancel(id), "second registration is cancellable");
        assert!(
            g2.token().is_cancelled(),
            "cancel arms the live token bound to g2"
        );
    }

    #[test]
    fn second_register_wins_over_concurrent_first() {
        // Two registrations of the same id overlap (a recycled ULID
        // mid-flight). The second registration replaces the slot;
        // dropping the first guard does NOT evict the second's
        // mapping. Cancel(id) reaches the second guard's token only;
        // the first guard's token is orphaned and unaffected.
        let reg = CancelRegistry::new();
        let id = Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");

        let g1 = reg.register(id);
        let t1 = g1.token();
        let g2 = reg.register(id);
        let t2 = g2.token();

        drop(g1);
        assert!(
            reg.cancel(id),
            "registry must still hold g2 after g1 dropped"
        );
        assert!(t2.is_cancelled());
        assert!(!t1.is_cancelled());
        drop(g2);
    }

    #[tokio::test]
    async fn cancel_after_register_fires_token() {
        // The worker spawn pattern from SPEC §4.2: register, hand a
        // token to the spawned task, fire `cancel(id)` from the
        // outside. The spawned task's `.cancelled()` await wakes
        // promptly.
        let reg = CancelRegistry::new();
        let id = Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");
        let guard = reg.register(id);
        let token = guard.token();
        let task = tokio::spawn(async move {
            token.cancelled().await;
            "cancelled"
        });
        // Tiny delay so the assert below proves the cancellation
        // signal flows from outside-thread `cancel` → spawned-task
        // wakeup, not from an immediate pre-cancelled token.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(reg.cancel(id));
        let r = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("task did not wake within 1s")
            .expect("task join");
        assert_eq!(r, "cancelled");
    }

    #[tokio::test]
    async fn dispatch_with_cancel_completes_when_worker_finishes_first() {
        let reg = CancelRegistry::new();
        let id = Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");
        let guard = reg.register(id);
        let out: Dispatched<u32> = dispatch_with_cancel(guard, |_token| async { 42 }).await;
        assert_eq!(out, Dispatched::Completed(42));
        // Guard dropped inside dispatch_with_cancel; registry must
        // no longer carry the id.
        assert!(!reg.cancel(id));
    }

    #[tokio::test]
    async fn dispatch_with_cancel_yields_cancelled_when_token_fires() {
        let reg = CancelRegistry::new();
        let id = Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");
        let guard = reg.register(id);
        let reg2 = reg.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            reg2.cancel(id);
        });
        let out: Dispatched<()> = dispatch_with_cancel(guard, |token| async move {
            token.cancelled().await;
        })
        .await;
        assert_eq!(out, Dispatched::Cancelled);
    }
}
