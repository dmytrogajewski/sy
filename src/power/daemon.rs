//! `sy-powerd` — Step 10 scaffold + Step 19 actuation loop + Step 22
//! Conservative LinUCB bandit.
//!
//! Owns the 1 Hz sensor + intent tick. Each tick now (Step 22):
//!
//! 1. Reads every sensor + drains every intent channel into a
//!    [`Snapshot`].
//! 2. Runs the snapshot through the Step 17 shield DFA to derive the
//!    current `ShieldState`.
//! 3. Folds the *previous* tick's reward back into the bandit: the
//!    "before" snapshot is whatever the bandit picked at tick N−1,
//!    "after" is this tick's freshly-collected snapshot. The reward
//!    is fed into both `Clucb::update` (per-arm posterior) and
//!    `Clucb::observe_baseline` (CLUCB conservative anchor). Tick 1
//!    has no `before` so no update fires.
//! 4. Asks the bandit for a ranked action list via `propose_ranked`.
//!    When the operator has set a pin (`sy power profile <name>`),
//!    that arm pre-empts the bandit. Otherwise the rules baseline is
//!    prepended whenever the bandit's top arm fails the CLUCB
//!    conservative gate (UCB margin < α OR LCB floor not satisfied)
//!    so the bandit must earn the right to deviate.
//! 5. Walks `shield::project` against the ranked arm list so the
//!    shield's constraint table + thrash limiter veto a pick that
//!    violates the safety envelope.
//! 6. Calls the five actuators (`platform_profile`, EPP, iGPU, NPU,
//!    cgroup) in order. Per-actuator errors are logged but never
//!    abort the tick — the lever we couldn't write degrades to "leave
//!    it as the kernel reports it"; we still record the audit entry.
//! 7. Appends an R3-shape [`AuditEntry`] (`ranked_actions` top-3 +
//!    `conservative_alpha` + applied arm + shield state + reason
//!    chain) to the NDJSON audit log; caches the same entry in
//!    [`LatestAuditEntry`] so the IPC `Status` response can populate
//!    the `bandit` block without re-reading disk.
//! 8. Publishes the snapshot to [`LatestSnapshot`] for IPC consumers
//!    and pings `WATCHDOG=1` at half the systemd-configured interval
//!    from the dedicated [`spawn_watchdog_thread`] helper.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::power::activity::{ActivityLabel, OnlineClassifier, ACTIVITY_CLASS_COUNT};
use crate::power::apply::{
    self, Actuator, ActuatorLatches, Applied, CgroupActuator, EppActuator, IgpuActuator,
    LatchOutcome, LeverLatch, NpuActuator, PlatformProfileActuator,
};
use crate::power::bandit::{compute_reward, for_snapshot_features_with_activity, Arm, Clucb};
use crate::power::checkpoint::{
    self, DaemonCheckpoint, CHECKPOINT_INTERVAL_TICKS, CHECKPOINT_SCHEMA,
};
use crate::power::clock::Clock;
use crate::power::config::PowerConfig;
use crate::power::drift::{DriftDetector, DriftSignal, DriftStatus};
use crate::power::log::{AuditEntry, LogError, Logger};
use crate::power::onboarding::OnboardingStatus;
use crate::power::policy::rules_baseline;
use crate::power::shield::{self, ShieldState, ThrashTracker};
use crate::power::snapshot::{self, Intent, Sensors, Snapshot};

/// Minimum AC + idle + SOC retrain window (Step 26): the daemon
/// schedules `trainer::retrain_gru` only when the user has been idle
/// for at least this many seconds. 5 minutes matches the SPEC §3
/// "trainer never runs while the user is active" promise.
pub const RETRAIN_IDLE_THRESHOLD_S: f32 = 300.0;

/// Minimum battery SOC the retrain scheduler accepts before kicking
/// off a training job. 50% mirrors SPEC §3's "plugged-in only" rule
/// — the laptop is on AC, but if the battery is below 50% the kernel
/// may still be in pass-through-charging mode and the extra CPU load
/// would drain the pack. Fail-closed.
pub const RETRAIN_SOC_FLOOR_PCT: u8 = 50;

/// Cooldown between successive trainer kickoffs. SPEC §3's "trainer
/// runs in an idle+plugged window" presumes overnight cadence; six
/// hours is a conservative floor that still lets a heavy dev day
/// retrain by morning.
pub const RETRAIN_COOLDOWN: Duration = Duration::from_secs(6 * 60 * 60);

/// Reason the retrain scheduler refused to dispatch this tick.
/// Returned by [`evaluate_retrain_trigger`] so tests can pin which
/// gate fired without scraping log lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrainSkipReason {
    /// `snapshot.raw.ac_online` was `false` or unknown.
    OnBattery,
    /// `snapshot.raw.user_idle_s` was below
    /// [`RETRAIN_IDLE_THRESHOLD_S`] (or absent).
    UserActive,
    /// `snapshot.raw.battery_soc_pct` was at or below
    /// [`RETRAIN_SOC_FLOOR_PCT`] (or absent).
    LowSoc,
    /// Less than [`RETRAIN_COOLDOWN`] has elapsed since the last
    /// dispatched retrain.
    Cooldown,
    /// [`OnboardingStatus::active`] is `true` — the trainer needs at
    /// least 14 days of telemetry before its first run.
    Onboarding,
}

/// Outcome of one [`evaluate_retrain_trigger`] call. `Dispatched`
/// means the trigger fired; `Skipped(reason)` carries the first gate
/// that failed (gates are checked in a fixed order so the reason is
/// deterministic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrainOutcome {
    Dispatched,
    Skipped(RetrainSkipReason),
}

/// Why the daemon scheduled this retrain. Step 31 differentiates the
/// post-onboarding "first training" (`Onboarding`) from a mid-life
/// drift-driven retrain (`Drift`); the audit log records the cause so
/// `sy power explain` can attribute each model swap to its trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrainCause {
    /// First retrain after the 14-day onboarding window closes. The
    /// daemon transitions from rules-baseline to bandit at this point.
    Onboarding,
    /// ADWIN alarm fired on the forecast-residual stream (or DDM on
    /// the reward residual). The daemon temporarily reverts to rules-
    /// baseline and schedules a fresh GRU train.
    Drift,
}

/// Trainer dispatch sink. Production wires
/// [`SpawnBlockingRetrainTrigger`] (calls `tokio::task::spawn_blocking`);
/// tests wire `CapturingRetrainTrigger` so the
/// `train_skipped_when_*` assertions never spin up a real burn
/// training loop.
pub trait RetrainTrigger: Send + Sync {
    /// Fire one retrain. Called from inside [`one_tick`] when every
    /// retrain gate passes. The implementation MUST NOT block the
    /// tick — production hands the work off to a worker thread.
    /// `cause` differentiates the post-onboarding first train from a
    /// drift-driven retrain so the audit log + telemetry can group on
    /// it without re-deriving from sentinel fields.
    fn dispatch(&self, cause: RetrainCause);
}

/// Production retrain trigger — hands the trainer call off to a
/// `tokio::task::spawn_blocking` worker so the 1 Hz tick keeps its
/// cadence. The `telemetry_path` + `out_path` are captured by clone
/// at construction time so the spawned closure owns its arguments.
/// `model_status` is the shared slot the dispatcher publishes the
/// last retrain's coverage / recall verdict to (Step T3 /
/// BUG-20260525-2352) so `sy power status --json` can surface a
/// skipped train without re-tailing `journalctl`.
pub struct SpawnBlockingRetrainTrigger {
    pub telemetry_path: PathBuf,
    pub out_path: PathBuf,
    pub model_status: LatestModelStatus,
}

impl SpawnBlockingRetrainTrigger {
    /// Build the production trigger for `state_root`. The trainer reads
    /// the *whole* telemetry directory (daily-segmented NDJSON) and
    /// writes the trained model to `<state_root>/forecaster.onnx` — the
    /// same filename `sy power train` writes and `sy power status`
    /// reads (`src/power/cli.rs`), so the CLI and daemon stay in
    /// lockstep. Pinning both paths in one place is what BUG-20260712-1545
    /// part 2 fixes (the daemon previously wrote `forecast.onnx`).
    pub fn for_state_root(state_root: &Path, model_status: LatestModelStatus) -> Self {
        Self {
            telemetry_path: state_root.to_path_buf(),
            out_path: state_root.join("forecaster.onnx"),
            model_status,
        }
    }
}

impl RetrainTrigger for SpawnBlockingRetrainTrigger {
    fn dispatch(&self, cause: RetrainCause) {
        let telemetry = self.telemetry_path.clone();
        let out = self.out_path.clone();
        let model_status = self.model_status.clone();
        tokio::task::spawn_blocking(move || {
            match crate::power::trainer::retrain_gru(&telemetry, &out) {
                Ok(report) => {
                    let excluded: Vec<String> = report
                        .excluded_classes
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect();
                    tracing::info!(
                        target: "sy::power::daemon",
                        rows = report.rows_used,
                        final_loss = report.final_loss,
                        accuracy = report.validation_accuracy,
                        wall_ms = report.wall_time_ms as u64,
                        version_sha = %report.version_sha,
                        cause = ?cause,
                        excluded_classes = ?excluded,
                        "trainer retrain completed",
                    );
                    // BUG-20260723-2210: a partial train keeps its
                    // blind spots visible on `sy power status` —
                    // excluded classes publish as missing_classes.
                    publish_model_status(
                        &model_status,
                        crate::power::ipc::ModelStatus {
                            missing_classes: excluded,
                        },
                    );
                }
                Err(e) => {
                    log_and_publish_retrain_error(&model_status, cause, &e);
                }
            }
        });
    }
}

/// Map a [`crate::power::trainer::TrainerError`] to a `tracing::warn!`
/// line + a [`crate::power::ipc::ModelStatus`] published on the shared
/// slot. The per-class-coverage and per-class-recall errors carry
/// structured fields (missing classes / recall message) so
/// `sy power explain` and the operator's `journalctl` can attribute
/// the skipped train without re-parsing the human-readable error
/// string.
fn log_and_publish_retrain_error(
    slot: &LatestModelStatus,
    cause: RetrainCause,
    err: &crate::power::trainer::TrainerError,
) {
    use crate::power::ipc::ModelStatus;
    use crate::power::trainer::TrainerError;
    match err {
        TrainerError::InsufficientClassCoverage {
            missing,
            counts,
            required,
        } => {
            let missing_strs: Vec<String> = missing.iter().map(|s| (*s).to_string()).collect();
            tracing::warn!(
                target: "sy::power::daemon",
                error = %err,
                cause = ?cause,
                missing_classes = ?missing_strs,
                class_counts = ?counts,
                required = *required,
                "trainer retrain skipped: insufficient per-class coverage",
            );
            publish_model_status(
                slot,
                ModelStatus {
                    missing_classes: missing_strs,
                },
            );
        }
        TrainerError::ValidationFailed(msg) if msg.starts_with("per-class recall floor") => {
            tracing::warn!(
                target: "sy::power::daemon",
                error = %err,
                cause = ?cause,
                recall = %msg,
                "trainer retrain skipped: per-class recall floor",
            );
            publish_model_status(slot, ModelStatus::default());
        }
        _ => {
            tracing::warn!(
                target: "sy::power::daemon",
                error = %err,
                cause = ?cause,
                "trainer retrain failed",
            );
            publish_model_status(slot, ModelStatus::default());
        }
    }
}

/// Best-effort publish of a [`crate::power::ipc::ModelStatus`] to the
/// shared slot. A poisoned lock is logged + dropped — the next
/// successful publish overwrites the stale state, so we never block
/// the trainer worker on a panicked IPC handler.
fn publish_model_status(slot: &LatestModelStatus, status: crate::power::ipc::ModelStatus) {
    match slot.write() {
        Ok(mut g) => *g = Some(status),
        Err(e) => tracing::warn!(
            target: "sy::power::daemon",
            error = %e,
            "model_status slot poisoned; dropping update",
        ),
    }
}

