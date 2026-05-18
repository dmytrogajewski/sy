//! Four-class strict-priority scheduler (SPEC §4.3 / ROADMAP
//! arch-aiplane-scheduler Step 2).
//!
//! ## Wire shape
//!
//! Callers submit a [`Request`] tagged with one of the four
//! [`sy_core::Priority`] classes via [`Scheduler::admit`]. The
//! dispatcher pulls in strict priority order
//! `Realtime > Interactive > Background > Batch` (no aging, no
//! starvation guards — SPEC §4.3 rationale: Realtime workloads
//! refuse queueing depth, so the only fairness concern is that an
//! infinite Background stream doesn't permanently starve
//! Interactive; the dispatcher's `select_biased!`-style polling
//! guarantees Interactive sees the CPU between any two Background
//! runs).
//!
//! ## Admission policy
//!
//! Each class carries a [`ModelQueuePolicy`] mapping cap-overrun to
//! one of [`TimeoutAction::Reject`] (caller gets `Overloaded` with a
//! `retry_after_ms` hint) or [`TimeoutAction::Delay`] (the
//! `Background`/`Batch` path; admission blocks until the queue
//! drains, on the assumption these callers prefer eventual
//! completion over backpressure feedback).
//!
//! ## Cancellation hooks
//!
//! Each request carries a [`tokio_util::sync::CancellationToken`].
//! The dispatcher checks it before consuming a request — a request
//! whose token tripped while it sat in the queue surfaces as
//! [`AiplaneError::Cancelled`]. Real `RunOptions::SetTerminate`-
//! driven mid-flight cancellation lands in Step 4.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crossbeam_channel::{bounded, select, Receiver, Sender, TrySendError};
use metrics::{counter, gauge, histogram};
use sy_core::Priority;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::error::AiplaneError;
use super::ipc::AiplaneDispatch;
use super::registry::{WorkloadInput, WorkloadKind, WorkloadOutput};

/// Per-class queue caps from SPEC §4.3 "ModelQueuePolicy" table. The
/// dispatcher rejects (or delays) further submissions once a class's
/// queue length hits these values — the constants are the wire-
/// stable admission contract that callers can plan around.
pub const CAP_REALTIME: usize = 4;
pub const CAP_INTERACTIVE: usize = 32;
pub const CAP_BACKGROUND: usize = 256;
pub const CAP_BATCH: usize = 4096;

/// SPEC §4.3 retry hint surfaced inside [`AiplaneError::Overloaded`].
/// Chosen to land just past the worst-case ORT `Embed` latency on a
/// warm cache (P99 ~150 ms on Phoenix) so a back-off-then-retry loop
/// has a good chance of finding the queue drained.
pub const OVERLOADED_RETRY_AFTER_MS: u64 = 200;

/// SPEC §4.3 cross-class hard-escape threshold. If a higher-priority
/// queue is non-empty AND the current inflight is a lower-priority
/// request that has been running this long, the watchdog fires
/// `AiplaneDispatch::cancel` to preempt it. The value is tight
/// enough to keep Interactive responsiveness inside a frame budget
/// (~16 ms at 60 Hz × ~12) but loose enough that the inflight has a
/// reasonable chance to finish on its own first.
pub const HARD_ESCAPE_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(200);

/// SPEC §4.3 watchdog tick. The dispatcher spawns a sibling thread
/// that polls inflight + queue depths at this cadence to decide
/// whether to fire a cross-class hard escape. Fine enough that the
/// preemption budget stays under [`HARD_ESCAPE_THRESHOLD`] +
/// [`HARD_ESCAPE_TICK`] in the worst case.
pub const HARD_ESCAPE_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// Per-class queue capacity. Surfaced via `Status.queue_caps` so
/// `sy aiplane status` reports the admission budget operators can
/// plan against, and so the dispatcher and CLI surfaces
/// (`--priority`) consult one source of truth.
pub fn queue_cap(class: Priority) -> usize {
    match class {
        Priority::Realtime => CAP_REALTIME,
        Priority::Interactive => CAP_INTERACTIVE,
        Priority::Background => CAP_BACKGROUND,
        Priority::Batch => CAP_BATCH,
    }
}

/// Triton-style admission policy per class (SPEC §4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelQueuePolicy {
    pub timeout_action: TimeoutAction,
}

/// What the scheduler does when [`Scheduler::admit`] hits the cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutAction {
    /// Return immediately with [`AiplaneError::Overloaded`]. The CLI
    /// / MCP path uses this for Realtime + Interactive — caller is
    /// expected to back off and retry.
    Reject,
    /// Block until the queue has space. The daemon's own background
    /// passes (`Background`/`Batch`) prefer eventual completion over
    /// backpressure feedback to a non-existent UI caller.
    Delay,
}

