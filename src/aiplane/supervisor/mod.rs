//! Parent-side worker manager: spawns one child process per
//! configured NPU workload kind, polls each via `WorkerReq::Health`,
//! and restarts on failure with exponential backoff.
//!
//! The supervisor is what makes the multi-process aiplane work
//! around XDNA's "one HW context per process" rule: each child owns
//! its own /dev/accel/accel0 attachment, so embed and rerank can
//! coexist without context swaps.
//!
//! Lifecycle:
//!
//! 1. `Supervisor::new()` — empty registry of children.
//! 2. `ensure(kind)` — spawn the child if not already managed, wait
//!    for its socket to bind. Idempotent.
//! 3. Background poll thread fires `Req::Health` every second; on
//!    consecutive failures or process exit, the child is restarted
//!    via `health::restart_policy`.
//! 4. `run_batch(kind, inputs)` — synchronous proxy to the worker
//!    socket. Read timeout scales with batch size.
//! 5. `shutdown()` — send `Req::Shutdown` to each child, wait for
//!    ack with deadline, escalate to SIGTERM if needed.

pub mod child;
pub mod health;

use std::sync::OnceLock;

/// Process-wide handle to the running daemon's supervisor. Set once
/// by `knowledge::daemon::run()` when `SY_AIPLANE_WORKERS=1`; callers
/// (indexer, search-rerank, status writer) read it via `current()`
/// and fall back to the legacy in-process path when `None`.
static CURRENT: OnceLock<Arc<Supervisor>> = OnceLock::new();

/// Returns the running supervisor if the daemon initialised one,
/// otherwise `None`. Cheap; no locking.
pub fn current() -> Option<Arc<Supervisor>> {
    CURRENT.get().cloned()
}

/// Install `sup` as the process-wide supervisor. Called exactly once
/// at daemon startup. Panics on re-init — the supervisor is meant to
/// outlive everything else; a second instance would mean we lost
/// track of the first batch of children.
pub fn set_current(sup: Arc<Supervisor>) {
    CURRENT
        .set(sup)
        .ok()
        .expect("supervisor::set_current called twice");
}

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use metrics::gauge;

use super::registry::{WorkloadInput, WorkloadKind, WorkloadOutput, WorkloadState};
use super::warm_pool::WarmPool;
use super::worker_ipc::{self, WorkerHealth, WorkerIpcError, WorkerReq, WorkerResp};

use child::{Child, ChildSpawn, RealSpawn};

const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Time to wait for a freshly-spawned worker to bind its socket
/// before declaring the spawn failed.
const SOCKET_BIND_DEADLINE: Duration = Duration::from_secs(5);
/// Per-call read budget for `RunBatch`. Generous on purpose so a
/// first-call VAIP compile (sub-minute on warm cache, several min
/// cold) fits.
const RUN_BATCH_TIMEOUT: Duration = Duration::from_secs(900);
/// SPEC §4.3 "Cancellation pattern" step 5: the supervisor gives a
/// worker this long to yield after a `WorkerReq::Cancel` before
/// SIGKILLing the child and respawning from the VitisAI compile
/// cache. The roadmap pegs the budget at 500 ms; the constant lives
/// in code so the test asserting "guard fires ≥ 500 ms" can reference
/// it.
pub const CANCEL_YIELD_DEADLINE: Duration = Duration::from_millis(500);
/// Polling interval inside [`Supervisor::cancel`] while watching the
/// worker's `inflight_request_id` to clear. Fine-grained so the guard
/// fires within a few tens of milliseconds of the worker yielding,
/// without burning CPU.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub struct Supervisor {
    spawn: Arc<dyn ChildSpawn>,
    inner: Arc<Mutex<Inner>>,
    /// Per-workload warm-pool bookkeeping (SPEC §4.3). Touched on
    /// every `ensure` + `run_batch` so the status snapshot can
    /// surface the warm set. Step 4 wires actual eviction (child
    /// shutdown + `Workload::unload`) onto the eviction signal.
    warm_pool: Arc<Mutex<WarmPool>>,
}

struct Inner {
    children: HashMap<WorkloadKind, ManagedChild>,
}