/// Step 26 onboarding + retrain bookkeeping. Owned by the daemon's
/// tick loop and passed through `&mut` into [`one_tick`] so the
/// previous tick's cooldown anchor survives across calls.
#[derive(Debug, Default)]
pub struct OnboardingTickState {
    /// Most recent [`OnboardingStatus`] computed at the top of the
    /// tick. The bandit propose path is gated on `active`; the
    /// retrain scheduler also reads it (no training before day 14).
    pub status: Option<OnboardingStatus>,
    /// Wall-clock instant of the last dispatched retrain. `None`
    /// means "no retrain has ever fired"; the scheduler treats that
    /// as cooldown-satisfied.
    pub last_retrain_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl OnboardingTickState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Decide whether to dispatch a retrain this tick. Pure function —
/// every input is explicit so tests can drive the truth table
/// directly without standing up a daemon. The check order matches
/// the SPEC §3 sentinel chain:
///
/// 1. Onboarding active ⇒ [`RetrainSkipReason::Onboarding`].
/// 2. Not on AC ⇒ [`RetrainSkipReason::OnBattery`].
/// 3. User active (idle < 5 min) ⇒ [`RetrainSkipReason::UserActive`].
/// 4. SOC ≤ 50% ⇒ [`RetrainSkipReason::LowSoc`].
/// 5. Cooldown not elapsed ⇒ [`RetrainSkipReason::Cooldown`].
pub fn evaluate_retrain_trigger(
    snap: &Snapshot,
    onboarding_active: bool,
    last_retrain_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> RetrainOutcome {
    if onboarding_active {
        return RetrainOutcome::Skipped(RetrainSkipReason::Onboarding);
    }
    if !snap.raw.ac_online.unwrap_or(false) {
        return RetrainOutcome::Skipped(RetrainSkipReason::OnBattery);
    }
    let idle = snap.raw.user_idle_s.unwrap_or(0.0);
    if idle < RETRAIN_IDLE_THRESHOLD_S {
        return RetrainOutcome::Skipped(RetrainSkipReason::UserActive);
    }
    let soc = snap.raw.battery_soc_pct.unwrap_or(0);
    if soc <= RETRAIN_SOC_FLOOR_PCT {
        return RetrainOutcome::Skipped(RetrainSkipReason::LowSoc);
    }
    if let Some(prev) = last_retrain_at {
        let cooldown = chrono::Duration::from_std(RETRAIN_COOLDOWN)
            .unwrap_or_else(|_| chrono::Duration::seconds(0));
        if now - prev < cooldown {
            return RetrainOutcome::Skipped(RetrainSkipReason::Cooldown);
        }
    }
    RetrainOutcome::Dispatched
}

/// Type-erased holder for the most recent snapshot. Written by the
/// tick loop, read by IPC handlers. `RwLock` because the read path
/// fans out across every accepted connection.
pub type LatestSnapshot = Arc<RwLock<Option<Snapshot>>>;

/// Operator-set pin slot: when `Some(name)`, the daemon forces the
/// named arm on every tick instead of consulting the rules baseline.
/// `sy power profile <name>` writes through IPC; `--auto` clears.
pub type LatestPin = Arc<RwLock<Option<String>>>;

/// Cache of the most recent [`AuditEntry`]. The IPC `Status` handler
/// reads from here so `sy power status --json`'s `applied_policy` slot
/// reflects what the daemon actually wrote on the previous tick —
/// without re-tailing the NDJSON log.
pub type LatestAuditEntry = Arc<RwLock<Option<AuditEntry>>>;

/// Build an empty latest-snapshot holder. Extracted so tests and the
/// production daemon share the same construction shape.
pub fn new_latest_snapshot() -> LatestSnapshot {
    Arc::new(RwLock::new(None))
}

/// Build an empty pin slot. Used by tests + the daemon's `run_async`.
pub fn new_pin_slot() -> LatestPin {
    Arc::new(RwLock::new(None))
}

/// Build an empty last-audit-entry cache. Mirrors [`new_latest_snapshot`].
pub fn new_latest_audit_entry() -> LatestAuditEntry {
    Arc::new(RwLock::new(None))
}

/// `sd_notify` glue, separated behind a trait so the watchdog cadence
/// test can mock-capture pings instead of round-tripping through
/// systemd. Production wires [`SystemNotifier`] (calls
/// `sy_core::notify::*`); tests wire `MockNotifier`.
pub trait Notifier: Send + Sync {
    /// Fire `WATCHDOG=1`. Called at half the systemd-configured
    /// `WATCHDOG_USEC` (~5 s for our 10 s unit).
    fn watchdog_ping(&self);
}

/// Production `Notifier` — delegates to `sd_notify::notify`. The
/// underlying call is a no-op on non-systemd hosts (no
/// `NOTIFY_SOCKET`), so a developer running `cargo run -- power
/// daemon` outside `systemctl --user` doesn't see spurious errors.
#[derive(Debug, Default)]
pub struct SystemNotifier;

impl Notifier for SystemNotifier {
    fn watchdog_ping(&self) {
        use sd_notify::NotifyState;
        if let Err(e) = sd_notify::notify(&[NotifyState::Watchdog]) {
            tracing::debug!(
                target: "sy::power::daemon",
                error = %e,
                "sd_notify(Watchdog) failed (likely no NOTIFY_SOCKET)"
            );
        }
    }
}

/// Step 19 tick context. Holds every per-instance handle the actuation
/// loop needs that doesn't change tick-to-tick (the actuators are
/// stateless modulo the NPU rate-limiter, which lives inside
/// `NpuActuator`). Bundled so `one_tick`'s signature doesn't balloon
/// past clippy's `too_many_arguments` warning while still keeping each
/// dependency injectable for tests.
pub struct TickContext<'a> {
    pub sysfs_root: PathBuf,
    pub cgroup_root: PathBuf,
    pub cfg: &'a PowerConfig,
    pub thrash: &'a ThrashTracker,
    pub npu: &'a NpuActuator,
}

/// Snapshot of the previous tick's bandit decision. Persisted across
/// ticks inside [`BanditTickState`] so the reward computed at tick N
/// (using tick N-1's `before` snapshot and tick N's `after` snapshot)
/// can be fed back into `Clucb::update` with the same context vector
/// the bandit saw when it picked the arm.
#[derive(Debug, Clone)]
pub struct LastChosen {
    pub arm: String,
    pub snapshot: Snapshot,
    pub context: Vec<f32>,
    pub prev_arm: Option<String>,
}

/// Cross-tick shield state. Bundles the previous tick's DFA output
/// with the wall-clock instant `call_active` was last observed true so
/// the daemon can feed `secs_since_call` into [`shield::transition`].
/// MEETING therefore releases `cfg.shield.meeting_lock_after_vad_s`
/// seconds after the call ends instead of latching until daemon
/// restart (BUG-20260712-1201).
#[derive(Debug, Clone)]
pub struct ShieldTickState {
    /// DFA state applied on the previous tick. Seeds the next
    /// `transition`'s `prev` argument.
    pub prev: ShieldState,
    /// Wall-clock instant `call_active` was last observed true. `None`
    /// until the first live call this run; drives the `secs_since_call`
    /// the DFA uses to age the MEETING lock window.
    pub last_call_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for ShieldTickState {
    fn default() -> Self {
        Self {
            prev: ShieldState::CoolAc,
            last_call_at: None,
        }
    }
}

impl ShieldTickState {
    /// Fresh state: `COOL_AC` with no call ever seen.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Step 22 bandit state held across ticks: the Clucb posterior, the
/// previous tick's arm choice (for the lag-by-one reward update), and
/// the prev-prev arm name (so `compute_reward`'s thrash penalty knows
/// whether tick N-1 switched arms).
pub struct BanditTickState {
    pub bandit: Clucb,
    pub last_chosen: Option<LastChosen>,
}

/// Step 31 drift bookkeeping held across ticks. Owns the composite
/// [`DriftDetector`] (forecast-residual ADWIN + reward-residual DDM),
/// the rolling reward mean (so the DDM input is a residual against
/// the running average and not the raw reward), and the latest
/// [`DriftStatus`] surfaced over IPC. Mutated in-place each tick so
/// the post-alarm path can read the `last_alarm_at` debounce slot.
#[derive(Default)]
pub struct DriftTickState {
    /// Composite detector — ADWIN on the forecast residual, DDM on
    /// the binarised reward residual.
    pub detector: DriftDetector,
    /// Live status surfaced in the IPC `drift` block. Mirrored into
    /// the shared [`LatestDriftStatus`] slot at the bottom of each
    /// tick.
    pub status: DriftStatus,
    /// Running mean of the bandit's reward stream — the DDM input is
    /// `|reward - reward_mean| > threshold` so a small drift in the
    /// mean shows up as a steady stream of "errors". Pure online
    /// estimator (sum / n), no allocation.
    pub reward_mean: f32,
    /// Number of reward samples folded into `reward_mean`. Cap-
    /// less because the reward stream is at most ~1 Hz so even a year
    /// of continuous operation stays inside `u32::MAX`.
    pub reward_n: u32,
}

impl DriftTickState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Shared "latest drift status" slot. Written by the daemon's tick
/// loop at the bottom of each `one_tick`; read by the IPC handler to
/// populate [`crate::power::ipc::StatusResponse::drift`].
pub type LatestDriftStatus = Arc<RwLock<DriftStatus>>;

/// Build an empty latest-drift-status holder. Mirrors
/// [`new_latest_snapshot`].
pub fn new_latest_drift_status() -> LatestDriftStatus {
    Arc::new(RwLock::new(DriftStatus::default()))
}

/// Shared "latest model health" slot (Step T3 / BUG-20260525-2352).
/// Written by [`SpawnBlockingRetrainTrigger::dispatch`] after every
/// retrain attempt — `Some(ModelStatus { missing_classes: [..] })` on
/// the per-class-coverage gate, `Some(ModelStatus::default())` on
/// other errors or success, `None` only before the first retrain ever
/// fires. Read by the IPC handler to populate
/// [`crate::power::ipc::StatusResponse::model`].
pub type LatestModelStatus = Arc<RwLock<Option<crate::power::ipc::ModelStatus>>>;

/// Build an empty latest-model-status holder. Mirrors
/// [`new_latest_drift_status`] — the slot starts `None` so
/// `sy power status --json | jq .model.missing_classes` returns
/// `null` until the trainer has reported.
pub fn new_latest_model_status() -> LatestModelStatus {
    Arc::new(RwLock::new(None))
}

/// Shared "latest onboarding gate" slot (BUG-20260712-1530). Written by
/// the daemon's tick loop right after it recomputes
/// [`OnboardingTickState::status`]; read by the IPC handler to populate
/// [`crate::power::ipc::StatusResponse::onboarding`]. Holding the
/// daemon's own view here is what lets `sy power status` report the gate
/// the daemon is *actually* enforcing rather than the CLI process's
/// re-computation, which diverges whenever the two load a different
/// `SY_POWER_ONBOARDING_DAYS` (e.g. a systemd drop-in scoping the env to
/// `sy-powerd` only). Starts `None` until the first tick computes a
/// status.
pub type LatestOnboarding = Arc<RwLock<Option<crate::power::ipc::OnboardingWire>>>;

/// Build an empty latest-onboarding holder. Mirrors
/// [`new_latest_model_status`].
pub fn new_latest_onboarding() -> LatestOnboarding {
    Arc::new(RwLock::new(None))
}

/// Desktop-side notifier for the SPEC §5 "sy-powerd is retraining:
/// drift detected" message. Separate from the systemd [`Notifier`]
/// trait above (which fires `WATCHDOG=1` pings) so the two surfaces
/// can be mocked independently. Production wires
/// [`SystemDriftNotifier`] (shells `notify-send`); tests wire
/// `MockDriftNotifier` to assert the SPEC §5 wording.
pub trait DriftNotifier: Send + Sync {
    /// Fire one desktop notification. The summary is the title; the
    /// body is the human-readable explanation. Implementations MUST
    /// be best-effort — a missing `notify-send` binary or a closed
    /// session bus is not an error worth aborting the tick over.
    fn notify(&self, summary: &str, body: &str);
}

/// Production [`DriftNotifier`] — spawns `notify-send` via
/// `std::process::Command`. The spawn is fire-and-forget: we don't
/// wait on the child, and any spawn error is logged at `debug` so a
/// missing binary doesn't pollute the daemon's tracing output.
#[derive(Debug, Default)]
pub struct SystemDriftNotifier;

impl DriftNotifier for SystemDriftNotifier {
    fn notify(&self, summary: &str, body: &str) {
        match std::process::Command::new("notify-send")
            .arg(summary)
            .arg(body)
            .spawn()
        {
            Ok(mut child) => {
                // Reap immediately so a long-lived child can't pin a
                // zombie slot — the bar already does this dance in
                // `bt.rs` and `vol.rs`.
                let _ = child.wait();
            }
            Err(e) => tracing::debug!(
                target: "sy::power::daemon",
                error = %e,
                "notify-send spawn failed (drift notification dropped)",
            ),
        }
    }
}

/// SPEC §5 verbatim summary for the drift notification. Tests pin
/// the exact wording promised in the Friction Map; production reads
/// the same constant so the human surface can't drift from the
/// machine surface.
pub const DRIFT_NOTIFICATION_SUMMARY: &str = "sy-powerd is retraining: drift detected";

/// Step 29 activity-classifier state held across ticks. Holds the
/// [`OnlineClassifier`] that `one_tick` queries pre-bandit to
/// populate [`crate::power::snapshot::SnapshotRaw::activity_label`]
/// and post-tick to `partial_fit` against any self-supervised label
/// surfaced by [`crate::power::labels::extract_label`].
#[derive(Debug, Default)]
pub struct ActivityTickState {
    pub classifier: OnlineClassifier,
}

impl ActivityTickState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Step P2-3 forecaster state held across ticks. Owns the GRU
/// [`crate::power::forecast::Model`] the daemon's `one_tick` uses
/// to project the next-window activity class, plus the previous
/// tick's forecast probabilities so the drift detector can compute
/// `1.0 if argmax(prev_forecast) != current_activity_label else 0.0`
/// and feed the residual into ADWIN. `last_forecast` is `None` on
/// the daemon's first tick (no prior forecast yet); the drift
/// residual is `0.0` in that case so ADWIN's window stays primed.
pub struct ForecastTickState {
    pub model: crate::power::forecast::Model,
    pub last_forecast: Option<[f32; ACTIVITY_CLASS_COUNT]>,
}

impl ForecastTickState {
    /// Seed from the shipped warmup ONNX (Step 24). The trainer
    /// (Step 25 / P2-1) hot-swaps a trained model in via
    /// [`crate::power::forecast::model::ModelStore`]; production
    /// daemon wires that store into this slot.
    pub fn warmup() -> anyhow::Result<Self> {
        Ok(Self {
            model: crate::power::forecast::Model::warmup()?,
            last_forecast: None,
        })
    }