/// SPEC §4.3 policy for the named class.
pub fn policy(class: Priority) -> ModelQueuePolicy {
    match class {
        Priority::Realtime | Priority::Interactive => ModelQueuePolicy {
            timeout_action: TimeoutAction::Reject,
        },
        Priority::Background | Priority::Batch => ModelQueuePolicy {
            timeout_action: TimeoutAction::Delay,
        },
    }
}

/// Scheduler request envelope. Built by the IPC bridge from a v1
/// `Request`; the response travels back on `respond`. Field order
/// matches SPEC §4.3.
pub struct Request {
    pub id: Ulid,
    pub workload: WorkloadKind,
    pub input: WorkloadInput,
    pub class: Priority,
    pub queued_at: Instant,
    pub deadline: Option<Instant>,
    pub cancel: CancellationToken,
    pub respond: oneshot::Sender<Result<WorkloadOutput, AiplaneError>>,
}

impl Request {
    pub fn new(
        id: Ulid,
        workload: WorkloadKind,
        input: WorkloadInput,
        class: Priority,
        deadline: Option<Instant>,
        cancel: CancellationToken,
    ) -> (
        Self,
        oneshot::Receiver<Result<WorkloadOutput, AiplaneError>>,
    ) {
        let (respond, rx) = oneshot::channel();
        let req = Self {
            id,
            workload,
            input,
            class,
            queued_at: Instant::now(),
            deadline,
            cancel,
            respond,
        };
        (req, rx)
    }
}

/// Caller-side handle to the four bounded queues. Holds *only* the
/// sender halves so the dispatcher thread can exit cleanly when every
/// caller drops its handle — if the dispatcher owned a sender clone,
/// the receivers would never see `Disconnected` and the thread would
/// park forever in `select!`.
pub struct Scheduler {
    realtime_tx: Sender<Request>,
    interactive_tx: Sender<Request>,
    background_tx: Sender<Request>,
    batch_tx: Sender<Request>,
}

/// Dispatcher-side state: receiver halves only. Constructed alongside
/// [`Scheduler`] in [`Scheduler::new`]; passed by value into
/// [`Dispatcher::run`] so the spawned thread owns it exclusively.
pub struct Dispatcher {
    realtime_rx: Receiver<Request>,
    interactive_rx: Receiver<Request>,
    background_rx: Receiver<Request>,
    batch_rx: Receiver<Request>,
    /// Shared with the cross-class hard-escape watchdog (Step 8). The
    /// dispatcher sets this on entry to [`Dispatcher::run_one`] and
    /// clears it on exit so the watchdog can read "what's running
    /// right now, and how long has it been running" without coupling
    /// to the dispatcher thread.
    inflight: Arc<Mutex<Option<InflightInfo>>>,
}

/// Snapshot of the dispatcher's currently-executing request.
/// Surfaced through [`Scheduler::inflight_snapshot`] for the
/// cross-class hard-escape watchdog (SPEC §4.3 / arch-aiplane-scheduler
/// Step 8) and any future status surface that wants to attribute
/// inflight latency.
#[derive(Clone, Debug)]
pub struct InflightInfo {
    pub request_id: Ulid,
    pub workload: WorkloadKind,
    pub class: Priority,
    pub started_at: Instant,
}

impl Scheduler {
    /// Build the caller-side `Scheduler` and the dispatcher-side
    /// `Dispatcher` as a paired set. The bridge holds the
    /// `Scheduler`; the supervisor calls `Dispatcher::run` on a
    /// dedicated thread.
    pub fn new() -> (Self, Dispatcher) {
        let (realtime_tx, realtime_rx) = bounded(CAP_REALTIME);
        let (interactive_tx, interactive_rx) = bounded(CAP_INTERACTIVE);
        let (background_tx, background_rx) = bounded(CAP_BACKGROUND);
        let (batch_tx, batch_rx) = bounded(CAP_BATCH);
        let inflight: Arc<Mutex<Option<InflightInfo>>> = Arc::new(Mutex::new(None));
        let scheduler = Self {
            realtime_tx,
            interactive_tx,
            background_tx,
            batch_tx,
        };
        let dispatcher = Dispatcher {
            realtime_rx,
            interactive_rx,
            background_rx,
            batch_rx,
            inflight,
        };
        (scheduler, dispatcher)
    }