struct ManagedChild {
    handle: Box<dyn Child>,
    socket_path: PathBuf,
    last_health: Option<WorkerHealth>,
    last_health_at: Option<Instant>,
    restart_attempts: u32,
    backoff_until: Option<Instant>,
}

impl Supervisor {
    /// Production constructor: children are real `sy aiplane worker`
    /// subprocesses spawned via `std::process::Command`.
    pub fn new() -> Self {
        Self::with_spawn(Arc::new(RealSpawn::new(default_sy_binary())))
    }

    /// Test constructor: pass a fake `ChildSpawn` that produces
    /// in-thread workers.
    pub fn with_spawn(spawn: Arc<dyn ChildSpawn>) -> Self {
        Self {
            spawn,
            inner: Arc::new(Mutex::new(Inner {
                children: HashMap::new(),
            })),
            warm_pool: Arc::new(Mutex::new(WarmPool::new())),
        }
    }

    /// Names of every workload currently in the warm pool (SPEC
    /// §4.3). Surfaced via `Status.warm_models` so `sy aiplane
    /// status` and the doctor recipe can see which models the device
    /// is holding ready.
    pub fn warm_models(&self) -> Vec<String> {
        self.warm_pool
            .lock()
            .expect("warm_pool poisoned")
            .warm_model_names()
    }

    /// Spawn the child for `kind` (idempotent) and block until either
    /// it reports `Ready` or the deadline expires. Returns the
    /// worker's `WorkerHealth` so the daemon's status writer can use
    /// it immediately. The warm pool tracks the touch so eviction
    /// bookkeeping stays accurate even when callers preload kinds
    /// without an immediate inference call.
    pub fn ensure(&self, kind: WorkloadKind, ready_deadline: Duration) -> Result<WorkerHealth> {
        self.ensure_spawned(kind)?;
        let _evicted = self
            .warm_pool
            .lock()
            .expect("warm_pool poisoned")
            .touch(kind);
        self.publish_warm_gauge();
        self.wait_for_ready(kind, ready_deadline)
    }

    /// SPEC §4.6 `sy_models_warm{kind}`. Emits one gauge sample per
    /// [`WorkloadKind`] with value 1 if the kind is currently in the
    /// warm pool, 0 otherwise. Called on every `ensure` and on
    /// shutdown so dashboards see the latest warm set without
    /// polling.
    fn publish_warm_gauge(&self) {
        let warm: std::collections::HashSet<WorkloadKind> = self
            .warm_pool
            .lock()
            .expect("warm_pool poisoned")
            .warm_kinds()
            .into_iter()
            .collect();
        for kind in WorkloadKind::ALL {
            let value = if warm.contains(&kind) { 1.0 } else { 0.0 };
            gauge!("sy_models_warm", "kind" => kind.as_str()).set(value);
        }
    }

    fn ensure_spawned(&self, kind: WorkloadKind) -> Result<()> {
        let mut inner = self.inner.lock().expect("supervisor poisoned");
        // Check-then-act so we don't hold an aliased borrow into
        // `inner.children` across the subsequent `.remove(&kind)`.
        let needs_spawn = match inner.children.get(&kind) {
            Some(mc) => {
                if mc.handle.is_alive() {
                    return Ok(());
                }
                tracing::warn!(
                    target: "sy::aiplane::supervisor",
                    kind = %kind,
                    pid = ?mc.handle.pid(),
                    "child died; respawning"
                );
                true
            }
            None => true,
        };
        if !needs_spawn {
            return Ok(());
        }
        inner.children.remove(&kind);

        let socket = worker_ipc::socket_path(kind);
        let handle = self
            .spawn
            .spawn(kind, &socket)
            .with_context(|| format!("spawn worker {kind}"))?;
        wait_for_socket(&socket, SOCKET_BIND_DEADLINE)
            .with_context(|| format!("worker {kind} did not bind {}", socket.display()))?;
        inner.children.insert(
            kind,
            ManagedChild {
                handle,
                socket_path: socket,
                last_health: None,
                last_health_at: None,
                restart_attempts: 0,
                backoff_until: None,
            },
        );
        Ok(())
    }