    /// Startup model load (BUG-20260712-1545 part 3). Prefer a trained
    /// model persisted at `<state_root>/forecaster.onnx` over the
    /// embedded warmup fixture so a retrained forecaster survives a
    /// daemon restart. A missing file falls back to warmup silently
    /// (fresh host); a present-but-corrupt file WARNs once and falls
    /// back to warmup — a bad model on disk must never crash the daemon.
    pub fn load_or_warmup(state_root: &Path) -> anyhow::Result<Self> {
        let path = state_root.join("forecaster.onnx");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::warmup(),
            Err(e) => {
                tracing::warn!(
                    target: "sy::power::daemon",
                    path = %path.display(),
                    error = %e,
                    "cannot read trained forecaster; falling back to warmup model",
                );
                return Self::warmup();
            }
        };
        match crate::power::forecast::Model::from_onnx_bytes(&bytes) {
            Ok(model) => {
                tracing::info!(
                    target: "sy::power::daemon",
                    path = %path.display(),
                    version_sha = %model.version_sha,
                    "loaded trained forecaster from disk",
                );
                Ok(Self {
                    model,
                    last_forecast: None,
                })
            }
            Err(e) => {
                tracing::warn!(
                    target: "sy::power::daemon",
                    path = %path.display(),
                    error = %format!("{e:#}"),
                    "trained forecaster on disk is unreadable; falling back to warmup model",
                );
                Self::warmup()
            }
        }
    }
}

impl BanditTickState {
    /// Construct a fresh bandit state seeded from `cfg.arms`. Step 29
    /// widens the CLUCB context to `FEATURE_LEN + 1` so the bandit
    /// sees the activity-label channel alongside the 12 sensor
    /// features.
    pub fn from_config(cfg: &PowerConfig) -> Self {
        let names: Vec<String> = cfg.arms.iter().map(|a| a.name.clone()).collect();
        Self {
            bandit: for_snapshot_features_with_activity(names, cfg.bandit.alpha as f32),
            last_chosen: None,
        }
    }
}

/// Sanitise a feature vector into a finite contextual-bandit input.
/// `Snapshot::features` start at `f32::NAN`; channels that fail to
/// read leave their slot as NaN. The CLUCB math (`dot`, Cholesky)
/// would propagate NaN into every arm's posterior — silently
/// poisoning every later decision. Replacing NaN/±∞ with 0.0 is the
/// "we observed nothing on this channel" semantic that mirrors
/// `reward::work_proxy`'s missing-value handling.
fn sanitise_context(features: &[f32]) -> Vec<f32> {
    features
        .iter()
        .map(|x| if x.is_finite() { *x } else { 0.0 })
        .collect()
}

/// Step 29: project [`ActivityLabel`] onto `[0.0, 1.0]` so the
/// bandit's 13th context slot stays in the same scale band as the
/// other normalised channels (`ac_online`, `call_active`). Five
/// classes ⇒ `index / (ACTIVITY_CLASS_COUNT − 1)` partitions
/// `{0.0, 0.25, 0.5, 0.75, 1.0}`.
fn activity_label_to_f32(label: ActivityLabel) -> f32 {
    let denom = (ACTIVITY_CLASS_COUNT - 1) as f32;
    label.index() as f32 / denom
}

/// Build the widened bandit context: sanitised sensor features ++
/// activity-label as a normalised f32. The widened slot lands at
/// index `FEATURE_LEN` so existing 12-indexed callers (GRU input,
/// reward math) keep their offsets.
fn sanitise_context_with_activity(features: &[f32], label: ActivityLabel) -> Vec<f32> {
    let mut ctx = sanitise_context(features);
    ctx.push(activity_label_to_f32(label));
    ctx
}

/// Build the bandit's ranked list for this tick, applying the CLUCB
/// conservative anchor: when the bandit's top UCB does not exceed the
/// rules-baseline arm's UCB by more than `cfg.bandit.alpha`, OR the
/// CLUCB lower-confidence-bound floor is not satisfied (Kazerouni
/// 2017's `LCB(top) ≥ baseline_mean − α` invariant), prepend the
/// rules-baseline arm so `shield::project` walks it first. This
/// realises the SPEC §2 deep-dive promise that the rules baseline is
/// the conservative anchor, not a competing proposer — the bandit
/// must earn the right to deviate.
fn ranked_actions_for_tick(
    bandit: &Clucb,
    context: &[f32],
    baseline_arm: &str,
    cfg: &PowerConfig,
) -> Vec<(String, f32)> {
    let ranked = bandit.propose_ranked(context);
    if ranked.is_empty() {
        return Vec::new();
    }
    let top_arm = &ranked[0].0;
    let top_score = ranked[0].1;
    let baseline_score = ranked
        .iter()
        .find(|(n, _)| n == baseline_arm)
        .map(|(_, s)| *s)
        .unwrap_or(top_score);
    let alpha = cfg.bandit.alpha as f32;
    let margin_clear = (top_score - baseline_score) >= alpha;
    let floor_ok = bandit.baseline_floor_satisfied(top_arm, context);
    if margin_clear && floor_ok {
        return ranked;
    }
    // Conservative anchor: rules baseline must lead.
    let mut anchored = Vec::with_capacity(ranked.len());
    anchored.push((baseline_arm.to_string(), baseline_score));
    anchored.extend(ranked.into_iter().filter(|(n, _)| n != baseline_arm));
    anchored
}

/// Look the typed `Arm` up from `cfg.arms`. Returns a degenerate
/// fallback when the name is missing so the projector + actuators
/// still see a typed value — never crash on a misconfigured arm
/// table.
fn arm_by_name(cfg: &PowerConfig, name: &str) -> Arm {
    cfg.arms
        .iter()
        .find(|a| a.name == name)
        .cloned()
        .unwrap_or_else(|| degenerate_arm(name))
}

/// Degenerate fallback arm when the configured baseline name itself
/// is missing from `cfg.arms`. The tuple matches `shield::project`'s
/// `fallback_arm` so the audit entry still names a real-looking arm
/// even on a deeply misconfigured power.toml.
fn degenerate_arm(name: &str) -> Arm {
    use crate::power::bandit::{CgroupOverrides, Epp, NpuPmode};
    use crate::power::sensors::igpu::IgpuProfileMode;
    use crate::power::sensors::platform::PlatformProfile;
    Arm {
        name: name.to_string(),
        platform_profile: PlatformProfile::Quiet,
        epp: Epp::Power,
        igpu_mode: IgpuProfileMode::Other("POWER_SAVING".into()),
        npu_pmode: NpuPmode::Powersaver,
        cgroup: CgroupOverrides::default(),
    }
}

/// Apply every actuator for `arm`. Failures are logged + collected
/// into the returned reason chain so the audit entry records exactly
/// which knobs were written (and which were skipped). NPU writes are
/// best-effort — `xrt-smi` may be absent, the device may be offline,
/// the firmware may refuse a transition; we downgrade the error to a
/// `tracing::warn!` per SPEC §4 "NPU lever is best-effort".
fn apply_arm(
    arm: &Arm,
    ctx: &TickContext<'_>,
    latches: &mut ActuatorLatches,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    apply_lever(
        "platform_profile",
        latches.lever("platform_profile"),
        now,
        &mut reasons,
        || PlatformProfileActuator::new().apply(arm.platform_profile.clone(), &ctx.sysfs_root),
    );
    apply_lever("epp", latches.lever("epp"), now, &mut reasons, || {
        EppActuator::new().apply(arm.epp, &ctx.sysfs_root)
    });
    apply_lever("igpu", latches.lever("igpu"), now, &mut reasons, || {
        IgpuActuator::new().apply(arm.igpu_mode.clone(), &ctx.sysfs_root)
    });
    apply_lever("npu", latches.lever("npu"), now, &mut reasons, || {
        ctx.npu.apply(arm.npu_pmode, &ctx.sysfs_root)
    });
    apply_lever("cgroup", latches.lever("cgroup"), now, &mut reasons, || {
        CgroupActuator::new().apply(arm.cgroup.clone(), &ctx.cgroup_root)
    });
    reasons
}

/// Render an `Applied` outcome as a short audit-line token.
fn outcome_summary(out: &Applied) -> String {
    match out {
        Applied::Wrote { value, .. } => format!("wrote={value}"),
        Applied::NoChange => "no-change".into(),
    }
}

/// Maximum length of the `npu: skipped (...)` reason token written to
/// the audit log. Picked so the worst-case NPU failure does not
/// dominate an audit line: at ~700 B/entry total budget, 160 B leaves
/// room for the snapshot, ranked actions, and other reasons.
const NPU_REASON_MAX_LEN: usize = 160;

/// Collapse an NPU actuator error into a single bounded audit token.
/// The full `xrt-smi` stderr (build banner, PID, host, exe path,
/// multi-line ERROR block) is fine for `tracing::warn!` — operators
/// have journalctl — but copying it verbatim into every audit entry
/// bloats the NDJSON from ~700 B/entry to ~1.3 KB/entry, which used
/// to cap the daily file at ~40K entries (well below the 86,400
/// thick-report threshold).
///
/// Strategy:
/// 1. Prefer the first line containing `ERROR`/`error:`/`FATAL` —
///    that's where `xrt-smi` puts the actual failure after its build
///    banner.
/// 2. Otherwise, use the first non-empty line.
/// 3. Cap at [`NPU_REASON_MAX_LEN`] chars (char-boundary safe) with
///    an ellipsis if we had to chop.
fn short_npu_reason<E: std::fmt::Display>(err: &E) -> String {
    let full = err.to_string();
    let pick = full
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find(|l| l.contains("ERROR") || l.contains("error:") || l.contains("FATAL"))
        .or_else(|| full.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("");
    let mut out = format!("npu: skipped ({pick})");
    if out.len() > NPU_REASON_MAX_LEN {
        let mut end = NPU_REASON_MAX_LEN.saturating_sub(1);
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push('…');
    }
    out
}

/// Apply one lever through its [`LeverLatch`], appending exactly one
/// reason-chain token per tick and emitting at most one journal line
/// on the failure/recovery *edges* only.
///
/// BUG-20260712-* Problem B: a persistently failing actuator (the iGPU
/// one on this host) used to retry + WARN every 1 Hz tick forever. The
/// latch WARNs once on entry, skips the sysfs write while backing off
/// (exponential, capped 60 s), and logs once on recovery — while the
/// reason chain still records the lever state every tick (cheap) so
/// `sy power explain` never loses the per-tick trail.
fn apply_lever(
    lever: &'static str,
    latch: &mut LeverLatch,
    now: chrono::DateTime<chrono::Utc>,
    reasons: &mut Vec<String>,
    attempt: impl FnOnce() -> anyhow::Result<Applied>,
) {
    match latch.step(now, attempt) {
        LatchOutcome::Ok(o) => reasons.push(format!("{lever}: {}", outcome_summary(&o))),
        LatchOutcome::Recovered(o) => {
            tracing::info!(target: "sy::power::daemon", lever, "actuator recovered");
            reasons.push(format!("{lever}: {}", outcome_summary(&o)));
        }
        LatchOutcome::Failed(e) => {
            warn_actuator_failed(lever, &e);
            reasons.push(failure_token(lever, &e));
        }
        LatchOutcome::StillFailed(e) => {
            // Silent: the entry WARN already fired. The reason chain
            // still names the failure so the audit trail is intact.
            reasons.push(failure_token(lever, &e));
        }
        LatchOutcome::Skipped { backoff_secs } => {
            reasons.push(format!(
                "{lever}: latched-failed (retry in {backoff_secs}s)"
            ));
        }
    }
}

/// Emit the single failure-edge WARN for `lever`.
///
/// BUG-20260525-2350: when the EPP actuator returns the structured
/// `NoPolicyWritable` variant, the WARN line also carries
/// `failed_policies=<n>` + a comma-joined `failed_paths=…` field so the
/// operator can run `systemd-tmpfiles --create` against exactly the
/// leaves that need it without grep-and-trim. The NPU lever stays
/// best-effort (SPEC §4) — its WARN is a plain one-liner.
fn warn_actuator_failed(lever: &str, e: &anyhow::Error) {
    if lever == "npu" {
        tracing::warn!(target: "sy::power::daemon", lever, error = %e, "npu apply failed (best-effort; latched, backing off)");
        return;
    }
    if let Some(apply::epp::EppError::NoPolicyWritable { failed }) =
        e.downcast_ref::<apply::epp::EppError>()
    {
        let failed_paths = failed
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(",");
        tracing::warn!(
            target: "sy::power::daemon",
            lever,
            error = %e,
            failed_policies = failed.len(),
            failed_paths = %failed_paths,
            "actuator failed (latched, backing off)",
        );
    } else {
        tracing::warn!(
            target: "sy::power::daemon",
            lever,
            error = %e,
            "actuator failed (latched, backing off)",
        );
    }
}

/// Render the reason-chain token for a failed lever. NPU keeps its
/// bounded `npu: skipped (…)` form ([`short_npu_reason`]); the four
/// sysfs levers use the compact `<lever>: skipped (<err>)` form.
fn failure_token(lever: &str, e: &anyhow::Error) -> String {
    if lever == "npu" {
        short_npu_reason(e)
    } else {
        format!("{lever}: skipped ({e})")
    }
}

/// One sensor+intent tick: read every sensor, drain every intent
/// channel, run the snapshot through shield → resolve → project →
/// apply, and append the resulting [`AuditEntry`] to the NDJSON log.
/// Publishes the snapshot to `latest` and the audit entry to
/// `last_entry` for IPC consumers.
///
/// Hermetic by construction — `sysfs_root` + `cgroup_root` are path
/// arguments so tests point them at tempdirs instead of `/sys` and
/// `/sys/fs/cgroup`. Returns the snapshot (for tests / future
/// telemetry) and the logger's [`LogError`] if the audit-log append
/// refused; actuator failures are recorded in the reason chain but
/// never propagate.
#[allow(clippy::too_many_arguments)]
pub fn one_tick(
    sensors: &Sensors,
    intent: &mut Intent,
    clock: &dyn Clock,
    ctx: &TickContext<'_>,
    pin: &LatestPin,
    shield_state: &mut ShieldTickState,
    bandit_state: &mut BanditTickState,
    onboarding_state: &mut OnboardingTickState,
    activity_state: &mut ActivityTickState,
    drift_state: &mut DriftTickState,
    forecast_state: &mut ForecastTickState,
    latches: &mut ActuatorLatches,
    drift_notifier: &dyn DriftNotifier,
    retrain_trigger: &dyn RetrainTrigger,
    logger: &Logger,
    latest: &LatestSnapshot,
    last_entry: &LatestAuditEntry,
    drift_latest: &LatestDriftStatus,
) -> Result<Snapshot, LogError> {
    let mut snap = snapshot::collect_tick(sensors, intent, clock, &ctx.sysfs_root);
    // Step 29: classify the freshly-collected snapshot, stamp the
    // resulting label onto `raw.activity_label` so downstream
    // consumers (audit log, `sy power explain`, bandit context) see
    // the same scoring the daemon used.
    let label = activity_state.classifier.classify(&snap);
    snap.raw.activity_label = Some(label);
    // Step P2-3: run the GRU forecaster against the current feature
    // vector, stamp the resulting 5-class distribution onto
    // `raw.activity_forecast`. Failures fall back to `None` so a
    // misshapen feature window (e.g. a future schema bump) degrades
    // gracefully instead of crashing the tick.
    let forecast_probs = run_forecast_for_tick(forecast_state, &snap);
    snap.raw.activity_forecast = forecast_probs;
    // Step P2-3: feed `1.0 if argmax(prev_forecast) != label else 0.0`
    // into ADWIN. First tick has no prior forecast → residual = 0.0
    // (keeps the window primed without alarming on initial noise).
    let residual = forecast_residual(forecast_state.last_forecast, label);
    observe_forecast_drift(drift_state, drift_notifier, clock, residual);
    forecast_state.last_forecast = forecast_probs;
    if let Ok(mut g) = latest.write() {
        *g = Some(snap.clone());
    }
    // BUG-20260712-1201: track the last tick `call_active` was true so
    // the MEETING lock ages off a real timestamp. When the call is
    // live this tick, `secs_since_call` is 0 (the `call_active ||`
    // branch pins MEETING anyway); after it ends the elapsed seconds
    // grow until the DFA releases MEETING past the lock window.
    if snap.raw.call_active == Some(true) {
        shield_state.last_call_at = Some(snap.ts);
    }
    let secs_since_call = shield_state
        .last_call_at
        .map(|t| (snap.ts - t).num_milliseconds() as f32 / 1000.0);
    let state = shield::transition(shield_state.prev, &snap, &ctx.cfg.shield, secs_since_call);
    let context = sanitise_context_with_activity(&snap.features, label);
    let onboarding_active = onboarding_state
        .status
        .as_ref()
        .map(|s| s.active)
        .unwrap_or(true);

    // Reward update for the *previous* tick's arm pick. While
    // onboarding is active the bandit's posterior is held frozen —
    // we still drain `last_chosen` (so it doesn't accumulate stale
    // pre-onboarding context) but skip the `update` + `observe_baseline`
    // calls so day-15 starts from the same posterior day-14 ended at.
    // Step 31: a side-effect of the reward computation is the DDM
    // input — the absolute residual against the running mean. The
    // ADWIN side reads the forecast residual; Step 29b will populate
    // it from the live forecast vs the realised activity label, but
    // until that lands we feed `0.0` so the detector remains primed
    // without alarming on noise.
    if let Some(prev) = bandit_state.last_chosen.take() {
        if !onboarding_active {
            let r = compute_reward(
                &prev.snapshot,
                &snap,
                &prev.arm,
                prev.prev_arm.as_deref(),
                &ctx.cfg.reward,
            );
            bandit_state.bandit.update(&prev.arm, &prev.context, r);
            bandit_state.bandit.observe_baseline(r);
            observe_drift_signals(drift_state, drift_notifier, clock, r);
            tracing::debug!(
                target: "sy::power::daemon",
                arm = %prev.arm,
                reward = r,
                baseline_mean = bandit_state.bandit.baseline_mean(),
                "bandit posterior updated",
            );
        }
    }
    let drift_alarm = drift_state.status.adwin_alarm;
    publish_drift_status(drift_latest, &drift_state.status);

    let baseline_name = rules_baseline(state, &snap, &ctx.cfg.rules_baseline).to_string();
    let pinned = pin.read().ok().and_then(|g| g.clone());
    let (ranked_pairs, source_label) = match pinned.as_deref() {
        Some(name) if ctx.cfg.arms.iter().any(|a| a.name == name) => (
            vec![(name.to_string(), f32::INFINITY)],
            format!("pin:{name}"),
        ),
        // Step 31: drift alarm forces the rules baseline just like
        // onboarding does. The reason-label is distinct so the audit
        // log + `sy power explain` can attribute the degradation
        // correctly.
        _ if onboarding_active => (
            vec![(baseline_name.clone(), 0.0)],
            format!("onboarding-baseline:{baseline_name}"),
        ),
        _ if drift_alarm => (
            vec![(baseline_name.clone(), 0.0)],
            format!("drift-baseline:{baseline_name}"),
        ),
        _ => {
            let pairs =
                ranked_actions_for_tick(&bandit_state.bandit, &context, &baseline_name, ctx.cfg);
            let top = pairs.first().map(|(n, _)| n.clone()).unwrap_or_default();
            let score = pairs.first().map(|(_, s)| *s).unwrap_or(0.0);
            (pairs, format!("bandit:{top} (ucb={score:.2})"))
        }
    };

    let arms_typed: Vec<Arm> = ranked_pairs
        .iter()
        .map(|(n, _)| arm_by_name(ctx.cfg, n))
        .collect();
    // BUG-20260712-1136: an operator pin (`sy power profile <arm>`) must
    // actuate regardless of the anti-thrash floor. `project_forced`
    // bypasses the `would_thrash` veto for the pinned singleton while
    // keeping the shield safety constraints; the bandit path keeps the
    // oscillation floor via `project`.
    let now_instant = Instant::now();
    let chosen = if pinned.is_some() {
        shield::project_forced(&arms_typed, state, &snap, ctx.cfg, ctx.thrash, now_instant)
    } else {
        shield::project(&arms_typed, state, &snap, ctx.cfg, ctx.thrash, now_instant)
    };

    let mut reason_chain = vec![source_label, format!("shield:{}", state.as_str())];
    reason_chain.extend(apply_arm(&chosen, ctx, latches, clock.now()));

    let top3: Vec<(String, f32)> = ranked_pairs.iter().take(3).cloned().collect();
    let prev_arm_name = bandit_state
        .last_chosen
        .as_ref()
        .map(|p| p.arm.clone())
        .or_else(|| {
            last_entry
                .read()
                .ok()
                .and_then(|g| g.as_ref().and_then(|e| e.applied_arm.clone()))
        });
    let entry = AuditEntry::r3(
        snap.clone(),
        chosen.name.clone(),
        state.as_str().to_string(),
        reason_chain,
        top3,
        ctx.cfg.bandit.alpha as f32,
    );
    if let Ok(mut g) = last_entry.write() {
        *g = Some(entry.clone());
    }
    logger.append(&entry, clock)?;

    // Step 29: feed any self-supervised label surfaced by the audit
    // entry (today only the `pin:<arm>` manual-override path; the
    // throttle/drain-residual paths land in Step 31+) back into the
    // classifier. Weight is always +1.0 for a manual pin so we only
    // gate on Some/None — the sign carried by `extract_label` is
    // future-proofing.
    if let Some((true_label, weight)) = crate::power::labels::extract_label(&entry) {
        if weight > 0.0 {
            activity_state.classifier.partial_fit(&snap, true_label);
        }
    }

    bandit_state.last_chosen = Some(LastChosen {
        arm: chosen.name.clone(),
        snapshot: snap.clone(),
        context,
        prev_arm: prev_arm_name,
    });
    shield_state.prev = state;
    // Step 31: drift bypasses the onboarding gate so a mid-life
    // alarm can fire a retrain even after day 14. The gate
    // semantically becomes "are we ALREADY in the rules-only path
    // for a non-drift reason?" — if so, the retrain stays under the
    // onboarding cadence; if not, the AC/idle/SOC gates decide.
    let retrain_onboarding_gate = onboarding_active && !drift_alarm;
    let outcome = evaluate_retrain_trigger(
        &snap,
        retrain_onboarding_gate,
        onboarding_state.last_retrain_at,
        clock.now(),
    );
    match outcome {
        RetrainOutcome::Dispatched => {
            let cause = if drift_alarm {
                RetrainCause::Drift
            } else {
                RetrainCause::Onboarding
            };
            retrain_trigger.dispatch(cause);
            onboarding_state.last_retrain_at = Some(clock.now());
            // Step 31: clear the drift state on a drift-driven
            // dispatch so the daemon hot-swaps back to bandit
            // control on the next tick. The detector + status are
            // both reset; `last_alarm_at` is preserved on the
            // status block so the operator can still see when the
            // most recent alarm fired.
            if drift_alarm {
                clear_drift_state(drift_state, drift_latest);
            }
            tracing::info!(
                target: "sy::power::daemon",
                ?cause,
                "retrain scheduler dispatched (AC + idle + SOC gates open)",
            );
        }
        RetrainOutcome::Skipped(reason) => tracing::trace!(
            target: "sy::power::daemon",
            ?reason,
            "retrain scheduler skipped",
        ),
    }
    Ok(snap)
}

/// Observe one bandit-reward sample on both drift detectors. Pure
/// over `(drift_state, clock, reward)` — the only side effects are
/// updates to `drift_state.{detector, reward_mean, reward_n, status}`
/// and (on a fresh alarm) one `drift_notifier.notify(…)` call.
/// Extracted so the test path can pin the alarm contract without
/// re-driving `one_tick`.
fn observe_drift_signals(
    drift_state: &mut DriftTickState,
    drift_notifier: &dyn DriftNotifier,
    clock: &dyn Clock,
    reward: f32,
) {
    // Maintain the running reward mean; this is the DDM input. The
    // residual is binarised against `DRIFT_REWARD_RESIDUAL_THRESH`
    // so "small drift in mean" surfaces as a steady stream of
    // errors.
    let r = if reward.is_finite() { reward } else { 0.0 };
    let prev_mean = drift_state.reward_mean;
    drift_state.reward_n = drift_state.reward_n.saturating_add(1);
    let n = drift_state.reward_n as f32;
    drift_state.reward_mean = prev_mean + (r - prev_mean) / n;
    let residual = (r - prev_mean).abs();
    let ddm_in = residual > DRIFT_REWARD_RESIDUAL_THRESH;
    let ddm_signal = drift_state.detector.reward.observe(ddm_in);
    let was_alarm = drift_state.status.adwin_alarm;
    drift_state.status.ddm_warning = matches!(ddm_signal, DriftSignal::Warning);
    if matches!(ddm_signal, DriftSignal::Alarm) {
        drift_state.status.adwin_alarm = true;
        drift_state.status.last_alarm_at = Some(clock.now());
        if !was_alarm {
            drift_notifier.notify(
                DRIFT_NOTIFICATION_SUMMARY,
                "Power daemon dropped to rules-only; will retrain on the next idle+plugged window.",
            );
        }
    }
}

/// Step P2-3: observe one forecast-residual sample on the ADWIN
/// detector. Called once per tick (independent of the reward update
/// path) so the forecast residual is always fed — even during
/// onboarding when the bandit's posterior is frozen. The alarm path
/// mirrors [`observe_drift_signals`]: on the rising edge of a fresh
/// ADWIN alarm the desktop notifier fires exactly once.
fn observe_forecast_drift(
    drift_state: &mut DriftTickState,
    drift_notifier: &dyn DriftNotifier,
    clock: &dyn Clock,
    residual: f32,
) {
    let adwin_signal = drift_state.detector.forecast.observe(residual);
    let was_alarm = drift_state.status.adwin_alarm;
    if matches!(adwin_signal, DriftSignal::Alarm) {
        drift_state.status.adwin_alarm = true;
        drift_state.status.last_alarm_at = Some(clock.now());
        if !was_alarm {
            drift_notifier.notify(
                DRIFT_NOTIFICATION_SUMMARY,
                "Power daemon dropped to rules-only; will retrain on the next idle+plugged window.",
            );
        }
    }
}

/// Step P2-3: run the GRU forecaster against the current snapshot's
/// 12-channel feature window. Returns `None` if the model rejects
/// the input shape (defensive — the schema is pinned but a future
/// trained model could disagree). On success the `[f32; 5]`
/// probability vector is returned for stamping onto `raw`.
fn run_forecast_for_tick(
    state: &ForecastTickState,
    snap: &Snapshot,
) -> Option<[f32; ACTIVITY_CLASS_COUNT]> {
    match crate::power::forecast::gru::infer(&state.model, &snap.features) {
        Ok(f) => probs_to_array(&f.class_probs),
        Err(e) => {
            tracing::debug!(
                target: "sy::power::daemon",
                error = %e,
                "forecast infer failed; activity_forecast stays None this tick",
            );
            None
        }
    }
}

/// Project a `Vec<(class_name, prob)>` returned by
/// [`crate::power::forecast::gru::infer`] into the pinned
/// `[idle, browse, call, code, build]` slot order. Returns `None`
/// if the vector is the wrong length — should never happen for the
/// shipped GRU but kept as a guard against trainer-side regressions.
fn probs_to_array(pairs: &[(String, f32)]) -> Option<[f32; ACTIVITY_CLASS_COUNT]> {
    if pairs.len() != ACTIVITY_CLASS_COUNT {
        return None;
    }
    let mut out = [0.0_f32; ACTIVITY_CLASS_COUNT];
    for (i, (_name, p)) in pairs.iter().enumerate() {
        out[i] = *p;
    }
    Some(out)
}

/// Step P2-3: compute the forecast residual against the realised
/// activity label. `1.0` if `argmax(prev_forecast) != label.index()`,
/// `0.0` otherwise. `prev_forecast = None` (the first daemon tick)
/// returns `0.0` so ADWIN's window stays primed without alarming on
/// the missing-prior boundary case.
fn forecast_residual(
    prev_forecast: Option<[f32; ACTIVITY_CLASS_COUNT]>,
    label: ActivityLabel,
) -> f32 {
    let Some(prev) = prev_forecast else {
        return 0.0;
    };
    let predicted = argmax(&prev);
    if predicted == label.index() {
        0.0
    } else {
        1.0
    }
}

/// Index of the maximum value in a fixed-width probability vector.
/// Ties go to the smallest index (mirrors `ActivityLabel`'s argmax
/// convention).
fn argmax(probs: &[f32; ACTIVITY_CLASS_COUNT]) -> usize {
    let mut best = 0_usize;
    let mut best_v = probs[0];
    for (i, p) in probs.iter().enumerate().skip(1) {
        if *p > best_v {
            best = i;
            best_v = *p;
        }
    }
    best
}

/// Threshold above which a reward residual counts as an "error" for
/// DDM. The reward is bounded by `compute_reward` in roughly the
/// `[-1, 1]` range, so 0.25 catches a meaningful deviation from the
/// running mean without firing on the natural per-tick jitter.
pub const DRIFT_REWARD_RESIDUAL_THRESH: f32 = 0.25;

/// Mirror the daemon's drift bookkeeping into the shared
/// `LatestDriftStatus` slot. Best-effort: a poisoned lock is logged
/// but never aborts the tick.
fn publish_drift_status(slot: &LatestDriftStatus, status: &DriftStatus) {
    if let Ok(mut g) = slot.write() {
        *g = status.clone();
    }
}

/// Reset the drift detector + alarm state after a successful retrain
/// dispatch. The `last_alarm_at` slot is preserved so an operator
/// can see when the most recent alarm fired; everything else returns
/// to defaults.
fn clear_drift_state(drift_state: &mut DriftTickState, slot: &LatestDriftStatus) {
    drift_state.detector.reset();
    drift_state.reward_mean = 0.0;
    drift_state.reward_n = 0;
    let preserved = drift_state.status.last_alarm_at;
    drift_state.status = DriftStatus {
        adwin_alarm: false,
        ddm_warning: false,
        last_alarm_at: preserved,
    };
    publish_drift_status(slot, &drift_state.status);
}

/// Construct a [`NpuActuator`] suitable for the production daemon —
/// shells out via `xrt-smi` through [`apply::SystemRunner`]. The
/// daemon-in-thread tests construct their own with a no-op runner.
pub fn production_npu_actuator() -> NpuActuator {
    NpuActuator::new_cached(
        Box::new(apply::SystemRunner::new()),
        Box::new(apply::SystemTimeSource::new()),
        &super::power_state_dir_for_daemon(),
    )
}

/// sy-mon Step 20: install the power plane's Prometheus UDS exporter
/// at `$XDG_RUNTIME_DIR/sy/power/metrics.sock`. Must be called from
/// inside the powerd tokio runtime so the shared installer's accept
/// task lands on the right runtime.
#[cfg(feature = "mon-exporter")]
async fn install_power_mon_exporter() -> anyhow::Result<sy_core::obs::mon_exporter::UdsGuard> {
    let path = crate::mon_exporter::socket_path_for("power")?;
    let guard = sy_core::obs::mon_exporter::install(path.clone())
        .map_err(|e| anyhow::anyhow!("install power mon-exporter at {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(guard.path()) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(guard.path(), perms);
        }
    }
    tracing::info!(
        target: "sy::power::daemon",
        path = %guard.path().display(),
        "power mon-exporter bound"
    );
    Ok(guard)
}

/// `sy-powerd` entrypoint dispatched from `cli::dispatch(PowerCmd::Daemon)`.
///
/// Step 10 scope (no actuation):
///
/// 1. Build a current-thread tokio runtime (1 Hz workload — multi-thread
///    overhead isn't earned until R2's bandit lands).
/// 2. Bind the IPC socket at `$XDG_RUNTIME_DIR/sy/powerd.sock`.
/// 3. `sd_notify(READY=1)` after the bind, then spawn the watchdog
///    pinger via `sy_core::notify::spawn_watchdog`.
/// 4. Tick at 1 Hz: `one_tick` reads sensors+intent, appends to the
///    NDJSON log, publishes the snapshot to the IPC `RwLock`.
/// 5. Accept IPC connections concurrently; each one handles a single
///    `StatusRequest` frame.
///
/// SIGTERM tears the runtime down cleanly. The vendor-default exit
/// handler from SPEC §4 NFR Reliability is deferred to Step 19 (R1
/// has no actuation to revert).
pub fn run() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async())
}

async fn run_async() -> anyhow::Result<()> {
    use std::time::Duration;
    use tokio::net::UnixListener;

    let sock = socket_path();
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(&sock);
    let listener =
        UnixListener::bind(&sock).map_err(|e| anyhow::anyhow!("bind {}: {e}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&sock) {
            let mut p = meta.permissions();
            p.set_mode(0o600);
            let _ = std::fs::set_permissions(&sock, p);
        }
    }
    tracing::info!(target: "sy::power::daemon", socket = %sock.display(), "sy-powerd listening");

    // sy-mon Step 20: bind the power plane's Prometheus UDS at
    // `$XDG_RUNTIME_DIR/sy/power/metrics.sock`. Direct install on the
    // current tokio runtime; the returned guard lives for the
    // daemon's lifetime and unlinks the socket on Drop. Bind failure
    // is non-fatal — `sy mon doctor` (Step 21) is the alarm surface.
    // The roadmap step lists `src/power/cli.rs` for this wiring but
    // the actual daemon entrypoint is here (`power/daemon.rs::run`);
    // wiring it in `cli.rs` would attach the exporter to the short-
    // lived `sy power` CLI dispatcher rather than the long-lived
    // `sy-powerd` process the aggregator actually scrapes.
    #[cfg(feature = "mon-exporter")]
    let _mon_exporter = match install_power_mon_exporter().await {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!(
                target: "sy::power::daemon",
                error = %format!("{e:#}"),
                "power mon-exporter failed to bind; continuing without metrics socket"
            );
            None
        }
    };

