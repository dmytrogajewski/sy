//! Generic worker loop. One process, one `Workload`, one socket.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;

#[cfg(test)]
use super::super::registry::WorkloadOutput;
use super::super::registry::{Workload, WorkloadInput, WorkloadKind, WorkloadState};
use super::super::session::SessionPool;
use super::super::worker_ipc::{self, write_resp, WorkerHealth, WorkerReq, WorkerResp};
use super::super::workloads;

const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Worker entry point. Called from `sy aiplane worker --kind <K>
/// --socket <path>` after the AMD-venv re-exec has fired (the
/// supervisor passes the same env down to children).
///
/// Returns `Ok(())` on a clean Shutdown or SIGTERM; `Err(_)` only
/// for unrecoverable startup failures (socket bind, unsupported
/// workload kind). The Workload's `load()` failing is *not* a
/// process-level error — it's reflected in `WorkerState::Failed` and
/// the supervisor decides whether to restart.
pub fn run(kind: WorkloadKind, socket: PathBuf) -> Result<()> {
    // SPEC §4.6 / arch-observability Step 1: each `sy aiplane worker`
    // subprocess is a daemon-shaped process (one per workload kind),
    // so it gets the journald + rolling-JSONL + stderr stack. The
    // guard is held for the worker's full lifetime; dropping it
    // could lose buffered log lines on a fast SIGTERM.
    let _obs_guard = sy_core::obs::init(sy_core::obs::Mode::Daemon { name: "sy-aiplane" })?;
    let workload = build_workload(kind)?;

    let state = Arc::new(Mutex::new(InternalState {
        kind,
        model_stem: workload.model_stem().to_string(),
        pid: std::process::id(),
        workload_state: WorkloadState::NotPrepared,
        ready_at_unix: 0,
        ema_ms: 0.0,
        calls: 0,
        errors: 0,
    }));

    tracing::info!(
        target: "sy::aiplane::worker",
        kind = %kind,
        pid = std::process::id(),
        socket = %socket.display(),
        "worker starting"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    install_signal_handlers(shutdown.clone());

    spawn_loader(workload.clone(), state.clone());

    let (req_tx, req_rx) = mpsc::channel::<(WorkerReq, std::os::unix::net::UnixStream)>();
    worker_ipc::serve(&socket, req_tx)?;

    let result = serve_loop(
        kind,
        workload.clone(),
        state.clone(),
        shutdown.clone(),
        req_rx,
    );

    tracing::info!(
        target: "sy::aiplane::worker",
        kind = %kind,
        result = ?result,
        "worker shutting down"
    );
    workload.unload();
    let _ = std::fs::remove_file(&socket);
    result
}

struct InternalState {
    kind: WorkloadKind,
    model_stem: String,
    pid: u32,
    workload_state: WorkloadState,
    ready_at_unix: u64,
    ema_ms: f64,
    calls: u64,
    errors: u64,
}

impl InternalState {
    fn to_health(&self, inflight_request_id: Option<ulid::Ulid>) -> WorkerHealth {
        WorkerHealth {
            kind: Some(self.kind),
            state: self.workload_state.clone(),
            model_stem: self.model_stem.clone(),
            pid: self.pid,
            ready_at_unix: self.ready_at_unix,
            ema_ms: self.ema_ms,
            calls: self.calls,
            errors: self.errors,
            inflight_request_id,
        }
    }
}

/// Instantiate the concrete `Workload` for a given kind. Returns
/// `Err(_)` for kinds whose worker isn't implemented yet — that's a
/// supervisor-config bug, not a runtime error.
fn build_workload(kind: WorkloadKind) -> Result<Arc<dyn Workload>> {
    match kind {
        WorkloadKind::Embed => Ok(Arc::new(workloads::embed::EmbedWorkload::new())),
        WorkloadKind::Rerank => Ok(Arc::new(workloads::rerank::RerankWorkload::new())),
        WorkloadKind::Vad => Ok(Arc::new(workloads::vad::VadWorkload::new())),
        WorkloadKind::Stt => Ok(Arc::new(workloads::stt::SttWorkload::new())),
        WorkloadKind::Ocr => Ok(Arc::new(workloads::ocr::OcrWorkload::new())),
        WorkloadKind::Tts | WorkloadKind::Clip | WorkloadKind::Denoise | WorkloadKind::EyeTrack => {
            anyhow::bail!("worker: {kind} not implemented yet — supervisor should not spawn it")
        }
    }
}

/// Background load thread. Transitions the worker through
/// NotPrepared → Loading → Ready / Failed. Held outside the serve
/// loop so the worker can answer Health probes while the model is
/// compiling its VAIP cache (3–10 min cold).
fn spawn_loader(workload: Arc<dyn Workload>, state: Arc<Mutex<InternalState>>) {
    thread::spawn(move || {
        {
            let mut s = state.lock().expect("worker state poisoned");
            s.workload_state = WorkloadState::Loading;
        }
        // SessionPool here is per-worker (own process, own NPU mutex)
        // — workloads don't share inside a worker so the lock is
        // mostly a no-op, but keeps the trait surface clean.
        let pool = SessionPool::new();
        match workload.load(&pool) {
            Ok(()) => {
                let backend = workload.health().backend;
                let mut s = state.lock().expect("worker state poisoned");
                s.workload_state = WorkloadState::Ready {
                    backend: if backend.is_empty() {
                        "unknown".to_string()
                    } else {
                        backend
                    },
                };
                s.ready_at_unix = unix_now();
                tracing::info!(
                    target: "sy::aiplane::worker",
                    kind = %s.kind,
                    backend = %workload.health().backend,
                    "worker ready"
                );
            }
            Err(e) => {
                let reason = format!("{e:#}");
                let mut s = state.lock().expect("worker state poisoned");
                s.workload_state = WorkloadState::Failed {
                    reason: reason.clone(),
                };
                tracing::error!(
                    target: "sy::aiplane::worker",
                    kind = %s.kind,
                    reason = %reason,
                    "worker load failed"
                );
            }
        }
    });
}

fn serve_loop(
    kind: WorkloadKind,
    workload: Arc<dyn Workload>,
    state: Arc<Mutex<InternalState>>,
    shutdown: Arc<AtomicBool>,
    req_rx: mpsc::Receiver<(WorkerReq, std::os::unix::net::UnixStream)>,
) -> Result<()> {
    // SPEC §4.3 inflight tracker: holds the Ulid of the RunBatch the
    // worker thread is currently processing (if any). A subsequent
    // `WorkerReq::Cancel` whose `request_id` matches calls
    // `Workload::try_cancel`; mismatches still ACK because cancel is
    // idempotent (the caller learns the actual outcome from the
    // RunBatch reply).
    let inflight: Arc<Mutex<Option<ulid::Ulid>>> = Arc::new(Mutex::new(None));
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        match req_rx.recv_timeout(HEALTH_POLL_INTERVAL) {
            Ok((req, stream)) => match req {
                WorkerReq::RunBatch { request_id, inputs } => {
                    spawn_run_batch(
                        kind,
                        workload.clone(),
                        state.clone(),
                        inflight.clone(),
                        request_id,
                        inputs,
                        stream,
                    );
                }
                WorkerReq::Cancel { request_id } => {
                    if inflight.lock().expect("inflight poisoned").as_ref() == Some(&request_id) {
                        let _ = workload.try_cancel();
                    }
                    let _ = write_resp(stream, &WorkerResp::CancelAck);
                }
                other => {
                    let exit_after = matches!(other, WorkerReq::Shutdown);
                    let resp = dispatch(other, &state, &inflight);
                    let _ = write_resp(stream, &resp);
                    if exit_after {
                        tracing::info!(
                            target: "sy::aiplane::worker",
                            kind = %kind,
                            "shutdown requested"
                        );
                        shutdown.store(true, Ordering::SeqCst);
                    }
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Spawn a one-shot thread that runs the inference, writes the
/// response, and clears the inflight tracker. The thread takes
/// ownership of the connection stream so the serve loop can keep
/// pulling new requests (notably `WorkerReq::Cancel`) while the
/// inference is in flight.
fn spawn_run_batch(
    kind: WorkloadKind,
    workload: Arc<dyn Workload>,
    state: Arc<Mutex<InternalState>>,
    inflight: Arc<Mutex<Option<ulid::Ulid>>>,
    request_id: ulid::Ulid,
    inputs: Vec<WorkloadInput>,
    stream: std::os::unix::net::UnixStream,
) {
    thread::Builder::new()
        .name(format!("sy-aiplane-worker:{kind}:run"))
        .spawn(move || {
            *inflight.lock().expect("inflight poisoned") = Some(request_id);
            let resp = run_batch(&workload, &state, inputs);
            *inflight.lock().expect("inflight poisoned") = None;
            let _ = write_resp(stream, &resp);
        })
        .expect("spawn run_batch thread");
}

/// Handle the cheap synchronous variants (Health / Shutdown).
/// RunBatch and Cancel never reach here — `serve_loop` special-cases
/// them so the inflight tracker can correlate the two across the
/// dispatch-on-thread boundary.
fn dispatch(
    req: WorkerReq,
    state: &Arc<Mutex<InternalState>>,
    inflight: &Arc<Mutex<Option<ulid::Ulid>>>,
) -> WorkerResp {
    match req {
        WorkerReq::Health => {
            let s = state.lock().expect("worker state poisoned");
            let inflight_id = *inflight.lock().expect("inflight poisoned");
            WorkerResp::Health(s.to_health(inflight_id))
        }
        WorkerReq::Shutdown => WorkerResp::ShutdownAck,
        WorkerReq::RunBatch { .. } | WorkerReq::Cancel { .. } => WorkerResp::Error {
            msg: "internal: RunBatch/Cancel must route through serve_loop".into(),
        },
    }
}

fn run_batch(
    workload: &Arc<dyn Workload>,
    state: &Arc<Mutex<InternalState>>,
    inputs: Vec<WorkloadInput>,
) -> WorkerResp {
    // Gate on Ready: a Loading/Failed/NotPrepared worker must not
    // silently fall through to `Workload::run` (which would panic or
    // bail with a stale internal error). The supervisor reads the
    // error and decides whether to restart or surface "not ready".
    let phase = {
        let s = state.lock().expect("worker state poisoned");
        s.workload_state.clone()
    };
    match &phase {
        WorkloadState::Ready { .. } => {}
        WorkloadState::Loading => {
            return WorkerResp::Error {
                msg: "worker still loading".into(),
            };
        }
        WorkloadState::Failed { reason } => {
            return WorkerResp::Error {
                msg: format!("worker load failed: {reason}"),
            };
        }
        WorkloadState::NotPrepared => {
            return WorkerResp::Error {
                msg: "worker not prepared (model file missing)".into(),
            };
        }
        WorkloadState::Unavailable => {
            return WorkerResp::Error {
                msg: "worker unavailable".into(),
            };
        }
    }

    let t0 = Instant::now();
    let n = inputs.len();
    let result = workload.run_batch(inputs);
    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let mut s = state.lock().expect("worker state poisoned");
    match result {
        Ok(outputs) => {
            if outputs.len() != n {
                s.errors += 1;
                return WorkerResp::Error {
                    msg: format!(
                        "run_batch returned {} outputs for {n} inputs",
                        outputs.len()
                    ),
                };
            }
            s.calls += 1;
            s.ema_ms = if s.ema_ms == 0.0 {
                elapsed_ms
            } else {
                0.2 * elapsed_ms + 0.8 * s.ema_ms
            };
            WorkerResp::RunBatch { outputs }
        }
        Err(e) => {
            s.errors += 1;
            WorkerResp::Error {
                msg: format!("{e:#}"),
            }
        }
    }
}

fn install_signal_handlers(shutdown: Arc<AtomicBool>) {
    use std::os::raw::c_int;
    static SIGNAL: AtomicBool = AtomicBool::new(false);
    extern "C" fn handler(_: c_int) {
        SIGNAL.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGTERM, handler as *const () as usize);
        libc::signal(libc::SIGINT, handler as *const () as usize);
    }
    thread::spawn(move || loop {
        if SIGNAL.load(Ordering::SeqCst) {
            shutdown.store(true, Ordering::SeqCst);
            return;
        }
        thread::sleep(Duration::from_millis(100));
    });
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aiplane::worker_ipc::{request, socket_path};

    /// Spin up a real worker (in-thread, with the FakeWorkload acting
    /// as Embed) and verify the full lifecycle:
    ///   - Health goes from NotPrepared → Loading → Ready quickly.
    ///   - RunBatch returns one Vector per input.
    ///   - Shutdown ends the loop and returns ack.
    #[test]
    fn worker_lifecycle_via_fake_workload() {
        use crate::aiplane::workloads::fake::FakeWorkload;
        use std::env;
        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Hermetic XDG_RUNTIME_DIR so the worker doesn't collide with
        // a live daemon's socket.
        let unique = format!(
            "sy-worker-runner-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let tmp = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);
        let socket = socket_path(WorkloadKind::Embed);

        // Drop a worker in a thread. Stand in a FakeWorkload directly
        // — the real `run()` calls `build_workload(Embed)` which would
        // try to load multilingual-e5-base. We replicate `run`'s
        // structure by hand here so we can substitute the Fake.
        let workload: Arc<dyn Workload> = Arc::new(FakeWorkload::embed());
        let state = Arc::new(Mutex::new(InternalState {
            kind: WorkloadKind::Embed,
            model_stem: "fake".into(),
            pid: std::process::id(),
            workload_state: WorkloadState::NotPrepared,
            ready_at_unix: 0,
            ema_ms: 0.0,
            calls: 0,
            errors: 0,
        }));
        spawn_loader(workload.clone(), state.clone());
        let shutdown = Arc::new(AtomicBool::new(false));
        let (req_tx, req_rx) = mpsc::channel::<(WorkerReq, std::os::unix::net::UnixStream)>();
        worker_ipc::serve(&socket, req_tx).expect("serve");
        let workload_for_loop = workload.clone();
        let state_for_loop = state.clone();
        let shutdown_for_loop = shutdown.clone();
        let join = thread::spawn(move || {
            let _ = serve_loop(
                WorkloadKind::Embed,
                workload_for_loop,
                state_for_loop,
                shutdown_for_loop,
                req_rx,
            );
        });

        // Poll Health until Ready (FakeWorkload's load is sub-ms).
        let mut got_ready = false;
        for _ in 0..30 {
            match request(&socket, &WorkerReq::Health, Duration::from_secs(2)) {
                Ok(WorkerResp::Health(h)) if h.state.is_ready() => {
                    got_ready = true;
                    break;
                }
                _ => {}
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(got_ready, "worker never reached Ready");

        // RunBatch with two embeds.
        let resp = request(
            &socket,
            &WorkerReq::RunBatch {
                request_id: ulid::Ulid::nil(),
                inputs: vec![
                    WorkloadInput::Text { text: "hi".into() },
                    WorkloadInput::Text {
                        text: "there".into(),
                    },
                ],
            },
            Duration::from_secs(5),
        )
        .expect("run_batch");
        match resp {
            WorkerResp::RunBatch { outputs } => {
                assert_eq!(outputs.len(), 2);
                for o in &outputs {
                    match o {
                        WorkloadOutput::Vector { vector } => {
                            assert!(!vector.is_empty());
                        }
                        _ => panic!("expected Vector"),
                    }
                }
            }
            other => panic!("expected RunBatch, got {other:?}"),
        }

        // Stats: ema_ms should be set, calls = 1, errors = 0.
        let h = match request(&socket, &WorkerReq::Health, Duration::from_secs(2)).expect("health2")
        {
            WorkerResp::Health(h) => h,
            _ => panic!("Health"),
        };
        assert_eq!(h.calls, 1);
        assert_eq!(h.errors, 0);
        assert!(h.ema_ms >= 0.0);

        // Shutdown.
        let resp =
            request(&socket, &WorkerReq::Shutdown, Duration::from_secs(2)).expect("shutdown");
        assert!(matches!(resp, WorkerResp::ShutdownAck));
        join.join().expect("worker thread");

        // Cleanup.
        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    #[test]
    fn set_terminate_unblocks_in_flight() {
        // SPEC §4.3 "Cancellation pattern" / ROADMAP Step 4: a
        // long-running RunBatch returns within 1 s after the
        // supervisor sends `WorkerReq::Cancel` with the matching
        // request_id — the serve loop's inflight tracker dispatches
        // the RunBatch on a worker thread, observes the Cancel, calls
        // `Workload::try_cancel`, and the FakeWorkload's sleep loop
        // surfaces the cancel as `WorkerResp::Error { msg: "..cancelled.." }`.
        use crate::aiplane::workloads::fake::FakeWorkload;
        use std::env;
        const SLEEP_MS: u64 = 5_000;
        const CANCEL_AFTER_MS: u64 = 100;
        const DEADLINE_MS: u64 = 1_500;

        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!(
            "sy-worker-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = env::var("XDG_RUNTIME_DIR").ok();
        env::set_var("XDG_RUNTIME_DIR", &tmp);
        let socket = socket_path(WorkloadKind::Embed);

        let workload: Arc<dyn Workload> =
            Arc::new(FakeWorkload::with_sleep_ms(WorkloadKind::Embed, SLEEP_MS));
        let state = Arc::new(Mutex::new(InternalState {
            kind: WorkloadKind::Embed,
            model_stem: "fake".into(),
            pid: std::process::id(),
            workload_state: WorkloadState::Ready {
                backend: "fake".into(),
            },
            ready_at_unix: 0,
            ema_ms: 0.0,
            calls: 0,
            errors: 0,
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (req_tx, req_rx) = mpsc::channel::<(WorkerReq, std::os::unix::net::UnixStream)>();
        worker_ipc::serve(&socket, req_tx).expect("serve");
        let serve_join = thread::spawn({
            let workload = workload.clone();
            let state = state.clone();
            let shutdown = shutdown.clone();
            move || {
                let _ = serve_loop(WorkloadKind::Embed, workload, state, shutdown, req_rx);
            }
        });

        let request_id = ulid::Ulid::new();
        let cancel_socket = socket.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(CANCEL_AFTER_MS));
            request(
                &cancel_socket,
                &WorkerReq::Cancel { request_id },
                Duration::from_secs(2),
            )
            .expect("cancel request")
        });

        let t0 = Instant::now();
        let resp = request(
            &socket,
            &WorkerReq::RunBatch {
                request_id,
                inputs: vec![WorkloadInput::Text { text: "hi".into() }],
            },
            Duration::from_secs(10),
        )
        .expect("run_batch");
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(DEADLINE_MS),
            "cancel must beat the {DEADLINE_MS}ms deadline; took {elapsed:?}"
        );
        match resp {
            WorkerResp::Error { msg } => {
                assert!(
                    msg.contains("cancelled"),
                    "response should advertise the cancel: {msg:?}"
                );
            }
            other => panic!("expected Error(cancelled), got {other:?}"),
        }
        let cancel_resp = canceller.join().expect("canceller joins");
        assert!(matches!(cancel_resp, WorkerResp::CancelAck));

        shutdown.store(true, Ordering::SeqCst);
        serve_join.join().expect("serve loop joins");
        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(v) = prev {
            env::set_var("XDG_RUNTIME_DIR", v);
        } else {
            env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    #[test]
    fn run_batch_rejects_when_not_ready() {
        let state = Arc::new(Mutex::new(InternalState {
            kind: WorkloadKind::Rerank,
            model_stem: "x".into(),
            pid: 1,
            workload_state: WorkloadState::Loading,
            ready_at_unix: 0,
            ema_ms: 0.0,
            calls: 0,
            errors: 0,
        }));
        use crate::aiplane::workloads::fake::FakeWorkload;
        let workload: Arc<dyn Workload> = Arc::new(FakeWorkload::new(WorkloadKind::Rerank));
        let resp = run_batch(&workload, &state, vec![]);
        match resp {
            WorkerResp::Error { msg } => assert!(msg.contains("loading")),
            _ => panic!("expected Error"),
        }
    }
}