    fn wait_for_ready(&self, kind: WorkloadKind, deadline: Duration) -> Result<WorkerHealth> {
        let start = Instant::now();
        loop {
            let socket = self.socket_for(kind)?;
            match worker_ipc::request(&socket, &WorkerReq::Health, HEALTH_PROBE_TIMEOUT) {
                Ok(WorkerResp::Health(h)) => {
                    self.record_health(kind, h.clone());
                    match &h.state {
                        WorkloadState::Ready { .. } => return Ok(h),
                        WorkloadState::Failed { reason } => {
                            anyhow::bail!("worker {kind} failed to load: {reason}");
                        }
                        WorkloadState::NotPrepared => {
                            anyhow::bail!(
                                "worker {kind} reports model not prepared — \
                                 run `python scripts/prep_npu_workload.py --workload {kind}`"
                            );
                        }
                        WorkloadState::Loading | WorkloadState::Unavailable => {}
                    }
                }
                Ok(other) => anyhow::bail!("worker {kind}: unexpected resp {other:?}"),
                Err(WorkerIpcError::WorkerDown) => {
                    // Socket gone while we were probing — worker died
                    // during init. Surface as a clean error; the
                    // restart policy can pick it up next call.
                    if !start.elapsed().lt(&deadline) {
                        anyhow::bail!("worker {kind} socket disappeared during init");
                    }
                }
                Err(WorkerIpcError::Wire(e)) => {
                    return Err(e.context(format!("worker {kind} health probe")));
                }
            }
            if start.elapsed() >= deadline {
                anyhow::bail!("worker {kind} did not become Ready within {deadline:?}");
            }
            std::thread::sleep(HEALTH_POLL_INTERVAL);
        }
    }

    fn socket_for(&self, kind: WorkloadKind) -> Result<PathBuf> {
        let inner = self.inner.lock().expect("supervisor poisoned");
        Ok(inner
            .children
            .get(&kind)
            .map(|c| c.socket_path.clone())
            .unwrap_or_else(|| worker_ipc::socket_path(kind)))
    }

    fn record_health(&self, kind: WorkloadKind, h: WorkerHealth) {
        let mut inner = self.inner.lock().expect("supervisor poisoned");
        if let Some(mc) = inner.children.get_mut(&kind) {
            mc.last_health = Some(h);
            mc.last_health_at = Some(Instant::now());
            if mc.last_health.as_ref().map(|h| h.state.is_ready()) == Some(true) {
                mc.restart_attempts = 0;
                mc.backoff_until = None;
            }
        }
    }

    /// Dispatch a batched inference to the worker for `kind`.
    /// Caller is responsible for matching `inputs` to the workload's
    /// expected `WorkloadInput` variant; the worker validates and
    /// returns a clear error otherwise.
    pub fn run_batch(
        &self,
        kind: WorkloadKind,
        inputs: Vec<WorkloadInput>,
    ) -> Result<Vec<WorkloadOutput>> {
        let socket = self.socket_for(kind)?;
        let req = WorkerReq::RunBatch {
            request_id: ulid::Ulid::nil(),
            inputs,
        };
        match worker_ipc::request(&socket, &req, RUN_BATCH_TIMEOUT) {
            Ok(WorkerResp::RunBatch { outputs }) => Ok(outputs),
            Ok(WorkerResp::Error { msg }) => anyhow::bail!("worker {kind}: {msg}"),
            Ok(other) => anyhow::bail!("worker {kind}: unexpected resp {other:?}"),
            Err(e) => Err(anyhow::anyhow!("worker {kind}: {e}")),
        }
    }

    /// Current pid of the managed child for `kind`, if any. Used by
    /// the SIGKILL-fallback test to assert that [`Supervisor::cancel`]
    /// respawned the worker rather than leaving the original child
    /// stuck. Test-only: production code that wants this should
    /// consume `all_health()` (which returns the worker's
    /// self-reported pid via `WorkerHealth.pid`).
    #[cfg(test)]
    pub fn pid(&self, kind: WorkloadKind) -> Option<u32> {
        let inner = self.inner.lock().expect("supervisor poisoned");
        inner.children.get(&kind).and_then(|mc| mc.handle.pid())
    }