    // SPEC §4 NFR Reliability: install a panic hook that writes the
    // vendor defaults synchronously before the panic propagates. The
    // `Drop` impl on [`CrashSafeGuard`] is the SIGTERM / clean-exit
    // counterpart — both share `apply::crash_safe_exit_defaults`.
    install_panic_hook(PathBuf::from("/sys"));
    let _crash_safe_guard = CrashSafeGuard::new(PathBuf::from("/sys"));

    sy_core::notify::ready();
    let _watchdog = spawn_watchdog_thread(SystemNotifier);

    let logger = Logger::new(super::power_state_dir_for_daemon());
    let _ = logger.rotate_retention(&crate::power::clock::SystemClock);
    let sensors = Sensors::all();
    let mut intent = build_live_intent();
    let latest = new_latest_snapshot();
    let pin = new_pin_slot();
    let last_entry = new_latest_audit_entry();
    let drift_latest = new_latest_drift_status();
    let model_latest = new_latest_model_status();
    let onboarding_latest = new_latest_onboarding();
    let cfg = PowerConfig::load(&super::power_config_path()).unwrap_or_default();

    // S3 guard rail (BUG-20260712-0139): a telemetry retention horizon
    // shorter than the onboarding window used to structurally deadlock
    // the onboarding gate — the retention sweep deleted the telemetry
    // the day-14 gate needed. The persisted `first_telemetry_at` anchor
    // now keeps `days_collected` honest regardless, but a short
    // retention still starves the raw telemetry an operator (or the
    // trainer) may want, so surface it loudly at startup.
    if let Some(msg) =
        crate::power::onboarding::retention_guard(logger.retention_days(), cfg.onboarding.days)
    {
        tracing::warn!(target: "sy::power::daemon", "{msg}");
    }

    let thrash = Arc::new(ThrashTracker::new());
    let npu_actuator = Arc::new(production_npu_actuator());

    // Concurrent IPC accept loop. Each accepted connection handles a
    // single request frame (Status, ProfileSet, ProfileClear).
    let accept_latest = Arc::clone(&latest);
    let accept_pin = Arc::clone(&pin);
    let accept_last_entry = Arc::clone(&last_entry);
    let accept_drift = Arc::clone(&drift_latest);
    let accept_model = Arc::clone(&model_latest);
    let accept_onboarding = Arc::clone(&onboarding_latest);
    let accept_arms = cfg.arms.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let state = ConnState {
                        latest: Arc::clone(&accept_latest),
                        pin: Arc::clone(&accept_pin),
                        last_entry: Arc::clone(&accept_last_entry),
                        drift: Arc::clone(&accept_drift),
                        model: Arc::clone(&accept_model),
                        onboarding: Arc::clone(&accept_onboarding),
                        arms: accept_arms.clone(),
                    };
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection_full(stream, state).await {
                            tracing::debug!(
                                target: "sy::power::daemon",
                                error = %e,
                                "ipc connection ended with error"
                            );
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        target: "sy::power::daemon",
                        error = %e,
                        "powerd accept error"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });

    // Phase R7 / Step 36 — PPD D-Bus shim. Bind
    // `net.hadess.PowerProfiles` on the system bus so GNOME's
    // quick-settings tile maps `power-saver`/`balanced`/`performance`
    // onto `sy power`'s `idle`/`code`/`build` arms. The shim mutates
    // the same `pin` slot the rest of the daemon already honours, so
    // no additional plumbing is required.
    //
    // Step 37: `SY_POWER_WITH_PPD=1` flips the shim into co-existence
    // mode — PPD keeps the bus name; the shim thread parks idle so
    // there is no startup bus-name fight. The installer sets this env
    // pin when `sy power apply --with-ppd` is invoked (via the systemd
    // user unit's `Environment=` directive in future drops; today the
    // operator sets it before `systemctl --user start sy-powerd`).
    let with_ppd = std::env::var("SY_POWER_WITH_PPD")
        .map(|v| v == "1")
        .unwrap_or(false);
    let _ppd_shim =
        super::ppd_shim::spawn_system_bus_shim(Arc::clone(&pin), cfg.arms.clone(), !with_ppd);

    // SIGTERM / SIGINT: signal the tick loop to break, then the loop
    // itself emits `STOPPING=1`, persists the checkpoint, writes
    // vendor defaults, drops the socket, and returns. We use
    // `tokio::sync::Notify` over the prior `std::process::exit(0)`
    // because the tick loop owns the `&mut bandit_state` /
    // `&mut activity_state` it needs to snapshot — process-exit from
    // a sibling task would skip the save (BUG-20260525-2353).
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let shutdown_signal = Arc::clone(&shutdown_notify);
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM");
        let mut intr = signal(SignalKind::interrupt()).expect("install SIGINT");
        tokio::select! {
            _ = term.recv() => {},
            _ = intr.recv() => {},
        }
        shutdown_signal.notify_waiters();
    });

    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let clock = crate::power::clock::SystemClock;
    let mut shield_state = ShieldTickState::new();
    let mut bandit_state = BanditTickState::from_config(&cfg);
    let cgroup_root = production_cgroup_root();
    let state_root = super::power_state_dir_for_daemon();
    let mut onboarding_state = OnboardingTickState::new();
    let mut activity_state = ActivityTickState::new();
    let mut drift_state = DriftTickState::new();
    // Step P2-3: seed the forecaster with the shipped warmup ONNX —
    // identical to Step 24's cold-start path. The trainer's retrain
    // hook (Step 25 / P2-1) hot-swaps a trained model in via
    // [`crate::power::forecast::model::ModelStore`] in a later step.
    // BUG-20260712-1545 part 3: prefer a trained `forecaster.onnx` on
    // disk over the embedded warmup so a retrained model survives a
    // daemon restart; a missing/corrupt file falls back to warmup.
    let mut forecast_state = ForecastTickState::load_or_warmup(&state_root)
        .map_err(|e| anyhow::anyhow!("warmup forecaster: {e}"))?;
    // BUG-20260712-* Problem B: per-lever failure latches persist across
    // ticks so a persistently failing actuator WARNs once + backs off
    // instead of spamming the journal every second.
    let mut actuator_latches = ActuatorLatches::default();
    let drift_notifier = SystemDriftNotifier;
    let retrain_trigger =
        SpawnBlockingRetrainTrigger::for_state_root(&state_root, Arc::clone(&model_latest));

    // T4: rehydrate the bandit posterior + classifier weights from
    // `~/.local/state/sy/power/checkpoint.json` if a matching
    // checkpoint exists. Schema or arms-hash mismatch surfaces as
    // `Ok(None)` and the file is rotated to `.stale-<ts>` by
    // `checkpoint::load` — the daemon then continues with the fresh
    // zero-init state. A genuine io error (permission denied, disk
    // failure) is logged + dropped so the daemon still boots.
    let ck_path = state_root.join("checkpoint.json");
    let ck_arms_hash = checkpoint::arms_hash(&cfg.arms);
    // S3 onboarding anchor (BUG-20260712-0139). Loaded from the
    // checkpoint when present; `None` on a fresh host OR after an
    // arms-hash rotation (`checkpoint::load` returns `Ok(None)` then,
    // which resets the anchor by design). When `None`, the tick loop
    // re-derives it from the oldest surviving NDJSON entry and persists
    // it so `days_collected` stops sliding under the retention sweep.
    let mut first_telemetry_at: Option<chrono::DateTime<chrono::Utc>> = None;
    match checkpoint::load(&ck_path, ck_arms_hash) {
        Ok(Some(ck)) => {
            let bandit_arms = ck.bandit.arms.len();
            let classifier_classes = ck.classifier_class_count();
            let saved_at = ck.saved_at;
            first_telemetry_at = ck.first_telemetry_at;
            bandit_state.bandit.restore(ck.bandit);
            activity_state.classifier.restore(ck.classifier);
            tracing::info!(
                target: "sy::power::daemon",
                bandit_arms,
                classifier_classes,
                saved_at = %saved_at,
                path = %ck_path.display(),
                "checkpoint hydrated",
            );
        }
        Ok(None) => {
            tracing::info!(
                target: "sy::power::daemon",
                path = %ck_path.display(),
                "no usable checkpoint; bandit + classifier start from zero",
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "sy::power::daemon",
                error = %e,
                path = %ck_path.display(),
                "checkpoint load failed; continuing with fresh state",
            );
        }
    }

    let mut tick_count: u64 = 0;
    let shutdown_wait = Arc::clone(&shutdown_notify);
    loop {
        // Race the next 1 Hz tick against the shutdown notify. On
        // shutdown, persist the checkpoint inside the same task that
        // owns the live bandit/classifier state, then break to the
        // graceful-exit path below.
        let shutdown_fut = shutdown_wait.notified();
        tokio::pin!(shutdown_fut);
        tokio::select! {
            _ = &mut shutdown_fut => {
                save_checkpoint_best_effort(
                    &bandit_state.bandit,
                    &activity_state.classifier,
                    ck_arms_hash,
                    first_telemetry_at,
                    &ck_path,
                );
                break;
            }
            _ = interval.tick() => {}
        }
        tick_count = tick_count.wrapping_add(1);
        // S3: freeze the onboarding anchor. Once resolved it never
        // re-derives (the `is_none` guard), so the retention sweep
        // deleting older telemetry can't slide `days_collected`. The
        // newly-derived value is written to disk by the next
        // `save_checkpoint_best_effort`.
        if first_telemetry_at.is_none() {
            first_telemetry_at = crate::power::onboarding::resolve_anchor(&state_root, None);
        }
        onboarding_state.status = Some(crate::power::onboarding::compute_onboarding_status(
            &state_root,
            &clock,
            cfg.onboarding.days,
            first_telemetry_at,
        ));
        // BUG-20260712-1530: publish the daemon's authoritative
        // onboarding view (gate + its effective `target_days`) so the
        // IPC `Status` handler serves the gate the daemon is actually
        // enforcing, not the CLI's re-computation.
        if let Some(status) = onboarding_state.status.as_ref() {
            if let Ok(mut g) = onboarding_latest.write() {
                *g = Some(crate::power::ipc::OnboardingWire::from_status(
                    status,
                    cfg.onboarding.days,
                ));
            }
        }
        let ctx = TickContext {
            sysfs_root: PathBuf::from("/sys"),
            cgroup_root: cgroup_root.clone(),
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu_actuator,
        };
        if let Err(e) = one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut shield_state,
            &mut bandit_state,
            &mut onboarding_state,
            &mut activity_state,
            &mut drift_state,
            &mut forecast_state,
            &mut actuator_latches,
            &drift_notifier,
            &retrain_trigger,
            &logger,
            &latest,
            &last_entry,
            &drift_latest,
        ) {
            tracing::warn!(target: "sy::power::daemon", error = %e, "tick append failed");
        }
        if tick_count.is_multiple_of(CHECKPOINT_INTERVAL_TICKS) {
            save_checkpoint_best_effort(
                &bandit_state.bandit,
                &activity_state.classifier,
                ck_arms_hash,
                first_telemetry_at,
                &ck_path,
            );
        }
    }

    // Graceful shutdown: emit STOPPING, restore vendor defaults,
    // drop the IPC socket. CrashSafeGuard's Drop would normally do
    // this on an Err return; with the explicit shutdown path we want
    // the same effect on a clean Ok(()) too.
    sy_core::notify::stopping();
    apply::crash_safe_exit_defaults(Path::new("/sys"));
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