    /// Submit a request. Returns `Ok(())` once the request is in the
    /// queue; on a full queue, [`Reject`](TimeoutAction::Reject)
    /// policies surface [`AiplaneError::Overloaded`] immediately, and
    /// [`Delay`](TimeoutAction::Delay) policies block the caller
    /// until space frees.
    pub fn admit(&self, req: Request) -> Result<(), AiplaneError> {
        let class = req.class;
        let tx = self.sender_for(class);
        match policy(class).timeout_action {
            TimeoutAction::Reject => match tx.try_send(req) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => Err(AiplaneError::Overloaded {
                    class,
                    queue_depth: queue_cap(class),
                    retry_after_ms: OVERLOADED_RETRY_AFTER_MS,
                }),
                Err(TrySendError::Disconnected(_)) => Err(AiplaneError::WorkloadFailed(
                    anyhow::anyhow!("scheduler dispatcher gone — no receiver for {class:?}"),
                )),
            },
            TimeoutAction::Delay => tx.send(req).map_err(|_| {
                AiplaneError::WorkloadFailed(anyhow::anyhow!(
                    "scheduler dispatcher gone — no receiver for {class:?}"
                ))
            }),
        }
    }

    fn sender_for(&self, class: Priority) -> &Sender<Request> {
        match class {
            Priority::Realtime => &self.realtime_tx,
            Priority::Interactive => &self.interactive_tx,
            Priority::Background => &self.background_tx,
            Priority::Batch => &self.batch_tx,
        }
    }

    /// Snapshot the current number of admitted-but-not-yet-dispatched
    /// requests in each priority class. Surfaced via
    /// `Status.queue_depths` so `sy aiplane status --json` can report
    /// scheduler pressure operators can act on. The values are point-
    /// in-time reads (`crossbeam_channel::Sender::len`); the
    /// dispatcher may drain a queue before the caller observes the
    /// snapshot.
    pub fn queue_depths(&self) -> std::collections::HashMap<Priority, usize> {
        let mut m = std::collections::HashMap::with_capacity(Priority::ALL.len());
        m.insert(Priority::Realtime, self.realtime_tx.len());
        m.insert(Priority::Interactive, self.interactive_tx.len());
        m.insert(Priority::Background, self.background_tx.len());
        m.insert(Priority::Batch, self.batch_tx.len());
        m
    }
}

impl Dispatcher {
    /// Spawn the strict-priority dispatcher thread plus the
    /// cross-class hard-escape watchdog. The returned handle is the
    /// dispatcher thread's join handle (the bridge holds it for the
    /// daemon's lifetime). The watchdog thread is detached — it
    /// observes the dispatcher's inflight slot and the queue depths,
    /// and exits when every queue's receiver disconnects (which
    /// happens when the dispatcher returns).
    pub fn run(self, aiplane: Arc<dyn AiplaneDispatch>) -> thread::JoinHandle<()> {
        // Clone the bits the watchdog needs *before* moving `self`
        // into the dispatcher thread.
        let inflight = Arc::clone(&self.inflight);
        let realtime_rx = self.realtime_rx.clone();
        let interactive_rx = self.interactive_rx.clone();
        let background_rx = self.background_rx.clone();
        let aiplane_for_watchdog = Arc::clone(&aiplane);
        thread::Builder::new()
            .name("sy-aiplane-escape".into())
            .spawn(move || {
                hard_escape_loop(
                    inflight,
                    realtime_rx,
                    interactive_rx,
                    background_rx,
                    aiplane_for_watchdog,
                );
            })
            .expect("spawn hard-escape watchdog");
        thread::Builder::new()
            .name("sy-aiplane-scheduler".into())
            .spawn(move || self.run_dispatcher(aiplane))
            .expect("spawn scheduler dispatcher")
    }

    fn run_dispatcher(&self, aiplane: Arc<dyn AiplaneDispatch>) {
        loop {
            // Strict-priority drain: poll Realtime first, then walk
            // down. A trickle of Background/Batch can never starve
            // Realtime because we always check Realtime before
            // blocking on a `select!` over all four.
            if let Ok(req) = self.realtime_rx.try_recv() {
                self.run_one(&aiplane, req);
                continue;
            }
            if let Ok(req) = self.interactive_rx.try_recv() {
                self.run_one(&aiplane, req);
                continue;
            }
            if let Ok(req) = self.background_rx.try_recv() {
                self.run_one(&aiplane, req);
                continue;
            }
            if let Ok(req) = self.batch_rx.try_recv() {
                self.run_one(&aiplane, req);
                continue;
            }
            // Nothing ready — park on a `select!` that wakes as soon
            // as anything lands in any class.
            let realtime = &self.realtime_rx;
            let interactive = &self.interactive_rx;
            let background = &self.background_rx;
            let batch = &self.batch_rx;
            let pulled: Option<Request> = select! {
                recv(realtime) -> r => r.ok(),
                recv(interactive) -> r => r.ok(),
                recv(background) -> r => r.ok(),
                recv(batch) -> r => r.ok(),
            };
            match pulled {
                Some(req) => self.run_one(&aiplane, req),
                None => {
                    // All four senders disconnected — the bridge
                    // dropped the Scheduler. Exit the loop so the
                    // thread joins cleanly.
                    return;
                }
            }
        }
    }