    /// Cooperative cancel of an inflight `RunBatch` on `kind`. Sends
    /// [`WorkerReq::Cancel { request_id }`], then polls the worker's
    /// `WorkerHealth.inflight_request_id` for up to
    /// [`CANCEL_YIELD_DEADLINE`] (SPEC §4.3 step 5: the 500 ms guard).
    ///
    /// On success (the worker yielded — `inflight_request_id` no
    /// longer matches `request_id`), returns `Ok(())`. On timeout,
    /// terminates the child (SIGKILL via [`Child::terminate`]),
    /// respawns it via [`Supervisor::ensure_spawned`], and returns
    /// `Err(anyhow!("worker did not yield in 500 ms; child restarted"))`.
    ///
    /// `request_id == Ulid::nil()` is treated as "best-effort":
    /// supervisor still fires the Cancel + waits the deadline, but
    /// since the worker can't match a nil id against its inflight
    /// tracker the SIGKILL path is the only way out for a stuck
    /// worker.
    pub fn cancel(&self, kind: WorkloadKind, request_id: ulid::Ulid) -> Result<()> {
        let socket = self.socket_for(kind)?;
        match worker_ipc::request(
            &socket,
            &WorkerReq::Cancel { request_id },
            HEALTH_PROBE_TIMEOUT,
        ) {
            Ok(WorkerResp::CancelAck) => {}
            Ok(other) => anyhow::bail!("worker {kind}: unexpected resp to cancel: {other:?}"),
            Err(e) => anyhow::bail!("worker {kind}: cancel send failed: {e}"),
        }
        let start = Instant::now();
        while start.elapsed() < CANCEL_YIELD_DEADLINE {
            match worker_ipc::request(&socket, &WorkerReq::Health, HEALTH_PROBE_TIMEOUT) {
                Ok(WorkerResp::Health(h)) => {
                    if h.inflight_request_id != Some(request_id) {
                        self.record_health(kind, h);
                        return Ok(());
                    }
                }
                Ok(_) | Err(_) => {
                    // Worker is unresponsive — escalate immediately.
                    break;
                }
            }
            std::thread::sleep(CANCEL_POLL_INTERVAL);
        }
        self.escalate_to_kill(kind);
        anyhow::bail!("worker {kind} did not yield in {CANCEL_YIELD_DEADLINE:?}; child restarted")
    }

    /// SIGKILL the worker for `kind` and respawn it. Used by
    /// [`Supervisor::cancel`] when the cooperative cancel guard
    /// expires. Idempotent — if the child is already dead, the
    /// respawn path runs anyway.
    fn escalate_to_kill(&self, kind: WorkloadKind) {
        {
            let mut inner = self.inner.lock().expect("supervisor poisoned");
            if let Some(mc) = inner.children.get_mut(&kind) {
                tracing::warn!(
                    target: "sy::aiplane::supervisor",
                    kind = %kind,
                    pid = ?mc.handle.pid(),
                    "cancel guard expired; SIGKILL + respawn"
                );
                mc.handle.terminate();
            }
        }
        if let Err(e) = self.ensure_spawned(kind) {
            tracing::error!(
                target: "sy::aiplane::supervisor",
                kind = %kind,
                error = %format!("{e:#}"),
                "respawn after cancel failed"
            );
        }
    }

    /// Snapshot of every managed child's last-known health. Returns
    /// the cached value from the most recent poll; the supervisor's
    /// background thread keeps it warm.
    pub fn all_health(&self) -> HashMap<WorkloadKind, Option<WorkerHealth>> {
        let inner = self.inner.lock().expect("supervisor poisoned");
        inner
            .children
            .iter()
            .map(|(k, mc)| (*k, mc.last_health.clone()))
            .collect()
    }

    /// Trigger a fresh Health probe for every managed child and
    /// record the result. Called by the supervisor's poll thread.
    pub fn poll_once(&self) {
        let kinds: Vec<WorkloadKind> = {
            let inner = self.inner.lock().expect("supervisor poisoned");
            inner.children.keys().copied().collect()
        };
        for kind in kinds {
            let socket = match self.socket_for(kind) {
                Ok(s) => s,
                Err(_) => continue,
            };
            match worker_ipc::request(&socket, &WorkerReq::Health, HEALTH_PROBE_TIMEOUT) {
                Ok(WorkerResp::Health(h)) => self.record_health(kind, h),
                Ok(_) | Err(_) => {
                    // Health probe failed or returned garbage. The
                    // health module will decide whether to restart
                    // (see `Supervisor::reap_and_restart`).
                }
            }
        }
        self.reap_and_restart();
    }