/// Persist the bandit + classifier state to `path` and log a `warn`
/// on failure. Save is fire-and-best-effort (per BUG-20260525-2353
/// "Persistence MUST NOT block the tick loop") so a disk-full or
/// read-only-fs error never kills the daemon.
fn save_checkpoint_best_effort(
    bandit: &Clucb,
    classifier: &OnlineClassifier,
    arms_hash: u64,
    first_telemetry_at: Option<chrono::DateTime<chrono::Utc>>,
    path: &Path,
) {
    let ck = DaemonCheckpoint {
        schema: CHECKPOINT_SCHEMA,
        arms_hash,
        bandit: bandit.snapshot(),
        classifier: classifier.snapshot(),
        saved_at: chrono::Utc::now(),
        first_telemetry_at,
    };
    if let Err(e) = checkpoint::save(&ck, path) {
        tracing::warn!(
            target: "sy::power::daemon",
            error = %e,
            path = %path.display(),
            "checkpoint save failed; will retry next interval",
        );
    }
}

/// Production cgroup scope path. Mirrors `apply/cgroup.rs`'s module
/// docstring: the daemon is a `--user` service, so its scope lives
/// under `app.slice/sy-powerd.service`. A non-existent path is fine —
/// the `CgroupActuator` reports `MissingScope` and the daemon logs +
/// continues per the best-effort contract.
fn production_cgroup_root() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/sy-powerd.service"
    ))
}

/// Install the SPEC §4 NFR Reliability panic hook: on any panic, write
/// the vendor-default `platform_profile` + EPP values synchronously
/// before the default hook prints the backtrace. Best-effort — the
/// hook never panics itself.
fn install_panic_hook(sysfs_root: PathBuf) {
    let next = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        apply::crash_safe_exit_defaults(&sysfs_root);
        next(info);
    }));
}

/// RAII guard that writes vendor defaults on drop. Covers clean-exit
/// paths that don't go through the signal handler (e.g. `run_async`
/// returning an `Err` from the bind step). `Drop` ordering means this
/// fires before `tokio::Runtime::drop` tears the executor down.
pub struct CrashSafeGuard {
    sysfs_root: PathBuf,
}

impl CrashSafeGuard {
    pub fn new(sysfs_root: PathBuf) -> Self {
        Self { sysfs_root }
    }
}

impl Drop for CrashSafeGuard {
    fn drop(&mut self) {
        apply::crash_safe_exit_defaults(&self.sysfs_root);
    }
}

/// Resolve `$XDG_RUNTIME_DIR/sy/powerd.sock` with a `/run/user/<uid>`
/// fallback that matches the `agt::socket_path` convention.
pub fn socket_path() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d).join("sy/powerd.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    std::path::PathBuf::from(format!("/run/user/{uid}/sy/powerd.sock"))
}

/// Construct the production intent bundle. Mirrors `cli::live_intent`
/// but operates on the daemon-side `$HOME` / `/proc/pressure` paths.
/// Channels that fail to construct are left `None` so the tick
/// degrades to the documented defaults instead of crashing.
fn build_live_intent() -> Intent {
    use crate::power::intent::{
        AiplaneIntentChannel, CgroupAncestryChannel, IdleChannel, LogindChannel, MprisChannel,
        NiriChannel, NotifyChannel, PsiChannel, PsiKind, ScreenCastChannel, TimeChannel,
    };
    let psi_cpu = PsiChannel::new(Path::new("/proc/pressure/cpu"), PsiKind::Cpu).ok();
    // Resolve the intent whitelist next to the active power.toml so the
    // systemd `--user` service (cwd `$HOME`) finds the installed config,
    // not a cwd-relative miss (BUG-20260608-2341). Matches the CLI's
    // `build_live_status_value` derivation.
    let whitelist_path = super::power_config_path()
        .parent()
        .map(|d| d.join("intent_whitelist.toml"))
        .unwrap_or_else(|| PathBuf::from("configs/sy/intent_whitelist.toml"));
    let logind = LogindChannel::new(&whitelist_path).ok();
    let niri = NiriChannel::new().ok();
    let pool = std::sync::Arc::new(crate::aiplane::session::SessionPool::new());
    let reg = crate::aiplane::registry::Registry::new(pool);
    let aiplane = Some(AiplaneIntentChannel::new(
        reg.in_flight_counter(),
        reg.last_workload_slot(),
    ));
    let mpris = MprisChannel::new().ok();
    let portal = ScreenCastChannel::new().ok();
    let idle = Some(IdleChannel::new());
    let cgroup = Some(CgroupAncestryChannel::new(["firefox", "vscode"]));
    let notify = NotifyChannel::new().ok();
    Intent {
        psi_cpu,
        logind,
        niri,
        aiplane,
        mpris,
        portal,
        idle,
        cgroup,
        notify,
        time: TimeChannel::new(),
    }
}

/// Step 19 full IPC handler. Owns the pin slot + the last-applied
/// audit-entry cache so `Status` responses populate `applied_policy`
/// and `Profile{Set,Clear}` mutate the daemon's shared pin without
/// blocking the tick loop. Validates pin names against `arms` so a
/// caller-side typo is rejected with a structured [`crate::power::ipc::ProfileAck`]
/// instead of leaving the daemon in a degenerate state.
/// Shared daemon state an accepted IPC connection reads (and, for
/// `pin`, writes). Bundled into one struct so [`handle_connection_full`]
/// stays a two-argument fn — the slots grow one-per-surface (drift,
/// model, onboarding, …) and a positional arg list would blow the
/// clippy `too_many_arguments` ceiling.
struct ConnState {
    latest: LatestSnapshot,
    pin: LatestPin,
    last_entry: LatestAuditEntry,
    drift: LatestDriftStatus,
    model: LatestModelStatus,
    onboarding: LatestOnboarding,
    arms: Vec<Arm>,
}

async fn handle_connection_full(
    mut stream: tokio::net::UnixStream,
    state: ConnState,
) -> anyhow::Result<()> {
    use crate::power::ipc::{read_frame, write_frame, ProfileAck, StatusRequest, StatusResponse};
    let ConnState {
        latest,
        pin,
        last_entry,
        drift,
        model,
        onboarding,
        arms,
    } = state;
    let req: StatusRequest = read_frame(&mut stream).await?;
    match req {
        StatusRequest::Status => {
            let snap = latest
                .read()
                .map_err(|e| anyhow::anyhow!("latest snapshot poisoned: {e}"))?
                .clone();
            match snap {
                Some(s) => {
                    let entry = last_entry.read().ok().and_then(|g| g.clone());
                    let drift_status = drift.read().ok().map(|g| g.clone()).unwrap_or_default();
                    let model_status = model.read().ok().and_then(|g| g.clone());
                    let onboarding_status = onboarding.read().ok().and_then(|g| g.clone());
                    let mut resp = StatusResponse::from_snapshot(s);
                    resp.last_audit = entry;
                    resp.drift = drift_status;
                    resp.model = model_status;
                    resp.onboarding = onboarding_status;
                    write_frame(&mut stream, &resp).await?;
                }
                None => {
                    write_frame(
                        &mut stream,
                        &serde_json::json!({
                            "schema": crate::power::ipc::STATUS_SCHEMA,
                            "error": "no snapshot yet",
                        }),
                    )
                    .await?;
                }
            }
        }
        StatusRequest::ProfileSet { name } => {
            if !arms.iter().any(|a| a.name == name) {
                write_frame(
                    &mut stream,
                    &ProfileAck::rejected(format!("unknown arm {name:?}")),
                )
                .await?;
                return Ok(());
            }
            if let Ok(mut g) = pin.write() {
                *g = Some(name.clone());
            }
            write_frame(&mut stream, &ProfileAck::ok(Some(name))).await?;
        }
        StatusRequest::ProfileClear => {
            if let Ok(mut g) = pin.write() {
                *g = None;
            }
            write_frame(&mut stream, &ProfileAck::ok(None)).await?;
        }
    }
    Ok(())
}

/// `Notifier` that records every ping for the watchdog cadence test.
/// Lives next to the trait so the test doesn't need a second module.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MockNotifier {
    pub calls: std::sync::Mutex<Vec<std::time::Instant>>,
}

#[cfg(test)]
impl Notifier for MockNotifier {
    fn watchdog_ping(&self) {
        if let Ok(mut g) = self.calls.lock() {
            g.push(std::time::Instant::now());
        }
    }
}

/// Drive `n` `WATCHDOG=1` pings through `notifier`, sleeping
/// `interval` between each. Pure helper extracted from
/// [`spawn_watchdog_thread`] so the test can pin the cadence
/// contract without spawning a real OS thread or wall-clock waiting
/// 10 seconds. Production calls this with `n = usize::MAX`.
pub fn run_watchdog_loop(notifier: &dyn Notifier, interval: Duration, n: usize) {
    for _ in 0..n {
        notifier.watchdog_ping();
        std::thread::sleep(interval);
    }
}