    /// Emit `sy_queue_depth{class, kind}` for every priority class
    /// using the current `crossbeam_channel::Receiver::len()` as the
    /// depth. `inflight_kind` labels each gauge so a dashboard can
    /// attribute the pressure to the workload kind the dispatcher
    /// just selected.
    fn publish_queue_depths(&self, inflight_kind: WorkloadKind) {
        for (class, depth) in [
            (Priority::Realtime, self.realtime_rx.len()),
            (Priority::Interactive, self.interactive_rx.len()),
            (Priority::Background, self.background_rx.len()),
            (Priority::Batch, self.batch_rx.len()),
        ] {
            gauge!(
                "sy_queue_depth",
                "class" => class.as_str(),
                "kind" => inflight_kind.as_str(),
            )
            .set(depth as f64);
        }
    }

    fn run_one(&self, aiplane: &Arc<dyn AiplaneDispatch>, req: Request) {
        let kind = req.workload;
        let class = req.class;
        // SPEC §4.6 `sy_queue_depth{class, kind}`. The four bounded
        // queues are per-class; the kind label reflects the request
        // we just pulled so a dashboard can see "Background/Embed
        // had N waiters when the dispatcher last drained it". A
        // per-kind queue split is a future refinement (SPEC §4.3
        // future work on Triton-style per-model admission).
        self.publish_queue_depths(kind);
        if req.cancel.is_cancelled() {
            // SPEC §4.6: cancellation is an error class on the workload
            // bucket. The `reason` label lets dashboards split
            // user-cancelled from worker-failed without parsing a
            // free-text message.
            counter!(
                "sy_workload_errors_total",
                "kind" => kind.as_str(),
                "reason" => "cancelled",
            )
            .increment(1);
            let _ = req.respond.send(Err(AiplaneError::Cancelled));
            return;
        }
        // Record this request in the inflight slot so the
        // cross-class hard-escape watchdog (Step 8) can see how long
        // it's been running and whether it must yield to a
        // higher-priority queued caller.
        *self.inflight.lock().expect("dispatcher inflight poisoned") = Some(InflightInfo {
            request_id: req.id,
            workload: kind,
            class,
            started_at: Instant::now(),
        });
        let started = Instant::now();
        let outcome = aiplane.run(kind, req.input);
        let latency = started.elapsed();
        *self.inflight.lock().expect("dispatcher inflight poisoned") = None;
        // SPEC §4.6: emit the completed counter on success and the
        // error counter (with a coarse `reason="failed"` label) on
        // failure. The histogram tracks dispatch-to-completion
        // latency for *every* run so percentile dashboards include
        // failed runs.
        histogram!(
            "sy_workload_latency_seconds",
            "kind" => kind.as_str(),
        )
        .record(latency.as_secs_f64());
        match &outcome {
            Ok(_) => {
                counter!(
                    "sy_workload_completed_total",
                    "kind" => kind.as_str(),
                )
                .increment(1);
            }
            Err(_) => {
                counter!(
                    "sy_workload_errors_total",
                    "kind" => kind.as_str(),
                    "reason" => "failed",
                )
                .increment(1);
            }
        }
        let _ = req
            .respond
            .send(outcome.map_err(AiplaneError::WorkloadFailed));
        let _ = req.queued_at;
        let _ = req.deadline;
    }
}