    /// Detect dead children, schedule restarts honouring the
    /// per-workload backoff. Idempotent; cheap when nothing has died.
    fn reap_and_restart(&self) {
        let mut to_respawn: Vec<WorkloadKind> = Vec::new();
        {
            let mut inner = self.inner.lock().expect("supervisor poisoned");
            for (kind, mc) in inner.children.iter_mut() {
                if mc.handle.is_alive() {
                    continue;
                }
                if let Some(until) = mc.backoff_until {
                    if Instant::now() < until {
                        continue;
                    }
                }
                mc.restart_attempts += 1;
                mc.backoff_until =
                    Some(Instant::now() + health::backoff_for_attempt(mc.restart_attempts));
                to_respawn.push(*kind);
            }
            // Keep the dead `ManagedChild` records in place so the
            // backoff timer + restart_attempts survive across the
            // respawn call below; `ensure_spawned` removes and
            // replaces the entry atomically when it decides to act.
        }
        for kind in to_respawn {
            tracing::info!(
                target: "sy::aiplane::supervisor",
                kind = %kind,
                "respawning worker"
            );
            if let Err(e) = self.ensure_spawned(kind) {
                tracing::error!(
                    target: "sy::aiplane::supervisor",
                    kind = %kind,
                    error = %format!("{e:#}"),
                    "respawn failed"
                );
            }
        }
    }

