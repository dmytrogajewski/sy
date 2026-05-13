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

use super::registry::{WorkloadInput, WorkloadKind, WorkloadOutput, WorkloadState};
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

pub struct Supervisor {
    spawn: Arc<dyn ChildSpawn>,
    inner: Arc<Mutex<Inner>>,
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
    last_spawn_at: Instant,
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
        }
    }

    /// Spawn the child for `kind` (idempotent) and block until either
    /// it reports `Ready` or the deadline expires. Returns the
    /// worker's `WorkerHealth` so the daemon's status writer can use
    /// it immediately.
    pub fn ensure(&self, kind: WorkloadKind, ready_deadline: Duration) -> Result<WorkerHealth> {
        self.ensure_spawned(kind)?;
        self.wait_for_ready(kind, ready_deadline)
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
                eprintln!(
                    "sy aiplane[supervisor]: child {kind} died (pid={:?}); respawning",
                    mc.handle.pid()
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
                last_spawn_at: Instant::now(),
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
        match worker_ipc::request(&socket, &WorkerReq::RunBatch { inputs }, RUN_BATCH_TIMEOUT) {
            Ok(WorkerResp::RunBatch { outputs }) => Ok(outputs),
            Ok(WorkerResp::Error { msg }) => anyhow::bail!("worker {kind}: {msg}"),
            Ok(other) => anyhow::bail!("worker {kind}: unexpected resp {other:?}"),
            Err(e) => Err(anyhow::anyhow!("worker {kind}: {e}")),
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
            eprintln!("sy aiplane[supervisor]: respawning {kind} worker");
            if let Err(e) = self.ensure_spawned(kind) {
                eprintln!("sy aiplane[supervisor]: respawn {kind} failed: {e:#}");
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
                eprintln!(
                    "sy aiplane[supervisor]: child pid={:?} did not exit, escalating",
                    mc.handle.pid()
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
        kind: WorkloadKind,
        state: Arc<Mutex<WorkloadState>>,
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
        fn handle_kind(&self) -> Option<WorkloadKind> {
            Some(self.kind)
        }
    }

    struct FakeSpawn {
        next_pid: Mutex<u32>,
    }

    impl FakeSpawn {
        fn new() -> Self {
            Self {
                next_pid: Mutex::new(10_000),
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
            let state_for_thread = state.clone();
            let shutdown_for_thread = shutdown.clone();
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
                                }),
                                WorkerReq::Shutdown => {
                                    shutdown_for_thread
                                        .store(true, std::sync::atomic::Ordering::SeqCst);
                                    WorkerResp::ShutdownAck
                                }
                                WorkerReq::RunBatch { .. } => WorkerResp::Error {
                                    msg: "fake worker: RunBatch not implemented".into(),
                                },
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
            Ok(Box::new(FakeWorker {
                kind,
                state,
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
        assert!(all.get(&WorkloadKind::Embed).is_some());

        sup.shutdown(Duration::from_secs(2));

        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }
}