/// Sibling thread spawned alongside the dispatcher. Polls the
/// inflight slot + per-class queue depths every [`HARD_ESCAPE_TICK`];
/// fires `AiplaneDispatch::cancel` on the inflight whenever it has
/// been running ≥ [`HARD_ESCAPE_THRESHOLD`] AND a strictly
/// higher-priority queue is non-empty (SPEC §4.3 "cross-class hard
/// escape"). Each inflight gets at most one cancel — the watchdog
/// deduplicates by `request_id` so a slow cancel path doesn't get
/// repeatedly tickled. Exits cleanly once both the bridge's
/// `Scheduler` and the in-thread `Dispatcher` have dropped their
/// `inflight` `Arc` clones (`strong_count <= 1` means only the
/// watchdog itself holds the slot).
fn hard_escape_loop(
    inflight: Arc<Mutex<Option<InflightInfo>>>,
    realtime_rx: Receiver<Request>,
    interactive_rx: Receiver<Request>,
    background_rx: Receiver<Request>,
    aiplane: Arc<dyn AiplaneDispatch>,
) {
    let mut last_cancelled: Option<Ulid> = None;
    loop {
        thread::sleep(HARD_ESCAPE_TICK);
        if Arc::strong_count(&inflight) <= 1 {
            return;
        }
        let Some(info) = inflight.lock().expect("inflight poisoned").clone() else {
            continue;
        };
        if Some(info.request_id) == last_cancelled {
            continue;
        }
        if info.started_at.elapsed() < HARD_ESCAPE_THRESHOLD {
            continue;
        }
        let preempt = match info.class {
            Priority::Realtime => false,
            Priority::Interactive => !realtime_rx.is_empty(),
            Priority::Background => !realtime_rx.is_empty() || !interactive_rx.is_empty(),
            Priority::Batch => {
                !realtime_rx.is_empty() || !interactive_rx.is_empty() || !background_rx.is_empty()
            }
        };
        if !preempt {
            continue;
        }
        let _ = aiplane.cancel(info.workload, info.request_id);
        last_cancelled = Some(info.request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aiplane::registry::{WorkloadInput, WorkloadKind, WorkloadOutput};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use metrics_util::MetricKind;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    const SAMPLE_TEXT: &str = "hi";

    fn sample_request(
        class: Priority,
    ) -> (
        Request,
        oneshot::Receiver<Result<WorkloadOutput, AiplaneError>>,
    ) {
        Request::new(
            Ulid::new(),
            WorkloadKind::Embed,
            WorkloadInput::Text {
                text: SAMPLE_TEXT.into(),
            },
            class,
            None,
            CancellationToken::new(),
        )
    }

    /// Hermetic `AiplaneDispatch` used by the scheduler tests below.
    /// Counts calls and optionally blocks each call on a per-instance
    /// gate so the dispatcher's interleaving can be observed without
    /// relying on real ORT.
    struct CountingDispatch {
        calls: AtomicUsize,
        order: Mutex<Vec<Priority>>,
        per_call_delay: Duration,
    }

    impl CountingDispatch {
        fn new(per_call_delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                order: Mutex::new(Vec::new()),
                per_call_delay,
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn snapshot(&self) -> Vec<Priority> {
            self.order.lock().expect("order poisoned").clone()
        }
    }

    /// `AiplaneDispatch` needs to know the priority class of each
    /// inflight request to record the dispatch order. We thread it
    /// through `WorkloadInput::Text` by sneaking the class name into
    /// the text payload so the trait shape doesn't have to change.
    impl AiplaneDispatch for CountingDispatch {
        fn run(
            &self,
            _workload: WorkloadKind,
            input: WorkloadInput,
        ) -> anyhow::Result<WorkloadOutput> {
            let label = match &input {
                WorkloadInput::Text { text } => text.clone(),
                _ => String::new(),
            };
            let class = match label.as_str() {
                "Realtime" => Priority::Realtime,
                "Interactive" => Priority::Interactive,
                "Background" => Priority::Background,
                "Batch" => Priority::Batch,
                _ => Priority::Interactive,
            };
            std::thread::sleep(self.per_call_delay);
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.order.lock().expect("order poisoned").push(class);
            Ok(WorkloadOutput::Text { text: label })
        }

        fn batch(
            &self,
            workload: WorkloadKind,
            inputs: Vec<WorkloadInput>,
        ) -> anyhow::Result<Vec<WorkloadOutput>> {
            inputs.into_iter().map(|i| self.run(workload, i)).collect()
        }
    }

    fn build_labelled_request(
        class: Priority,
    ) -> (
        Request,
        oneshot::Receiver<Result<WorkloadOutput, AiplaneError>>,
    ) {
        let (mut req, rx) = sample_request(class);
        req.input = WorkloadInput::Text {
            text: class.as_str().into(),
        };
        (req, rx)
    }

    /// Install a `DebuggingRecorder` exactly once per test binary
    /// and return the cached `Snapshotter`. The recorder's
    /// per-counter `swap`-on-snapshot semantics let parallel tests
    /// share the global recorder: callers hold [`TEST_ENV_LOCK`],
    /// take a fresh snapshot to drain prior values, exercise the
    /// scheduler, then take a second snapshot whose values reflect
    /// only their own emissions.
    fn test_snapshotter() -> metrics_util::debugging::Snapshotter {
        static SNAPSHOTTER: std::sync::OnceLock<metrics_util::debugging::Snapshotter> =
            std::sync::OnceLock::new();
        SNAPSHOTTER
            .get_or_init(|| {
                let recorder = DebuggingRecorder::new();
                let snapshotter = recorder.snapshotter();
                let _ = recorder.install();
                snapshotter
            })
            .clone()
    }

    /// Extract the value of a counter from the snapshot, matching
    /// both the metric name and (`label_key`, `label_value`).
    /// Consumes the snapshot — `Snapshot` is non-Clone so callers
    /// take a fresh snapshot per assertion.
    fn counter_value(
        snap: metrics_util::debugging::Snapshot,
        name: &str,
        label_key: &str,
        label_value: &str,
    ) -> Option<u64> {
        for (ck, _unit, _desc, value) in snap.into_vec() {
            if ck.kind() != MetricKind::Counter {
                continue;
            }
            if ck.key().name() != name {
                continue;
            }
            if !ck
                .key()
                .labels()
                .any(|l| l.key() == label_key && l.value() == label_value)
            {
                continue;
            }
            if let DebugValue::Counter(v) = value {
                return Some(v);
            }
        }
        None
    }

    #[tokio::test]
    async fn request_round_trip_observes_recv_error_on_sender_drop() {
        // Lifecycle sanity: dropping the request's `respond` sender
        // before the worker writes a reply must surface as a
        // `RecvError` on the awaiting oneshot. Otherwise a cancelled
        // request leaks a never-completing future on the caller.
        let (req, rx) = sample_request(Priority::Interactive);
        drop(req); // workload never picked it up — sender drops.
        assert!(rx.await.is_err(), "oneshot must surface RecvError on drop");
    }

    #[test]
    fn workload_completed_increments_counter() {
        // SPEC §4.6 / arch-observability Step 7: a successful
        // workload run must bump `sy_workload_completed_total{kind}`
        // by exactly one. Drive a single Interactive request through
        // the dispatcher, snapshot the recorder before and after,
        // and assert the delta on the Embed bucket.
        //
        // Uses a tokio current-thread runtime built inside the test
        // body so the `TEST_ENV_LOCK` mutex never crosses an await
        // (clippy::await_holding_lock).
        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let snapshotter = test_snapshotter();
        // Drain any prior counter state so this test starts at zero.
        let _ = snapshotter.snapshot();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let outcome = rt.block_on(async {
            let dispatch = CountingDispatch::new(Duration::from_millis(0));
            let (scheduler, dispatcher) = Scheduler::new();
            let handle = dispatcher.run(Arc::clone(&dispatch) as Arc<dyn AiplaneDispatch>);

            let (req, rx) = sample_request(Priority::Interactive);
            scheduler.admit(req).expect("admit");
            let result = tokio::time::timeout(Duration::from_secs(5), rx)
                .await
                .expect("dispatcher drained")
                .expect("oneshot")
                .expect("workload ok");
            drop(scheduler);
            handle.join().expect("dispatcher joins on drop");
            result
        });
        assert!(matches!(outcome, WorkloadOutput::Text { .. }));

        let snap = snapshotter.snapshot();
        let v = counter_value(snap, "sy_workload_completed_total", "kind", "embed")
            .expect("sy_workload_completed_total{kind=embed} must be present");
        assert_eq!(v, 1, "exactly one completed Embed run");
    }

    #[test]
    fn queue_depths_reflect_pending_admissions_per_class() {
        // SPEC §4.3 + Cross-cutting DoD: `sy aiplane status --json`
        // reports per-class queue depths. The scheduler exposes them
        // via [`Scheduler::queue_depths`]; the value tracks the live
        // `crossbeam_channel` len as requests admit and drain.
        const N_REALTIME: usize = 2;
        const N_BACKGROUND: usize = 5;
        let (scheduler, _dispatcher) = Scheduler::new();
        let mut keep = Vec::new();
        for _ in 0..N_REALTIME {
            let (req, rx) = sample_request(Priority::Realtime);
            scheduler.admit(req).expect("admit realtime");
            keep.push(rx);
        }
        for _ in 0..N_BACKGROUND {
            let (req, rx) = sample_request(Priority::Background);
            scheduler.admit(req).expect("admit background");
            keep.push(rx);
        }
        let depths = scheduler.queue_depths();
        assert_eq!(depths[&Priority::Realtime], N_REALTIME);
        assert_eq!(depths[&Priority::Background], N_BACKGROUND);
        assert_eq!(depths[&Priority::Interactive], 0);
        assert_eq!(depths[&Priority::Batch], 0);
    }

    #[test]
    fn overloaded_rejects_realtime() {
        // Realtime cap=4 with `Reject` policy: a 5th admit returns
        // `Err(Overloaded { class: Realtime, .. })` immediately.
        // Don't drive the dispatcher — that would drain the queue
        // before we can observe the cap. Keep the `Dispatcher` half
        // alive so admit doesn't see `Disconnected`.
        let (scheduler, _dispatcher) = Scheduler::new();
        let mut keep = Vec::new();
        for _ in 0..CAP_REALTIME {
            let (req, rx) = sample_request(Priority::Realtime);
            scheduler.admit(req).expect("under cap");
            keep.push(rx);
        }
        let (req, _rx) = sample_request(Priority::Realtime);
        match scheduler.admit(req) {
            Err(AiplaneError::Overloaded {
                class: Priority::Realtime,
                queue_depth,
                retry_after_ms,
            }) => {
                assert_eq!(queue_depth, CAP_REALTIME);
                assert_eq!(retry_after_ms, OVERLOADED_RETRY_AFTER_MS);
            }
            other => panic!("expected Overloaded(Realtime), got {other:?}"),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn background_delays_then_runs() {
        // Background policy is Delay: cap+1 admits must all eventually
        // succeed once the dispatcher drains the queue. No rejections.
        //
        // `TEST_ENV_LOCK` here is a serialization barrier against the
        // global `DebuggingRecorder` used by
        // `workload_completed_increments_counter` — without it our
        // dispatcher's `sy_workload_completed_total{kind=embed}`
        // emissions can pollute that test's snapshot delta. The lock
        // is a std `Mutex` held across `.await`, which is benign in a
        // `current_thread` tokio runtime (the default for
        // `#[tokio::test]`).
        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        const OVER_CAP: usize = CAP_BACKGROUND + 1;
        let dispatch = CountingDispatch::new(Duration::from_millis(0));
        let (scheduler, dispatcher) = Scheduler::new();
        let scheduler = Arc::new(scheduler);
        let handle = dispatcher.run(Arc::clone(&dispatch) as Arc<dyn AiplaneDispatch>);

        let mut receivers = Vec::with_capacity(OVER_CAP);
        for _ in 0..OVER_CAP {
            let (req, rx) = sample_request(Priority::Background);
            // `admit` on a full Background queue blocks; offload to
            // spawn_blocking so the tokio runtime stays responsive.
            let scheduler = Arc::clone(&scheduler);
            tokio::task::spawn_blocking(move || scheduler.admit(req))
                .await
                .expect("join")
                .expect("admit ok");
            receivers.push(rx);
        }
        for rx in receivers {
            let result = tokio::time::timeout(Duration::from_secs(5), rx)
                .await
                .expect("dispatcher drained queue")
                .expect("oneshot")
                .expect("workload ok");
            assert!(matches!(result, WorkloadOutput::Text { .. }));
        }
        drop(scheduler);
        handle.join().expect("dispatcher joins on drop");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn higher_class_never_starves_to_lower() {
        // Enqueue a batch of Background, then a single Interactive.
        // The dispatcher serialises through a single ORT-like worker
        // (per-call delay = 5 ms), so the Interactive must complete
        // before the bulk of the Background backlog: order of
        // executed requests should show the Interactive within the
        // first 3 entries (the dispatcher may have pulled 1-2
        // Background already before the Interactive arrived).
        //
        // See `background_delays_then_runs` for why `TEST_ENV_LOCK`
        // is held here: it serialises sibling scheduler tests against
        // `workload_completed_increments_counter` so their dispatcher
        // emissions don't bleed into that test's snapshot delta.
        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        const N_BG: usize = 20;
        let dispatch = CountingDispatch::new(Duration::from_millis(5));
        let (scheduler, dispatcher) = Scheduler::new();
        let scheduler = Arc::new(scheduler);
        let handle = dispatcher.run(Arc::clone(&dispatch) as Arc<dyn AiplaneDispatch>);

        let mut bg_rxs = Vec::with_capacity(N_BG);
        for _ in 0..N_BG {
            let (req, rx) = build_labelled_request(Priority::Background);
            scheduler.admit(req).expect("admit bg");
            bg_rxs.push(rx);
        }
        // Tiny gap so the dispatcher pulls at most one Background
        // before the Interactive request arrives. Larger than the
        // per-call delay so at least one Background completes first;
        // smaller than the full backlog so the Interactive still
        // preempts.
        tokio::time::sleep(Duration::from_millis(8)).await;
        let (req, interactive_rx) = build_labelled_request(Priority::Interactive);
        scheduler.admit(req).expect("admit interactive");

        // Wait for the Interactive to finish.
        let _ = tokio::time::timeout(Duration::from_secs(5), interactive_rx)
            .await
            .expect("interactive completes")
            .expect("oneshot")
            .expect("workload ok");

        // Drain remaining Background so the dispatcher quiesces
        // before we read the order log.
        for rx in bg_rxs {
            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
        }
        drop(scheduler);
        handle.join().expect("join");

        let order = dispatch.snapshot();
        let interactive_idx = order
            .iter()
            .position(|p| *p == Priority::Interactive)
            .expect("interactive ran");
        assert!(
            interactive_idx < 3,
            "interactive must preempt the bulk of background: order={order:?}"
        );
        assert!(dispatch.calls() > N_BG);
    }

    /// `AiplaneDispatch` used by the cross-class hard-escape test:
    /// every `run` parks on a gate forever; `cancel` records the
    /// (workload, request_id) AND releases the gate so the
    /// dispatcher can finish unwinding. This is the minimal shape
    /// that lets the watchdog observe "inflight too long, higher
    /// priority queued → preempt" and lets the test assert the
    /// cancel actually fired.
    struct GatedRecordingDispatch {
        gate: Mutex<bool>,
        gate_cv: std::sync::Condvar,
        cancels: Mutex<Vec<(WorkloadKind, Ulid)>>,
    }

    impl GatedRecordingDispatch {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                gate: Mutex::new(false),
                gate_cv: std::sync::Condvar::new(),
                cancels: Mutex::new(Vec::new()),
            })
        }

        fn cancels(&self) -> Vec<(WorkloadKind, Ulid)> {
            self.cancels.lock().expect("cancels poisoned").clone()
        }
    }

    impl AiplaneDispatch for GatedRecordingDispatch {
        fn run(
            &self,
            _workload: WorkloadKind,
            _input: WorkloadInput,
        ) -> anyhow::Result<WorkloadOutput> {
            let mut g = self.gate.lock().expect("gate poisoned");
            while !*g {
                g = self.gate_cv.wait(g).expect("gate cv poisoned");
            }
            Ok(WorkloadOutput::Text {
                text: "gated".into(),
            })
        }
        fn batch(
            &self,
            workload: WorkloadKind,
            inputs: Vec<WorkloadInput>,
        ) -> anyhow::Result<Vec<WorkloadOutput>> {
            inputs.into_iter().map(|i| self.run(workload, i)).collect()
        }
        fn cancel(&self, workload: WorkloadKind, request_id: Ulid) -> anyhow::Result<()> {
            self.cancels
                .lock()
                .expect("cancels poisoned")
                .push((workload, request_id));
            *self.gate.lock().expect("gate poisoned") = true;
            self.gate_cv.notify_all();
            Ok(())
        }
    }

    #[test]
    fn cross_class_hard_escape_interactive_preempts_batch() {
        // SPEC §4.3 / arch-aiplane-scheduler Step 8: an inflight
        // Batch that has been running ≥ HARD_ESCAPE_THRESHOLD must
        // yield to a queued Interactive. The watchdog tick fires
        // `AiplaneDispatch::cancel(Batch's workload, Batch's id)`
        // within HARD_ESCAPE_TICK after the Interactive lands in
        // the queue.
        //
        // Hold `TEST_ENV_LOCK` so this test's dispatcher emissions
        // can't pollute the snapshot delta inspected by
        // `workload_completed_increments_counter` under parallel
        // `cargo test` runs.
        let _guard = crate::aiplane::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        const SETTLE_AFTER_ADMIT: Duration = Duration::from_millis(50);
        const POST_INTERACTIVE_BUDGET: Duration = Duration::from_millis(150);

        let dispatch = GatedRecordingDispatch::new();
        let (scheduler, dispatcher) = Scheduler::new();
        let scheduler = Arc::new(scheduler);
        let _handle = dispatcher.run(Arc::clone(&dispatch) as Arc<dyn AiplaneDispatch>);

        // Admit Batch — the dispatcher pulls it and parks on the gate.
        let (batch_req, _batch_rx) = sample_request(Priority::Batch);
        let batch_id = batch_req.id;
        scheduler.admit(batch_req).expect("admit batch");

        // Let the dispatcher fully transition into the gated `run`.
        std::thread::sleep(SETTLE_AFTER_ADMIT);

        // Cross the threshold so the watchdog considers the inflight
        // ripe — without this an Interactive admit would not yet
        // qualify for hard escape.
        std::thread::sleep(HARD_ESCAPE_THRESHOLD);

        // Admit Interactive. The watchdog's next tick (≤ HARD_ESCAPE_TICK
        // later) sees `Interactive non-empty` + inflight Batch over
        // threshold → fires cancel.
        let (i_req, _i_rx) = sample_request(Priority::Interactive);
        scheduler.admit(i_req).expect("admit interactive");

        std::thread::sleep(POST_INTERACTIVE_BUDGET);

        let recorded = dispatch.cancels();
        assert!(
            !recorded.is_empty(),
            "watchdog must fire at least one cancel"
        );
        assert_eq!(recorded[0].0, WorkloadKind::Embed);
        assert_eq!(recorded[0].1, batch_id);

        drop(scheduler);
    }
}