    /// Graceful shutdown: send `Req::Shutdown` to every child, wait
    /// briefly for ack, then SIGTERM stragglers.
    pub fn shutdown(&self, deadline: Duration) {
        let kinds: Vec<WorkloadKind> = {
            let inner = self.inner.lock().expect("supervisor poisoned");
            inner.children.keys().copied().collect()
        };
        // SPEC §4.6 `sy_models_warm{kind}`: zero every gauge on
        // shutdown so a restarted daemon's metrics don't carry the
        // previous process's warm set into its first emission
        // window. The warm-pool roster itself doesn't need to clear
        // — the supervisor is going away.
        for k in WorkloadKind::ALL {
            gauge!("sy_models_warm", "kind" => k.as_str()).set(0.0);
        }
        for kind in &kinds {
            let socket = match self.socket_for(*kind) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let _ = worker_ipc::request(&socket, &WorkerReq::Shutdown, HEALTH_PROBE_TIMEOUT);
        }
        let start = Instant::now();
        while start.elapsed() < deadline {
            let any_alive = {
                let inner = self.inner.lock().expect("supervisor poisoned");
                inner.children.values().any(|mc| mc.handle.is_alive())
            };
            if !any_alive {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut inner = self.inner.lock().expect("supervisor poisoned");
        for mc in inner.children.values_mut() {
            if mc.handle.is_alive() {
                tracing::warn!(
                    target: "sy::aiplane::supervisor",
                    pid = ?mc.handle.pid(),
                    "child did not exit, escalating"
                );
                mc.handle.terminate();
            }
        }
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

fn wait_for_socket(path: &std::path::Path, deadline: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!("socket {} not bound within {deadline:?}", path.display())
}

fn default_sy_binary() -> PathBuf {
    // Prefer `/proc/self/exe`: the worker child must be the same
    // binary version as the supervisor (workloads' wire types are
    // not stable across versions). Falls back to "sy" on PATH only
    // if the proc entry is unreadable (shouldn't happen on Linux).
    std::fs::read_link("/proc/self/exe").unwrap_or_else(|_| PathBuf::from("sy"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aiplane::worker_ipc::{serve, write_resp};
    use std::sync::mpsc;
    use std::thread;

    /// In-thread fake worker for supervisor tests. Binds the socket
    /// the supervisor expects, answers Health with a configurable
    /// state, exits when told.
    struct FakeWorker {
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
        pid: u32,
    }

    impl child::Child for FakeWorker {
        fn pid(&self) -> Option<u32> {
            Some(self.pid)
        }
        fn is_alive(&self) -> bool {
            !self.shutdown.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn terminate(&mut self) {
            self.shutdown
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    struct FakeSpawn {
        next_pid: Mutex<u32>,
        /// When `Some(id)`, every spawned worker reports
        /// `inflight_request_id = Some(id)` in its Health responses
        /// — used by the SPEC §4.3 SIGKILL-fallback test to keep the
        /// supervisor convinced the worker is stuck.
        inflight_seed: Option<ulid::Ulid>,
        /// When true, a [`WorkerReq::Cancel`] is ACK'd without
        /// clearing the inflight tracker. Pairs with `inflight_seed`
        /// to simulate "RunOptions::SetTerminate not yielding" per
        /// SPEC §7 Open Q2.
        ignore_cancel: bool,
    }

    impl FakeSpawn {
        fn new() -> Self {
            Self {
                next_pid: Mutex::new(10_000),
                inflight_seed: None,
                ignore_cancel: false,
            }
        }

        /// Build a [`FakeSpawn`] whose workers permanently report the
        /// given `request_id` as inflight and ignore Cancel signals.
        fn with_stuck_inflight(seed: ulid::Ulid) -> Self {
            Self {
                next_pid: Mutex::new(10_000),
                inflight_seed: Some(seed),
                ignore_cancel: true,
            }
        }
    }

    impl ChildSpawn for FakeSpawn {
        fn spawn(
            &self,
            kind: WorkloadKind,
            socket: &std::path::Path,
        ) -> Result<Box<dyn child::Child>> {
            let (req_tx, req_rx) = mpsc::channel::<(WorkerReq, std::os::unix::net::UnixStream)>();
            serve(socket, req_tx)?;
            let state = Arc::new(Mutex::new(WorkloadState::Ready {
                backend: "fake".into(),
            }));
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let inflight = Arc::new(Mutex::new(self.inflight_seed));
            let state_for_thread = state.clone();
            let shutdown_for_thread = shutdown.clone();
            let inflight_for_thread = inflight.clone();
            let ignore_cancel = self.ignore_cancel;
            let kind_copy = kind;
            let thread = thread::spawn(move || {
                while !shutdown_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    match req_rx.recv_timeout(Duration::from_millis(200)) {
                        Ok((req, stream)) => {
                            let resp = match req {
                                WorkerReq::Health => WorkerResp::Health(WorkerHealth {
                                    kind: Some(kind_copy),
                                    state: state_for_thread.lock().unwrap().clone(),
                                    model_stem: "fake".into(),
                                    pid: 1,
                                    ready_at_unix: 1,
                                    ema_ms: 0.0,
                                    calls: 0,
                                    errors: 0,
                                    inflight_request_id: *inflight_for_thread.lock().unwrap(),
                                }),
                                WorkerReq::Shutdown => {
                                    shutdown_for_thread
                                        .store(true, std::sync::atomic::Ordering::SeqCst);
                                    WorkerResp::ShutdownAck
                                }
                                WorkerReq::RunBatch { .. } => WorkerResp::Error {
                                    msg: "fake worker: RunBatch not implemented".into(),
                                },
                                WorkerReq::Cancel { .. } => {
                                    if !ignore_cancel {
                                        *inflight_for_thread.lock().unwrap() = None;
                                    }
                                    WorkerResp::CancelAck
                                }
                            };
                            let _ = write_resp(stream, &resp);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            let mut next = self.next_pid.lock().unwrap();
            let pid = *next;
            *next += 1;
            // `kind` and `state` are deliberately *not* stored on
            // the FakeWorker — the spawned thread already owns its
            // own clones, which keep the underlying Arcs alive for
            // the lifetime of the worker.
            let _ = (kind, state);
            Ok(Box::new(FakeWorker {
                shutdown,
                thread: Some(thread),
                pid,
            }))
        }
    }

    #[test]
    fn supervisor_spawns_waits_for_ready_health_aggregates() {
        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "sy-supervisor-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", &tmp);

        let sup = Supervisor::with_spawn(Arc::new(FakeSpawn::new()));
        let h = sup
            .ensure(WorkloadKind::Embed, Duration::from_secs(3))
            .expect("ensure");
        assert!(h.state.is_ready());
        assert_eq!(h.kind, Some(WorkloadKind::Embed));

        // Idempotent ensure.
        let h2 = sup
            .ensure(WorkloadKind::Embed, Duration::from_secs(3))
            .expect("ensure2");
        assert!(h2.state.is_ready());

        // all_health surfaces it.
        let all = sup.all_health();
        assert!(all.contains_key(&WorkloadKind::Embed));

        // SPEC §4.3 warm pool: a fresh `ensure(Embed)` must register
        // the kind in the supervisor's warm-pool roster alongside
        // the always-warm tier (VAD + EyeTrack).
        let warm = sup.warm_models();
        for required in ["embed", "vad", "eye-track"] {
            assert!(
                warm.iter().any(|m| m == required),
                "expected `{required}` in warm_models: {warm:?}"
            );
        }

        sup.shutdown(Duration::from_secs(2));

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    #[test]
    fn sigkill_after_500ms_no_yield() {
        // SPEC §4.3 step 5 / ROADMAP Step 4: a worker that
        // ACKnowledges Cancel but refuses to clear its inflight
        // tracker (the production analogue: `RunOptions::SetTerminate`
        // fails to unblock — SPEC §7 Open Q2) must be SIGKILLed by
        // the supervisor and respawned within ~500 ms.
        const ESCALATION_FLOOR: Duration = Duration::from_millis(500);
        const ESCALATION_CEILING: Duration = Duration::from_millis(1_500);

        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "sy-supervisor-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", &tmp);

        let seed = ulid::Ulid::new();
        let sup = Supervisor::with_spawn(Arc::new(FakeSpawn::with_stuck_inflight(seed)));
        sup.ensure(WorkloadKind::Embed, Duration::from_secs(3))
            .expect("ensure");
        let pid_before = sup.pid(WorkloadKind::Embed).expect("pid");

        let t0 = Instant::now();
        let cancel_err = sup
            .cancel(WorkloadKind::Embed, seed)
            .expect_err("stuck worker forces SIGKILL fallback");
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= ESCALATION_FLOOR,
            "guard fired before 500 ms: {elapsed:?}"
        );
        assert!(
            elapsed < ESCALATION_CEILING,
            "guard took unreasonably long: {elapsed:?}"
        );
        assert!(
            cancel_err.to_string().contains("did not yield"),
            "error should explain the escalation: {cancel_err}"
        );

        let pid_after = sup.pid(WorkloadKind::Embed).expect("pid after respawn");
        assert_ne!(
            pid_before, pid_after,
            "supervisor must respawn the worker after SIGKILL"
        );

        sup.shutdown(Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    /// arch-observability Step 2: the supervisor's warnings must be
    /// emitted as `tracing::warn!` events with structured fields, not
    /// as `eprintln!("…")` lines. The cancel-guard escalation path
    /// (analogous to the SPEC §4.3 NPU-EAGAIN "worker stuck" branch)
    /// must carry the workload `kind` as a typed field so journald /
    /// the OTel formatter can index on it.
    #[test]
    #[tracing_test::traced_test]
    fn warn_on_npu_eagain_emits_structured_field() {
        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "sy-supervisor-warn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", &tmp);

        let seed = ulid::Ulid::new();
        let sup = Supervisor::with_spawn(Arc::new(FakeSpawn::with_stuck_inflight(seed)));
        sup.ensure(WorkloadKind::Embed, Duration::from_secs(3))
            .expect("ensure");
        let _ = sup
            .cancel(WorkloadKind::Embed, seed)
            .expect_err("stuck worker forces SIGKILL fallback");

        // The cancel-guard expired warning must (a) be a tracing event
        // — not a raw stderr write — and (b) carry the workload kind
        // as a structured field, not just interpolated into the body.
        // `tracing-test`'s capture uses the `compact` formatter, which
        // renders Display-valued fields as `kind=embed` (no quotes).
        assert!(
            logs_contain("kind=embed"),
            "expected `kind=embed` structured field in captured tracing logs"
        );
        assert!(
            logs_contain("cancel guard expired"),
            "expected the cancel-guard warning body in captured logs"
        );
        assert!(
            logs_contain("sy::aiplane::supervisor"),
            "expected the supervisor tracing target in captured logs"
        );

        sup.shutdown(Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }
}