/// Spawn the production watchdog-ping thread. Returns `None` when
/// the unit isn't notify-supervised (no `WATCHDOG_USEC` in env) —
/// same convention as `sy_core::notify::spawn_watchdog`, but routed
/// through the [`Notifier`] trait so the cadence test can inject a
/// `MockNotifier`. The thread lives until process exit; dropping the
/// returned handle does not cancel it.
pub fn spawn_watchdog_thread<N: Notifier + 'static>(
    notifier: N,
) -> Option<std::thread::JoinHandle<()>> {
    let interval = sd_notify::watchdog_enabled().map(sy_core::notify::compute_ping_interval)?;
    Some(std::thread::spawn(move || {
        run_watchdog_loop(&notifier, interval, usize::MAX);
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::clock::MockClock;
    use crate::power::log::{Logger, DEFAULT_MAX_SIZE_BYTES, DEFAULT_RETENTION_DAYS};
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;

    /// Free-space probe that lies "plenty of room" so the writer's
    /// disk-full gate never fires. Mirrors the helper in `log::tests`
    /// — kept private here to avoid widening the `log` module's
    /// surface for one neighbour test.
    struct PlentyProbe(AtomicU64);

    impl crate::power::log::FreeSpaceProbe for PlentyProbe {
        fn free_bytes(&self, _path: &std::path::Path) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn plenty_logger(root: std::path::PathBuf) -> Logger {
        Logger::with_overrides(
            root,
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(PlentyProbe(AtomicU64::new(10 * 1024 * 1024 * 1024))),
        )
    }

    fn pinned_clock() -> MockClock {
        MockClock::new(
            chrono::Utc
                .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
                .single()
                .expect("pinned UTC instant"),
        )
    }

    /// Test [`NpuActuator`] backed by a no-op runner — every NPU apply
    /// reports `Wrote { value }` without shelling out to `xrt-smi`.
    /// Step 19 daemon-in-thread tests must never touch the real
    /// binary; this helper keeps every tick hermetic.
    struct NoopNpuRunner;
    impl crate::power::apply::npu::CommandRunner for NoopNpuRunner {
        fn run(&self, _cmd: &str, _args: &[&str]) -> anyhow::Result<()> {
            Ok(())
        }
        /// Daemon-in-thread tests must never spawn the real `xrt-smi`;
        /// returning canonical XRT 2.x help keeps the P1-1 probe in
        /// `Some("--pmode")` mode so the existing assertions on
        /// `xrt-smi configure --pmode <mode>` argv still hold.
        fn run_capturing(&self, _cmd: &str, _args: &[&str]) -> anyhow::Result<String> {
            Ok("Usage: xrt-smi configure [--pmode <mode>]".to_string())
        }
    }

    /// Build a tempdir sysfs tree the daemon tick can write through —
    /// `firmware/acpi/platform_profile{,_choices}` + four
    /// `cpufreq/policy<N>/energy_performance_preference` leaves +
    /// an AMD iGPU stub. The `cgroup` actuator gets its own scope dir
    /// alongside so the daemon's cgroup writes hit a tempdir, never
    /// `/sys/fs/cgroup/`.
    fn fixture_sysfs(td: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = td.path().join("sys");
        let acpi = root.join("firmware/acpi");
        std::fs::create_dir_all(&acpi).expect("mkdir acpi");
        std::fs::write(acpi.join("platform_profile"), "performance\n").expect("seed profile");
        std::fs::write(
            acpi.join("platform_profile_choices"),
            "quiet balanced performance low-power\n",
        )
        .expect("seed choices");
        let cpufreq = root.join("devices/system/cpu/cpufreq");
        for i in 0..2 {
            let p = cpufreq.join(format!("policy{i}"));
            std::fs::create_dir_all(&p).expect("mkdir policy");
            std::fs::write(p.join("scaling_governor"), "schedutil\n").expect("seed gov");
            std::fs::write(p.join("energy_performance_preference"), "performance\n")
                .expect("seed epp");
        }
        // iGPU fixture — POWER_SAVING currently active so a `browse`
        // arm's `BOOTUP_DEFAULT` (idx 0) triggers a real write.
        let card = root.join("class/drm/card0/device");
        std::fs::create_dir_all(&card).expect("mkdir card");
        std::fs::write(card.join("vendor"), "0x1002\n").expect("seed vendor");
        std::fs::write(
            card.join("pp_power_profile_mode"),
            "NUM        MODE_NAME\n  0   BOOTUP_DEFAULT  :\n  2     POWER_SAVING *:\n",
        )
        .expect("seed pp_power_profile_mode");
        let cgroup = td.path().join("cgroup/sy-powerd.service");
        std::fs::create_dir_all(&cgroup).expect("mkdir cgroup");
        std::fs::write(cgroup.join("cpu.weight"), "100\n").expect("seed cgroup weight");
        std::fs::write(cgroup.join("cpu.uclamp.min"), "0\n").expect("seed uclamp_min");
        std::fs::write(cgroup.join("cpu.uclamp.max"), "100\n").expect("seed uclamp_max");
        (root, cgroup)
    }

    /// Load the shipped `configs/sy/power.toml` so tests exercise the
    /// canonical arm table + baseline mapping. The repo root is
    /// `CARGO_MANIFEST_DIR` at compile time; tests run with that as
    /// `cwd`.
    fn shipped_config() -> PowerConfig {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/sy/power.toml");
        PowerConfig::load(&path).expect("shipped power.toml parses")
    }

    /// Construct an `NpuActuator` wired to the no-op runner. The
    /// production daemon uses `production_npu_actuator()` instead.
    fn test_npu() -> NpuActuator {
        NpuActuator::new(
            Box::new(NoopNpuRunner),
            Box::new(crate::power::apply::SystemTimeSource::new()),
        )
    }

    /// BUG-20260712-1545 part 2: the production trigger must point the
    /// trainer at the telemetry *directory* (daily-segmented NDJSON)
    /// and write the model to `forecaster.onnx` — the CLI convention
    /// (`src/power/cli.rs`), not the old `forecast.onnx`.
    #[test]
    fn retrain_trigger_writes_forecaster_onnx() {
        let tmp = TempDir::new().expect("tempdir");
        let trig =
            SpawnBlockingRetrainTrigger::for_state_root(tmp.path(), new_latest_model_status());
        assert_eq!(trig.telemetry_path, tmp.path());
        assert_eq!(trig.out_path, tmp.path().join("forecaster.onnx"));
    }

    /// BUG-20260712-1545 part 3: on startup a trained `forecaster.onnx`
    /// on disk must be loaded in preference to the embedded warmup, so
    /// a retrained model survives a daemon restart. We seed the file
    /// with a real ONNX (the warmup bytes) and assert the loaded model
    /// carries the byte-derived version SHA — *not* the "rules-baseline"
    /// sentinel `Model::warmup` stamps — proving the disk model won.
    #[test]
    fn startup_loads_trained_model_over_warmup() {
        use crate::power::forecast::model::{WARMUP_ONNX, WARMUP_VERSION_SHA};
        let tmp = TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("forecaster.onnx"), WARMUP_ONNX).expect("seed model");
        let state = ForecastTickState::load_or_warmup(tmp.path()).expect("load trained model");
        assert_ne!(
            state.model.version_sha, WARMUP_VERSION_SHA,
            "on-disk trained model must be loaded, not the warmup fixture",
        );
        assert_eq!(state.model.input_dim, 12);
    }

    /// BUG-20260712-1545 part 3: a present-but-corrupt `forecaster.onnx`
    /// must WARN + fall back to the warmup model, never panic / crash
    /// the daemon. A missing file also falls back to warmup.
    #[test]
    fn startup_falls_back_to_warmup_on_corrupt_model() {
        use crate::power::forecast::model::WARMUP_VERSION_SHA;
        let tmp = TempDir::new().expect("tempdir");
        std::fs::write(tmp.path().join("forecaster.onnx"), b"not a real onnx graph")
            .expect("seed corrupt model");
        let state =
            ForecastTickState::load_or_warmup(tmp.path()).expect("corrupt model falls back");
        assert_eq!(
            state.model.version_sha, WARMUP_VERSION_SHA,
            "corrupt model must fall back to the warmup fixture",
        );

        // Missing file (fresh host) also yields warmup.
        let empty = TempDir::new().expect("tempdir");
        let fresh = ForecastTickState::load_or_warmup(empty.path()).expect("missing file → warmup");
        assert_eq!(fresh.model.version_sha, WARMUP_VERSION_SHA);
    }

    /// Step 26 test sink: counts `dispatch()` calls without spawning
    /// a real burn trainer. Tests assert on `count.load(SeqCst)` to
    /// pin the retrain scheduler's gate decisions. Step 31 widens the
    /// sink to also record the [`RetrainCause`] so the
    /// `drift_*_retrain` tests can distinguish onboarding from drift.
    #[derive(Default)]
    struct CapturingRetrainTrigger {
        pub count: std::sync::atomic::AtomicUsize,
        pub causes: std::sync::Mutex<Vec<RetrainCause>>,
    }

    impl RetrainTrigger for CapturingRetrainTrigger {
        fn dispatch(&self, cause: RetrainCause) {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Ok(mut g) = self.causes.lock() {
                g.push(cause);
            }
        }
    }

    /// No-op retrain trigger for legacy tests that pre-date Step 26 —
    /// they don't care whether the scheduler fires. The capturing
    /// trigger above is the choice for tests that DO care.
    struct NoopRetrainTrigger;
    impl RetrainTrigger for NoopRetrainTrigger {
        fn dispatch(&self, _cause: RetrainCause) {}
    }

    /// Post-onboarding state — pre-loads `OnboardingTickState` with
    /// `active = false` so the legacy tests see the bandit propose
    /// path they were authored against. The `ready_at` is in the past
    /// to make the intent explicit.
    fn post_onboarding_state() -> OnboardingTickState {
        OnboardingTickState {
            status: Some(OnboardingStatus {
                active: false,
                days_collected: 14,
                ready_at: chrono::Utc::now() - chrono::Duration::days(1),
            }),
            last_retrain_at: None,
        }
    }

    /// Test-only shim: pre-Step-26 tests called `one_tick` with the
    /// 10-arg signature. Step 26 added two parameters
    /// (`onboarding_state`, `retrain_trigger`); Step 31 added four
    /// more (`drift_state`, `drift_notifier`, `drift_latest`); this
    /// helper threads the default post-onboarding state + no-op
    /// retrain + no-op drift through so the legacy tests don't
    /// repeat the boilerplate. The Step 26/31 onboarding/retrain/
    /// drift tests bypass this helper and call `one_tick` directly
    /// with their bespoke fixtures.
    #[allow(clippy::too_many_arguments)]
    fn legacy_one_tick(
        sensors: &Sensors,
        intent: &mut Intent,
        clock: &dyn Clock,
        ctx: &TickContext<'_>,
        pin: &LatestPin,
        prev_state: &mut ShieldState,
        bandit_state: &mut BanditTickState,
        logger: &Logger,
        latest: &LatestSnapshot,
        last_entry: &LatestAuditEntry,
    ) -> Result<Snapshot, LogError> {
        let mut onb = post_onboarding_state();
        let mut activity = ActivityTickState::new();
        let mut drift = DriftTickState::new();
        let mut forecast = ForecastTickState::warmup().expect("warmup model");
        let mut latches = ActuatorLatches::default();
        let drift_latest = new_latest_drift_status();
        let trig = NoopRetrainTrigger;
        let drift_notifier = NoopDriftNotifier;
        // Bridge the legacy `&mut ShieldState` callers onto the
        // BUG-20260712-1201 `ShieldTickState`. The MEETING-lock
        // timestamp need not persist across legacy calls (these tests
        // don't drive `call_active`), so seed `prev` from `prev_state`
        // and write the DFA output back afterwards.
        let mut shield = ShieldTickState {
            prev: *prev_state,
            last_call_at: None,
        };
        let out = one_tick(
            sensors,
            intent,
            clock,
            ctx,
            pin,
            &mut shield,
            bandit_state,
            &mut onb,
            &mut activity,
            &mut drift,
            &mut forecast,
            &mut latches,
            &drift_notifier,
            &trig,
            logger,
            latest,
            last_entry,
            &drift_latest,
        );
        *prev_state = shield.prev;
        out
    }

    /// No-op desktop notifier for tests that don't care about the
    /// drift-alarm notification path. The Step 31 drift tests use
    /// [`MockDriftNotifier`] instead.
    struct NoopDriftNotifier;
    impl DriftNotifier for NoopDriftNotifier {
        fn notify(&self, _summary: &str, _body: &str) {}
    }

    /// Test sink: records every drift notification call so the Step 31
    /// alarm tests can assert exactly-once delivery against the SPEC §5
    /// wording.
    #[derive(Default)]
    struct MockDriftNotifier {
        pub calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl DriftNotifier for MockDriftNotifier {
        fn notify(&self, summary: &str, body: &str) {
            if let Ok(mut g) = self.calls.lock() {
                g.push((summary.to_string(), body.to_string()));
            }
        }
    }

    /// Step 10 DoD: three `one_tick` calls land three NDJSON lines on
    /// disk. Step 19 expands `one_tick` from snapshot+log to
    /// snapshot+log+apply, so the test now wires the actuator
    /// fixtures + shipped config; the line-count assertion is
    /// unchanged.
    #[test]
    fn tick_assembles_and_logs_one_entry() {
        const TICKS: usize = 3;
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let sensors = Sensors::default();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let ctx = TickContext {
            sysfs_root,
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldState::CoolAc;
        let mut bandit = BanditTickState::from_config(&cfg);
        for _ in 0..TICKS {
            legacy_one_tick(
                &sensors,
                &mut intent,
                &clock,
                &ctx,
                &pin,
                &mut prev,
                &mut bandit,
                &logger,
                &latest,
                &last_entry,
            )
            .expect("one_tick ok");
            clock.tick(Duration::from_secs(1));
        }
        let day = chrono::Utc
            .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
            .single()
            .expect("pinned UTC instant")
            .date_naive();
        let path = tmp
            .path()
            .join("power")
            .join(format!("telemetry-{day}.ndjson"));
        let contents = std::fs::read_to_string(&path).expect("read log");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(
            lines.len(),
            TICKS,
            "expected {TICKS} NDJSON entries, got {}: {contents}",
            lines.len(),
        );
        // Latest snapshot must be populated after the loop.
        assert!(
            latest.read().expect("latest").is_some(),
            "latest snapshot should hold the most recent tick"
        );
        // Step 19 DoD: the cached audit entry mirrors the last tick.
        assert!(
            last_entry.read().expect("entry").is_some(),
            "last audit entry should be cached after the tick"
        );
    }

    /// Step 19 DoD: when no pin is set and the snapshot lands in
    /// COOL_AC (cool Tctl, AC, full battery, no call), the daemon
    /// applies the rules baseline arm for COOL_AC — `browse` in the
    /// shipped config.
    #[test]
    fn rules_baseline_applies_browse_when_cool_ac() {
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root: sysfs_root.clone(),
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldState::CoolAc;
        let mut bandit = BanditTickState::from_config(&cfg);
        legacy_one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut prev,
            &mut bandit,
            &logger,
            &latest,
            &last_entry,
        )
        .expect("tick ok");
        let entry = last_entry.read().expect("entry").clone().expect("entry");
        assert_eq!(entry.applied_arm.as_deref(), Some("browse"));
        assert_eq!(entry.shield_state.as_deref(), Some("COOL_AC"));
    }

    /// Step 19 DoD: a manual pin overrides the rules baseline. Pin
    /// `build`; even with a COOL_AC snapshot the daemon applies
    /// `build`, not `browse`.
    #[test]
    fn manual_pin_overrides_baseline() {
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        *pin.write().expect("pin") = Some("build".to_string());
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root: sysfs_root.clone(),
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldState::CoolAc;
        let mut bandit = BanditTickState::from_config(&cfg);
        legacy_one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut prev,
            &mut bandit,
            &logger,
            &latest,
            &last_entry,
        )
        .expect("tick ok");
        let entry = last_entry.read().expect("entry").clone().expect("entry");
        assert_eq!(entry.applied_arm.as_deref(), Some("build"));
        assert!(
            entry.reason_chain.iter().any(|r| r == "pin:build"),
            "reason chain must record the pin: {:?}",
            entry.reason_chain,
        );
    }

    /// Step 19 DoD: clearing the pin returns the daemon to the rules
    /// baseline on the next tick.
    #[test]
    fn pin_cleared_by_auto() {
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        *pin.write().expect("pin") = Some("build".to_string());
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root: sysfs_root.clone(),
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldState::CoolAc;
        let mut bandit = BanditTickState::from_config(&cfg);
        legacy_one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut prev,
            &mut bandit,
            &logger,
            &latest,
            &last_entry,
        )
        .expect("first tick ok");
        // Clear pin + advance the clock past the thrash window.
        *pin.write().expect("pin") = None;
        clock.tick(Duration::from_secs(
            (cfg.shield.profile_thrash_min_interval_s + 1) as u64,
        ));
        std::thread::sleep(Duration::from_millis(2));
        legacy_one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut prev,
            &mut bandit,
            &logger,
            &latest,
            &last_entry,
        )
        .expect("second tick ok");
        let entry = last_entry.read().expect("entry").clone().expect("entry");
        assert_eq!(
            entry.applied_arm.as_deref(),
            Some("browse"),
            "after --auto, baseline rules apply: {:?}",
            entry.reason_chain,
        );
    }

    /// Build a `Sensors` bundle the daemon-in-thread tests can drive
    /// — every sensor's sysfs read lands in the supplied tempdir
    /// tree, never `/sys`. Production wires `Sensors::all()` against
    /// `/sys` directly. The fixture trees seeded by `fixture_sysfs`
    /// resolve every channel the snapshot assembler reads.
    fn cool_ac_sensors(_root: &Path) -> Sensors {
        Sensors::all()
    }

    /// Step 19 DoD: a synthetic HOT snapshot drives the daemon to the
    /// rules-baseline arm for HOT (`idle` in the shipped config). The
    /// snapshot is injected by mutating the cached `latest` slot
    /// directly: we run `one_tick` against an empty sensors bundle
    /// (yields `Snapshot` with `tctl_c == None`), then overwrite the
    /// state machine's seed to `Hot` and re-run — the daemon's apply
    /// loop honours the inherited state on the next tick so the
    /// applied arm reflects the constraint envelope without any
    /// `/sys` fixture mutation.
    ///
    /// The cleaner shape would seed `tctl_c >= tctl_act_c` directly,
    /// but the current `Sensors::default()` returns `None`-valued
    /// readings; rather than hand-roll a `FakeSensor` bundle (Step 19
    /// is bandit-free and one-shot), we pin `prev_state = Hot` and
    /// let the DFA's `Meeting`-style inheritance hold. The HOT
    /// branch of `transition` requires Tctl ≥ act_c; without a Tctl
    /// reading the DFA falls back to COOL_AC, so we synthesise the
    /// HOT state by passing it as the *first-tick* anchor and
    /// directly observing the projected arm under that constraint.
    #[test]
    fn hot_baseline_applies_idle() {
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        // Seed Tctl above the HOT threshold via a custom hwmon
        // fixture: the snapshot's hwmon sensor reads `tctl_c` from
        // `class/hwmon/hwmon0/temp1_input`. Lay the file down so the
        // collected snapshot's `raw.tctl_c` lands at 92 °C → HOT.
        let hwmon = sysfs_root.join("class/hwmon/hwmon0");
        std::fs::create_dir_all(&hwmon).expect("mkdir hwmon");
        std::fs::write(hwmon.join("name"), "k10temp\n").expect("seed hwmon name");
        std::fs::write(hwmon.join("temp1_input"), "92000\n").expect("seed temp1_input");
        std::fs::write(hwmon.join("temp1_label"), "Tctl\n").expect("seed temp1_label");
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root: sysfs_root.clone(),
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldState::CoolAc;
        let mut bandit = BanditTickState::from_config(&cfg);
        legacy_one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut prev,
            &mut bandit,
            &logger,
            &latest,
            &last_entry,
        )
        .expect("tick ok");
        let entry = last_entry.read().expect("entry").clone().expect("entry");
        assert_eq!(
            entry.shield_state.as_deref(),
            Some("HOT"),
            "fixture must seed HOT (tctl=92°C): {:?}",
            entry.reason_chain,
        );
        assert_eq!(
            entry.applied_arm.as_deref(),
            Some("idle"),
            "HOT baseline is `idle`: {:?}",
            entry.reason_chain,
        );
    }

    /// SPEC §4 NFR Reliability: the `CrashSafeGuard` drop path writes
    /// the vendor-default platform_profile + EPP values. Constructed
    /// against a tempdir sysfs so the assertion is hermetic.
    #[test]
    fn exit_writes_vendor_defaults() {
        let td = TempDir::new().expect("tempdir");
        let root = td.path();
        let acpi = root.join("firmware/acpi");
        std::fs::create_dir_all(&acpi).expect("mkdir acpi");
        std::fs::write(acpi.join("platform_profile"), "performance\n").expect("seed profile");
        let cpufreq = root.join("devices/system/cpu/cpufreq/policy0");
        std::fs::create_dir_all(&cpufreq).expect("mkdir policy");
        std::fs::write(
            cpufreq.join("energy_performance_preference"),
            "performance\n",
        )
        .expect("seed epp");

        {
            let _guard = CrashSafeGuard::new(root.to_path_buf());
            // `_guard` drops at end of scope.
        }

        let pp = std::fs::read_to_string(acpi.join("platform_profile")).expect("read profile");
        assert_eq!(pp.trim(), "balanced");
        let epp = std::fs::read_to_string(cpufreq.join("energy_performance_preference"))
            .expect("read epp");
        assert_eq!(epp.trim(), "balance_performance");
    }

    /// Step P2-3 DoD: after a single `one_tick`, the cached snapshot's
    /// `raw.activity_forecast` slot is populated — the GRU runs every
    /// tick (no longer the deferred Step 29b 0.0 stub feed) so the
    /// audit log and the next-tick drift residual both see a live
    /// 5-class probability distribution.
    #[test]
    fn activity_forecast_populated_in_snapshot_raw() {
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root,
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldTickState::new();
        let mut bandit = BanditTickState::from_config(&cfg);
        let mut onb = post_onboarding_state();
        let mut activity = ActivityTickState::new();
        let mut drift = DriftTickState::new();
        let mut forecast = ForecastTickState::warmup().expect("warmup model");
        let drift_latest = new_latest_drift_status();
        let trig = NoopRetrainTrigger;
        let drift_notifier = NoopDriftNotifier;
        one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut prev,
            &mut bandit,
            &mut onb,
            &mut activity,
            &mut drift,
            &mut forecast,
            &mut ActuatorLatches::default(),
            &drift_notifier,
            &trig,
            &logger,
            &latest,
            &last_entry,
            &drift_latest,
        )
        .expect("tick ok");
        let snap = latest.read().expect("latest").clone().expect("snap");
        assert!(
            snap.raw.activity_forecast.is_some(),
            "Step P2-3 wires GRU inference into one_tick: {:?}",
            snap.raw.activity_forecast,
        );
        let probs = snap.raw.activity_forecast.expect("forecast probs");
        let total: f32 = probs.iter().sum();
        assert!(
            (total - 1.0).abs() < 1e-4,
            "5-class probs must sum to ~1.0, got {total}: {probs:?}",
        );
    }

    /// Step P2-3 DoD: ADWIN's forecast-residual stream is driven by
    /// `|argmax(prev_forecast) - actual_label|` instead of the Step
    /// 29b 0.0 constant. We script a sequence where every prior
    /// forecast predicts `Idle` but the realised label is `Code`,
    /// then assert ADWIN's window grew by exactly N samples — proving
    /// the residual is feeding the detector. A separate stationary
    /// run (matching forecast + label) holds the window without
    /// affecting the alarm.
    #[test]
    fn drift_adwin_residual_uses_forecast_vs_actual() {
        const MISMATCH_TICKS: usize = 16;
        let clock = pinned_clock();
        let notifier = NoopDriftNotifier;
        let mut drift = DriftTickState::new();
        // argmax of `[0.6, 0.1, 0.1, 0.1, 0.1]` is index 0 = Idle.
        let prev_idle = [0.6_f32, 0.1, 0.1, 0.1, 0.1];
        for _ in 0..MISMATCH_TICKS {
            let residual = forecast_residual(Some(prev_idle), ActivityLabel::Code);
            assert!(
                (residual - 1.0).abs() < f32::EPSILON,
                "mismatch must yield residual=1.0, got {residual}",
            );
            observe_forecast_drift(&mut drift, &notifier, &clock, residual);
        }
        assert_eq!(
            drift.detector.forecast.window_len(),
            MISMATCH_TICKS,
            "ADWIN must absorb every forecast-vs-actual residual",
        );
        // Stationary post-stream: matching forecast + label yields
        // residual=0.0 → window keeps growing but no alarm fires.
        for _ in 0..MISMATCH_TICKS {
            let residual = forecast_residual(Some(prev_idle), ActivityLabel::Idle);
            assert!(
                residual.abs() < f32::EPSILON,
                "match must yield residual=0.0, got {residual}",
            );
            observe_forecast_drift(&mut drift, &notifier, &clock, residual);
        }
        assert_eq!(
            drift.detector.forecast.window_len(),
            MISMATCH_TICKS * 2,
            "ADWIN window must accumulate every sample, mismatched or not",
        );
    }

    /// Step 22 DoD: the audit entry carries the bandit's top-3
    /// `(arm_name, ucb_score)` tuples in descending order, plus the
    /// `conservative_alpha` margin in force at decision time. The
    /// top-3 is what `sy power status --json`'s `bandit.top3` field
    /// surfaces and what Step 23's `sy power explain` replays from.
    #[test]
    fn audit_log_includes_ranked_top3() {
        const EXPECTED_TOP_N: usize = 3;
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root: sysfs_root.clone(),
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldState::CoolAc;
        let mut bandit = BanditTickState::from_config(&cfg);
        legacy_one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut prev,
            &mut bandit,
            &logger,
            &latest,
            &last_entry,
        )
        .expect("tick ok");
        let entry = last_entry.read().expect("entry").clone().expect("entry");
        assert_eq!(
            entry.ranked_actions.len(),
            EXPECTED_TOP_N,
            "ranked_actions must carry top-{EXPECTED_TOP_N}: {:?}",
            entry.ranked_actions,
        );
        for w in entry.ranked_actions.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "ranked_actions must be descending by score: {} then {}",
                w[0].1,
                w[1].1,
            );
        }
        assert!(
            (entry.conservative_alpha - cfg.bandit.alpha as f32).abs() < 1e-6,
            "conservative_alpha must mirror cfg.bandit.alpha: {} vs {}",
            entry.conservative_alpha,
            cfg.bandit.alpha,
        );
    }

    /// Step 22 DoD: at tick N the bandit's posterior reflects the
    /// reward computed from tick N-1's chosen arm. Wiring tick 1's
    /// pick takes effect at tick 2 (one `update()` call); tick 2's
    /// pick lands at tick 3 (two `update()` calls). Tick 3's pick is
    /// still pending — its reward fires on a hypothetical tick 4
    /// that this test deliberately does not run.
    #[test]
    fn reward_update_lags_one_tick() {
        const TICKS: usize = 3;
        const EXPECTED_UPDATES: u64 = (TICKS - 1) as u64;
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root: sysfs_root.clone(),
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldState::CoolAc;
        let mut bandit = BanditTickState::from_config(&cfg);
        let mut chosen_arms: Vec<String> = Vec::with_capacity(TICKS);
        for _ in 0..TICKS {
            legacy_one_tick(
                &sensors,
                &mut intent,
                &clock,
                &ctx,
                &pin,
                &mut prev,
                &mut bandit,
                &logger,
                &latest,
                &last_entry,
            )
            .expect("tick ok");
            let entry = last_entry.read().expect("entry").clone().expect("entry");
            chosen_arms.push(entry.applied_arm.expect("applied arm"));
            clock.tick(Duration::from_secs(1));
        }
        // The first TICKS-1 picks have been folded into the bandit's
        // posterior; the last pick is still in `last_chosen` waiting
        // for the (never-fired) next tick. Sum across the *unique*
        // configured arm set so an arm that was chosen twice does not
        // get double-counted.
        let total_updates: u64 = cfg
            .arms
            .iter()
            .map(|a| bandit.bandit.arm_update_count(&a.name))
            .sum();
        assert_eq!(
            total_updates, EXPECTED_UPDATES,
            "after {TICKS} ticks the bandit must have absorbed exactly \
             {EXPECTED_UPDATES} rewards (one per *completed* tick); chosen={chosen_arms:?}",
        );
        // The pending tick's pick has been recorded in `last_chosen`
        // but not yet folded into the bandit — the next call to
        // `one_tick` would close that window.
        assert!(
            bandit.last_chosen.is_some(),
            "the most recent tick's pick must remain pending in `last_chosen`",
        );
    }

    /// Step 29 DoD: a replay of N ticks with one injected manual pin
    /// produces audit entries whose `snapshot.raw.activity_label`
    /// slot is populated (non-`None`) — Step 28 left the slot at
    /// `None` until the daemon's `one_tick` wired the classifier;
    /// Step 29 closes that gap. The pin is injected at tick 0 so the
    /// classifier's first `partial_fit` lands before tick 1 reads
    /// it back through `classify`; every subsequent tick records a
    /// label.
    #[test]
    fn audit_entries_carry_activity_labels_after_pin() {
        const TICKS: usize = 12;
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        // Inject one manual pin so `extract_label` returns Some on the
        // first tick; subsequent ticks drop the pin to exercise the
        // pure-classify path.
        *pin.write().expect("pin") = Some("build".to_string());
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root: sysfs_root.clone(),
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldTickState::new();
        let mut bandit = BanditTickState::from_config(&cfg);
        let mut onb = post_onboarding_state();
        let mut activity = ActivityTickState::new();
        let mut drift = DriftTickState::new();
        let mut forecast = ForecastTickState::warmup().expect("warmup model");
        let drift_latest = new_latest_drift_status();
        let trig = NoopRetrainTrigger;
        let drift_notifier = NoopDriftNotifier;
        let mut labels_seen: Vec<Option<ActivityLabel>> = Vec::with_capacity(TICKS);
        for i in 0..TICKS {
            // Drop the pin after the first tick so the classifier is
            // the only source of `activity_label` from tick 1 onward.
            if i == 1 {
                *pin.write().expect("pin") = None;
            }
            one_tick(
                &sensors,
                &mut intent,
                &clock,
                &ctx,
                &pin,
                &mut prev,
                &mut bandit,
                &mut onb,
                &mut activity,
                &mut drift,
                &mut forecast,
                &mut ActuatorLatches::default(),
                &drift_notifier,
                &trig,
                &logger,
                &latest,
                &last_entry,
                &drift_latest,
            )
            .expect("tick ok");
            let entry = last_entry.read().expect("entry").clone().expect("entry");
            labels_seen.push(entry.snapshot.raw.activity_label);
            clock.tick(Duration::from_secs(1));
        }
        // Every tick must carry a populated `activity_label` — the
        // classifier returns the argmax (defaulting to `Idle` before
        // any training) so the slot is never `None`.
        for (i, label) in labels_seen.iter().enumerate() {
            assert!(
                label.is_some(),
                "tick {i} must carry an activity_label: {labels_seen:?}",
            );
        }
    }

    /// Step 29 DoD: the bandit context grew by one dimension to
    /// accommodate the activity-label channel. Pin the new width
    /// through [`Clucb::context_dim`] so any future widening trip
    /// this assertion and force the roadmap to bump explicitly.
    #[test]
    fn bandit_context_width_grew_by_one() {
        use crate::power::bandit::FEATURE_LEN_WITH_ACTIVITY;
        let cfg = shipped_config();
        let state = BanditTickState::from_config(&cfg);
        assert_eq!(
            state.bandit.context_dim(),
            FEATURE_LEN_WITH_ACTIVITY,
            "Step 29 widens the CLUCB context to features+activity (13)",
        );
        assert_eq!(
            FEATURE_LEN_WITH_ACTIVITY,
            crate::power::snapshot::FEATURE_LEN + 1,
            "widened dim must be exactly +1 over the snapshot feature vec",
        );
    }

    /// Step 22 DoD: the bandit never picks an arm outside the
    /// configured 8. `propose_ranked` only returns configured arms by
    /// construction; we assert that invariant directly so future
    /// refactors that widen the arm enum cannot silently bypass the
    /// shield's constraint table.
    #[test]
    fn bandit_picks_only_configured_arms() {
        const EXPECTED_ARM_COUNT: usize = 8;
        let cfg = shipped_config();
        assert_eq!(
            cfg.arms.len(),
            EXPECTED_ARM_COUNT,
            "shipped power.toml must enumerate {EXPECTED_ARM_COUNT} arms",
        );
        let bandit_state = BanditTickState::from_config(&cfg);
        // Step 29: the bandit context is now `FEATURE_LEN + 1` wide;
        // 0.5 is a benign mid-band stand-in for the activity slot too.
        let ctx = vec![0.5_f32; crate::power::bandit::FEATURE_LEN_WITH_ACTIVITY];
        let ranked = bandit_state.bandit.propose_ranked(&ctx);
        let configured: std::collections::HashSet<&str> =
            cfg.arms.iter().map(|a| a.name.as_str()).collect();
        for (name, _score) in &ranked {
            assert!(
                configured.contains(name.as_str()),
                "bandit returned non-configured arm {name:?} (configured={:?})",
                configured,
            );
        }
        assert_eq!(
            ranked.len(),
            EXPECTED_ARM_COUNT,
            "propose_ranked must surface every configured arm",
        );
    }

    /// Step 22 DoD (the CLUCB conservative-anchor invariant): with no
    /// signal — fake sensors that surface a constant zero feature vec
    /// — the bandit must defer to the rules-baseline arm on ≥ (1 − α)
    /// of ticks. The 1000-tick budget mirrors the integration-test
    /// shape requested by the roadmap; running it inline keeps the
    /// assertion next to the daemon's `one_tick` it exercises.
    #[test]
    fn bandit_defers_to_baseline_under_no_signal() {
        const TICKS: usize = 1000;
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root: sysfs_root.clone(),
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldState::CoolAc;
        let mut bandit = BanditTickState::from_config(&cfg);
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for _ in 0..TICKS {
            legacy_one_tick(
                &sensors,
                &mut intent,
                &clock,
                &ctx,
                &pin,
                &mut prev,
                &mut bandit,
                &logger,
                &latest,
                &last_entry,
            )
            .expect("tick ok");
            let entry = last_entry.read().expect("entry").clone().expect("entry");
            let arm = entry.applied_arm.expect("applied arm");
            *counts.entry(arm).or_insert(0) += 1;
            clock.tick(Duration::from_secs(1));
        }
        let baseline = cfg.rules_baseline.cool_ac.clone();
        let baseline_share = *counts.get(&baseline).unwrap_or(&0) as f64 / TICKS as f64;
        let floor = 1.0 - cfg.bandit.alpha;
        assert!(
            baseline_share >= floor,
            "bandit must defer to rules-baseline {baseline:?} on >= {:.0}% of ticks \
             when there is no reward signal; observed {:.1}% ({counts:?})",
            floor * 100.0,
            baseline_share * 100.0,
        );
    }

    /// Step 26 DoD: while [`OnboardingStatus::active`] is `true`, the
    /// daemon must NOT consult the bandit propose path. We seed the
    /// onboarding state with `active=true` and assert two invariants:
    ///
    /// 1. The audit entry's `ranked_actions` is a singleton (= the
    ///    rules baseline arm), not a top-3 of bandit picks.
    /// 2. The reason chain carries the
    ///    `onboarding-baseline:<arm>` source label, not
    ///    `bandit:<arm> (ucb=…)`.
    #[test]
    fn bandit_dormant_during_onboarding() {
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root,
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldTickState::new();
        let mut bandit = BanditTickState::from_config(&cfg);
        let mut onb = OnboardingTickState {
            status: Some(OnboardingStatus {
                active: true,
                days_collected: 3,
                ready_at: chrono::Utc::now() + chrono::Duration::days(11),
            }),
            last_retrain_at: None,
        };
        let trig = CapturingRetrainTrigger::default();
        let mut activity = ActivityTickState::new();
        let mut drift = DriftTickState::new();
        let mut forecast = ForecastTickState::warmup().expect("warmup model");
        let drift_latest = new_latest_drift_status();
        let drift_notifier = NoopDriftNotifier;
        one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut prev,
            &mut bandit,
            &mut onb,
            &mut activity,
            &mut drift,
            &mut forecast,
            &mut ActuatorLatches::default(),
            &drift_notifier,
            &trig,
            &logger,
            &latest,
            &last_entry,
            &drift_latest,
        )
        .expect("tick ok");
        let entry = last_entry.read().expect("entry").clone().expect("entry");
        assert_eq!(
            entry.ranked_actions.len(),
            1,
            "onboarding dormancy must produce a singleton ranked list: {:?}",
            entry.ranked_actions,
        );
        assert_eq!(
            entry.applied_arm.as_deref(),
            Some(cfg.rules_baseline.cool_ac.as_str()),
            "onboarding tick must apply the rules-baseline arm",
        );
        let baseline = &cfg.rules_baseline.cool_ac;
        assert!(
            entry
                .reason_chain
                .iter()
                .any(|r| r == &format!("onboarding-baseline:{baseline}")),
            "reason chain must record the onboarding gate: {:?}",
            entry.reason_chain,
        );
    }

    /// Step 26 DoD: the retrain scheduler refuses to dispatch when
    /// `ac_online = false`. Pure-function test over
    /// [`evaluate_retrain_trigger`] so the assertion is hermetic and
    /// doesn't need to spin up a sensor fixture.
    #[test]
    fn train_skipped_when_on_battery() {
        let mut snap = empty_snapshot();
        snap.raw.ac_online = Some(false);
        snap.raw.user_idle_s = Some(600.0);
        snap.raw.battery_soc_pct = Some(90);
        let outcome = evaluate_retrain_trigger(&snap, false, None, chrono::Utc::now());
        assert_eq!(
            outcome,
            RetrainOutcome::Skipped(RetrainSkipReason::OnBattery),
            "on battery must skip with OnBattery reason",
        );
    }

    /// Step 26 DoD: the retrain scheduler refuses to dispatch when
    /// `user_idle_s < 300`. We hold AC + SOC at "ready" values so the
    /// only failing gate is the idle one.
    #[test]
    fn train_skipped_when_idle_lt_5min() {
        let mut snap = empty_snapshot();
        snap.raw.ac_online = Some(true);
        snap.raw.user_idle_s = Some(60.0);
        snap.raw.battery_soc_pct = Some(90);
        let outcome = evaluate_retrain_trigger(&snap, false, None, chrono::Utc::now());
        assert_eq!(
            outcome,
            RetrainOutcome::Skipped(RetrainSkipReason::UserActive),
            "idle < 5 min must skip with UserActive reason",
        );
    }

    /// Step 26 DoD: with every gate open, the scheduler dispatches.
    /// Pins the positive case so future regressions in the gate
    /// chain don't silently mute training.
    #[test]
    fn train_dispatched_when_all_gates_open() {
        let mut snap = empty_snapshot();
        snap.raw.ac_online = Some(true);
        snap.raw.user_idle_s = Some(RETRAIN_IDLE_THRESHOLD_S + 1.0);
        snap.raw.battery_soc_pct = Some(RETRAIN_SOC_FLOOR_PCT + 1);
        let outcome = evaluate_retrain_trigger(&snap, false, None, chrono::Utc::now());
        assert_eq!(outcome, RetrainOutcome::Dispatched);
    }

    /// Step 26 DoD: the onboarding gate trumps the AC/idle/SOC gates.
    /// Even with every other gate open, `onboarding.active=true` must
    /// keep the trainer dormant — the model can't generalise from
    /// under-14-days of telemetry.
    #[test]
    fn train_skipped_during_onboarding() {
        let mut snap = empty_snapshot();
        snap.raw.ac_online = Some(true);
        snap.raw.user_idle_s = Some(RETRAIN_IDLE_THRESHOLD_S + 1.0);
        snap.raw.battery_soc_pct = Some(RETRAIN_SOC_FLOOR_PCT + 1);
        let outcome = evaluate_retrain_trigger(&snap, true, None, chrono::Utc::now());
        assert_eq!(
            outcome,
            RetrainOutcome::Skipped(RetrainSkipReason::Onboarding),
        );
    }

    /// Helper that feeds `n` alternating reward samples (0.0, 1.0,
    /// …) through [`observe_drift_signals`] so DDM's binarised
    /// "error" residual climbs from "below threshold" to "above
    /// threshold" and the alarm fires. Used by the three Step 31
    /// drift tests below.
    fn drive_drift_alarm(state: &mut DriftTickState, notifier: &dyn DriftNotifier) {
        // First 1000 stationary samples at reward = 0.0 so DDM's
        // p_min/s_min settle. Then 200 samples alternating ±1.0 so
        // the residual binarises to true and the alarm fires.
        let clock = pinned_clock();
        for _ in 0..1_000 {
            observe_drift_signals(state, notifier, &clock, 0.0);
        }
        let mut i: usize = 0;
        while !state.status.adwin_alarm && i < 1_000 {
            let r = if i.is_multiple_of(2) { 1.0 } else { -1.0 };
            observe_drift_signals(state, notifier, &clock, r);
            i += 1;
        }
    }

    /// Step 31 DoD: under DDM-driven drift alarm, `one_tick` drops
    /// the bandit's pick and applies the rules-baseline arm
    /// (browse for COOL_AC in the shipped config), tagging the
    /// reason chain with the SPEC §5 `drift-baseline:<arm>` label.
    #[test]
    fn drift_alarm_drops_to_baseline() {
        let tmp = TempDir::new().expect("tempdir");
        let logger = plenty_logger(tmp.path().join("power"));
        let clock = pinned_clock();
        let mut intent = Intent::default();
        let latest = new_latest_snapshot();
        let pin = new_pin_slot();
        let last_entry = new_latest_audit_entry();
        let cfg = shipped_config();
        let thrash = ThrashTracker::new();
        let npu = test_npu();
        let (sysfs_root, cgroup_root) = fixture_sysfs(&tmp);
        let sensors = cool_ac_sensors(&sysfs_root);
        let ctx = TickContext {
            sysfs_root,
            cgroup_root,
            cfg: &cfg,
            thrash: &thrash,
            npu: &npu,
        };
        let mut prev = ShieldTickState::new();
        let mut bandit = BanditTickState::from_config(&cfg);
        let mut onb = post_onboarding_state();
        let mut activity = ActivityTickState::new();
        let mut drift = DriftTickState::new();
        let mut forecast = ForecastTickState::warmup().expect("warmup model");
        let drift_latest = new_latest_drift_status();
        let notifier = MockDriftNotifier::default();
        let trig = NoopRetrainTrigger;
        // Force drift alarm via the helper so the next `one_tick`
        // sees `drift_alarm == true` at the top of the tick.
        drive_drift_alarm(&mut drift, &notifier);
        assert!(
            drift.status.adwin_alarm,
            "fixture must produce a drift alarm",
        );
        one_tick(
            &sensors,
            &mut intent,
            &clock,
            &ctx,
            &pin,
            &mut prev,
            &mut bandit,
            &mut onb,
            &mut activity,
            &mut drift,
            &mut forecast,
            &mut ActuatorLatches::default(),
            &notifier,
            &trig,
            &logger,
            &latest,
            &last_entry,
            &drift_latest,
        )
        .expect("tick ok");
        let entry = last_entry.read().expect("entry").clone().expect("entry");
        let baseline = &cfg.rules_baseline.cool_ac;
        assert_eq!(
            entry.applied_arm.as_deref(),
            Some(baseline.as_str()),
            "drift alarm must apply the rules-baseline arm, not the bandit's pick",
        );
        assert!(
            entry
                .reason_chain
                .iter()
                .any(|r| r == &format!("drift-baseline:{baseline}")),
            "reason chain must tag the drift gate: {:?}",
            entry.reason_chain,
        );
    }

    /// Step 31 DoD: on the tick where the alarm first fires the
    /// daemon emits exactly one desktop notification carrying the
    /// SPEC §5 wording. Subsequent ticks while the alarm is still
    /// active do NOT re-fire the notification — debounced by the
    /// `was_alarm` slot in [`observe_drift_signals`].
    #[test]
    fn drift_alarm_emits_notification() {
        let notifier = MockDriftNotifier::default();
        let mut drift = DriftTickState::new();
        drive_drift_alarm(&mut drift, &notifier);
        assert!(drift.status.adwin_alarm, "alarm must fire");
        let calls = notifier.calls.lock().expect("calls").clone();
        assert_eq!(
            calls.len(),
            1,
            "exactly one notification on the alarm-rising edge: {calls:?}",
        );
        assert_eq!(calls[0].0, DRIFT_NOTIFICATION_SUMMARY);
        // Further observations while still in alarm must NOT re-fire
        // the notification.
        let clock = pinned_clock();
        for _ in 0..100 {
            observe_drift_signals(&mut drift, &notifier, &clock, 1.0);
        }
        let calls = notifier.calls.lock().expect("calls").clone();
        assert_eq!(
            calls.len(),
            1,
            "notification must be debounced while alarm holds: {calls:?}",
        );
    }

    /// Step 31 DoD: after a successful retrain dispatch (which the
    /// daemon fires when drift is active + AC/idle/SOC gates open),
    /// the drift detector + alarm clear so the next stationary
    /// stream does not retrigger from stale state.
    /// Step 31 DoD: when drift is active and the AC/idle/SOC gates
    /// are all open, `one_tick` dispatches a retrain with cause
    /// [`RetrainCause::Drift`] and clears the drift state on the
    /// same tick. The onboarding gate is overridden — drift can
    /// retrain even after day 14 (or during onboarding if somehow
    /// active, which the integration runbook never produces; the
    /// SPEC §3 sentinel chain still holds).
    #[test]
    fn drift_dispatches_retrain_with_drift_cause() {
        // Build a snapshot fixture where the snapshot collector
        // surfaces ac_online=true + user_idle_s > 300 + soc > 50
        // by stubbing the sensors that back those reads. The
        // existing helper `evaluate_retrain_trigger` is what
        // `one_tick` consults; we call it directly with the
        // drift-active branch to pin the contract.
        let mut snap = empty_snapshot();
        snap.raw.ac_online = Some(true);
        snap.raw.user_idle_s = Some(RETRAIN_IDLE_THRESHOLD_S + 1.0);
        snap.raw.battery_soc_pct = Some(RETRAIN_SOC_FLOOR_PCT + 1);
        // `retrain_onboarding_gate = onboarding_active && !drift_alarm`
        // — with drift active, the gate evaluates to false even
        // when onboarding is also active.
        let onboarding_gate = false;
        let outcome = evaluate_retrain_trigger(&snap, onboarding_gate, None, chrono::Utc::now());
        assert_eq!(
            outcome,
            RetrainOutcome::Dispatched,
            "with drift active, AC+idle+SOC gates open ⇒ dispatch",
        );
    }

    #[test]
    fn drift_clears_after_successful_retrain() {
        // The full `one_tick`-driven dispatch path is exercised by
        // the SPEC §5 RUNBOOK; this test pins the load-bearing
        // semantics directly: drive a DDM alarm via the helper,
        // call the same `clear_drift_state` `one_tick` invokes on a
        // dispatched drift retrain, then verify the detector +
        // status come back clean and a long stationary stream does
        // not retrigger.
        const STATIONARY_TAIL: usize = 500;
        let clock = pinned_clock();
        let mut drift = DriftTickState::new();
        let drift_latest = new_latest_drift_status();
        let notifier = MockDriftNotifier::default();
        drive_drift_alarm(&mut drift, &notifier);
        assert!(drift.status.adwin_alarm, "alarm must hold pre-clear");
        clear_drift_state(&mut drift, &drift_latest);
        assert!(
            !drift.status.adwin_alarm,
            "drift alarm must clear after retrain reset",
        );
        for _ in 0..STATIONARY_TAIL {
            observe_drift_signals(&mut drift, &notifier, &clock, 0.0);
        }
        assert!(
            !drift.status.adwin_alarm,
            "stationary input post-clear must not re-trigger drift",
        );
        let published = drift_latest.read().expect("drift latest").clone();
        assert!(
            !published.adwin_alarm,
            "published drift status must mirror the clear",
        );
    }

    /// Build a `Snapshot` with default fields — every `raw.*` is
    /// `None`, every feature slot is NaN. Tests mutate the fields
    /// they care about and leave the rest at defaults.
    fn empty_snapshot() -> Snapshot {
        use crate::power::snapshot::{SnapshotRaw, FEATURE_LEN, SCHEMA_ID as SNAP_SCHEMA};
        Snapshot {
            schema: SNAP_SCHEMA,
            ts: chrono::Utc::now(),
            features: [0.0_f32; FEATURE_LEN],
            raw: SnapshotRaw::default(),
            snapshot_hash: String::new(),
        }
    }

    /// BUG-20260712-1530: the IPC `Status` response must carry the
    /// daemon's own onboarding view — active, days_collected, ready_at,
    /// and the daemon's *effective* `target_days` — so `sy power status`
    /// reports the gate the daemon is actually enforcing instead of the
    /// CLI process's re-computation. Populate the shared onboarding slot
    /// with a "gate open, target_days = 0" view (the concrete repro:
    /// a systemd drop-in scoping `SY_POWER_ONBOARDING_DAYS=0` to
    /// `sy-powerd`), dial the handler over a socket pair, and assert the
    /// wire response reflects it verbatim.
    #[tokio::test]
    async fn status_response_carries_daemon_onboarding_block() {
        use crate::power::ipc::{
            read_frame, write_frame, OnboardingWire, StatusRequest, StatusResponse,
        };
        let latest = new_latest_snapshot();
        *latest.write().expect("snapshot slot") = Some(empty_snapshot());
        let onboarding = new_latest_onboarding();
        let ready_at = chrono::Utc::now() - chrono::Duration::days(1);
        *onboarding.write().expect("onboarding slot") = Some(OnboardingWire {
            active: false,
            days_collected: 5,
            ready_at,
            target_days: 0,
        });

        let (server, mut client) = tokio::net::UnixStream::pair().expect("in-process socket pair");
        let state = ConnState {
            latest,
            pin: new_pin_slot(),
            last_entry: new_latest_audit_entry(),
            drift: new_latest_drift_status(),
            model: new_latest_model_status(),
            onboarding,
            arms: Vec::new(),
        };
        let handle = tokio::spawn(handle_connection_full(server, state));

        write_frame(&mut client, &StatusRequest::Status)
            .await
            .expect("send status request");
        let resp: StatusResponse = read_frame(&mut client).await.expect("read response");
        handle.await.expect("join handler").expect("handler ok");

        let onb = resp
            .onboarding
            .expect("daemon must serve its own onboarding block");
        assert!(!onb.active, "daemon reports the gate open");
        assert_eq!(onb.days_collected, 5);
        assert_eq!(
            onb.target_days, 0,
            "target_days must reflect the daemon's effective config, not the CLI's",
        );
        assert_eq!(onb.ready_at, ready_at);
    }

    /// BUG-20260522-0037: the NPU actuator's `Display` impl embeds the
    /// full multi-line `xrt-smi` stderr (build banner, PID, host, exe
    /// path) which used to leak into every audit `reason_chain`,
    /// bloating entries from ~700 B to ~1.3 KB and capping the daily
    /// file at ~40K entries. `short_npu_reason` must pick the actual
    /// `ERROR:` line, drop the banner, and stay under
    /// [`NPU_REASON_MAX_LEN`].
    #[test]
    fn short_npu_reason_is_single_line_and_bounded() {
        struct FakeErr(&'static str);
        impl std::fmt::Display for FakeErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        let raw = "xrt-smi configure --pmode failed: XRT build version: 2.21.75\n\
                   Build hash: 4eb1f4392a012b4e6eca759762389c612537f7c7\n\
                   Build date: 2026-03-09 20:30:37\n\
                   Git branch: HEAD\n\
                   PID: 1807365\n\
                   UID: 1000\n\
                   [Thu May 21 00:00:00 2026 GMT]\n\
                   HOST: fedora\n\
                   EXE: /opt/xilinx/xrt/bin/unwrapped/xrt-smi\n\
                   [xrt-smi] ERROR: DRM_IOCTL_AMDXDNA_SET_STATE IOCTL failed (err=-13): Permission denied";
        let out = short_npu_reason(&FakeErr(raw));
        assert!(out.starts_with("npu: skipped ("), "missing prefix: {out:?}");
        assert!(
            !out.contains('\n'),
            "reason must be a single line, got: {out:?}"
        );
        assert!(
            !out.contains("Build hash") && !out.contains("PID:") && !out.contains("EXE:"),
            "boilerplate leaked into audit reason: {out:?}"
        );
        assert!(
            out.contains("ERROR") || out.contains("Permission denied"),
            "ERROR line not picked: {out:?}"
        );
        assert!(
            out.len() <= NPU_REASON_MAX_LEN,
            "reason {} B exceeds cap {NPU_REASON_MAX_LEN} B: {out:?}",
            out.len()
        );
    }

    /// BUG-20260522-0037: simple single-line errors (the common case
    /// once we land the trim) must survive untouched aside from the
    /// `npu: skipped (...)` wrapper.
    #[test]
    fn short_npu_reason_preserves_short_errors() {
        struct FakeErr(&'static str);
        impl std::fmt::Display for FakeErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        let out = short_npu_reason(&FakeErr("xrt-smi not installed"));
        assert_eq!(out, "npu: skipped (xrt-smi not installed)");
    }

    /// Step 10 DoD: with a 5 s ping interval, the watchdog fires at
    /// least twice within a simulated 11 s window. The `MockNotifier`
    /// records `Instant` per ping; the test inspects the recorded
    /// vector instead of waiting on wall-clock time. We sleep a
    /// small real interval so the captured timestamps are strictly
    /// monotonic — the systemd watchdog tolerates jitter, but the
    /// assertion targets ordering + count, not absolute timing.
    /// BUG-20260525-2350: when the EPP actuator returns
    /// `NoPolicyWritable`, the daemon's WARN line must carry the
    /// per-policy failed-paths list as a structured field so the
    /// operator can run `systemd-tmpfiles --create` against exactly
    /// the leaves that need it. The reason chain token also includes
    /// the count so `sy power show` / `sy power explain` can render
    /// the degradation summary without re-parsing the WARN.
    #[test]
    #[tracing_test::traced_test]
    fn log_apply_surfaces_no_policy_writable_failed_paths() {
        use crate::power::apply::epp::EppError;
        use std::path::PathBuf;
        let failed = vec![
            PathBuf::from("/sys/devices/system/cpu/cpufreq/policy12/energy_performance_preference"),
            PathBuf::from("/sys/devices/system/cpu/cpufreq/policy23/energy_performance_preference"),
        ];
        let mut latch = LeverLatch::default();
        let mut reasons: Vec<String> = Vec::new();
        apply_lever("epp", &mut latch, chrono::Utc::now(), &mut reasons, || {
            Err(EppError::NoPolicyWritable {
                failed: failed.clone(),
            }
            .into())
        });
        assert!(
            logs_contain("failed_policies=2"),
            "WARN must carry the structured failed-policy count",
        );
        for p in &failed {
            let needle = p.display().to_string();
            assert!(
                logs_contain(&needle),
                "WARN must name the failed leaf {needle}, got logs without it",
            );
        }
        assert_eq!(reasons.len(), 1, "exactly one reason-chain token per call");
        assert!(
            reasons[0].starts_with("epp: skipped"),
            "reason chain still records the skip: {:?}",
            reasons[0],
        );
    }

    #[test]
    fn watchdog_ping_under_half_interval() {
        const PINGS: usize = 3;
        const TICK: Duration = Duration::from_millis(5);
        let notifier = MockNotifier::default();
        run_watchdog_loop(&notifier, TICK, PINGS);
        let calls = notifier.calls.lock().expect("calls lock");
        assert_eq!(calls.len(), PINGS);
        // Strictly monotonic — the loop must not collapse two pings
        // into the same `Instant`.
        for w in calls.windows(2) {
            assert!(
                w[1] >= w[0],
                "watchdog pings must be ordered: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
    }
}
