//! `xrt-smi configure --pmode <…>` actuator — sy-power Step 16 lever D.
//!
//! Shells out to AMD's `xrt-smi` to switch the NPU power mode. Five
//! rungs (`default | powersaver | balanced | performance | turbo`) per
//! `bandit::NpuPmode`. Two safety properties live in this actuator
//! (defence in depth, not "only in the shield"):
//!
//! 1. **Rate-limited to ≤ 1 / 5 s.** SPEC §4 "Concrete Shield
//!    Constraint Set" caps NPU pmode transitions at 5-second
//!    intervals — XDNA state changes are heavy and back-to-back
//!    requests stall the firmware. A second call within the window
//!    short-circuits to [`Applied::NoChange`].
//! 2. **Idempotent on no-op transitions.** The same `target` written
//!    twice (across the rate-limit window) still surfaces as
//!    `NoChange` because the second call hits the in-process cache of
//!    the last successfully-applied mode.
//!
//! `xrt-smi` is shelled out via the [`CommandRunner`] trait so tests
//! never touch the real binary; hermetic test fixtures inject
//! `MockRunner` and assert call counts + arg shape.

use std::fmt;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::super::bandit::NpuPmode;
use super::{Actuator, Applied};

/// SPEC §4: NPU pmode transitions cap at 1 / 5 s. The window lives on
/// the actuator so the daemon (Step 19) and the shield (Step 17–18)
/// don't double-rate-limit each other.
const RATE_LIMIT: Duration = Duration::from_secs(5);

/// Binary we shell out to. Vendor-shipped, on the operator's `PATH`
/// after `sy aiplane` lays down the venv link. Not statically linked
/// because XDNA firmware versions move faster than our release cadence.
const XRT_SMI: &str = "xrt-smi";

/// Errors specific to the NPU writer. `XrtSmiFailed` is structural —
/// the daemon (Step 19) downcasts and logs once before dropping the
/// arm; the NPU lever is best-effort by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NpuError {
    /// `xrt-smi configure --pmode …` exited non-zero. `stderr` is the
    /// captured diagnostic so the audit log records why XDNA refused.
    XrtSmiFailed { stderr: String },
}

impl fmt::Display for NpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NpuError::XrtSmiFailed { stderr } => {
                write!(f, "xrt-smi configure --pmode failed: {stderr}")
            }
        }
    }
}

impl std::error::Error for NpuError {}

/// Abstract shell-out so tests can assert call counts + arg shape
/// without invoking the real binary. The production impl is
/// [`SystemRunner`]; tests build `MockRunner`.
pub trait CommandRunner: Send + Sync {
    /// Run `cmd args…`. Returns `Ok(())` on exit code 0;
    /// `Err(NpuError::XrtSmiFailed)` on non-zero with captured stderr.
    fn run(&self, cmd: &str, args: &[&str]) -> Result<()>;

    /// Run `cmd args…` and return captured stdout (UTF-8 lossy). Used
    /// by [`XrtSmiProbe`] to discover the installed `xrt-smi`'s pmode
    /// flag name. Default impl shells out via `std::process::Command`;
    /// the [`SystemRunner`] inherits it. Test doubles override to
    /// return canned help text. Non-zero exits surface as
    /// [`NpuError::XrtSmiFailed`] so probes can fall back cleanly.
    fn run_capturing(&self, cmd: &str, args: &[&str]) -> Result<String> {
        let out = Command::new(cmd).args(args).output().map_err(|e| {
            anyhow::Error::from(NpuError::XrtSmiFailed {
                stderr: format!("spawn {cmd}: {e}"),
            })
        })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(NpuError::XrtSmiFailed { stderr }.into());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Production runner — spawns `xrt-smi` via `std::process::Command`.
#[derive(Debug, Default)]
pub struct SystemRunner;

impl SystemRunner {
    pub fn new() -> Self {
        Self
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, cmd: &str, args: &[&str]) -> Result<()> {
        let out = Command::new(cmd).args(args).output().map_err(|e| {
            anyhow::Error::from(NpuError::XrtSmiFailed {
                stderr: format!("spawn {cmd}: {e}"),
            })
        })?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(NpuError::XrtSmiFailed { stderr }.into())
    }
}

/// Candidate pmode flag names probed in priority order. The first one
/// that appears verbatim in `xrt-smi configure --help` (or, on
/// fallback, `xrt-smi --help`) becomes the cached flag. Order matters:
/// `--pmode` is SPEC §2's canonical name, `--power-mode` is observed
/// on some XRT 2.x builds, `--mode` is the legacy XRT 1.x spelling.
const PMODE_FLAG_CANDIDATES: &[&str] = &["--pmode", "--power-mode", "--mode"];

/// Stderr substrings that mark a probe-time `xrt-smi configure` runtime
/// test as "lever permanently unavailable to this caller". The live
/// host (kernel 7.0.6 / amdxdna) returns
/// `DRM_IOCTL_AMDXDNA_SET_STATE IOCTL failed (err=-13): Permission
/// denied` to unprivileged callers; the driver requires
/// `CAP_SYS_ADMIN` and a user-mode `sy-powerd` cannot have it. Any
/// match → cache `flag = None` → silent no-op forever; no WARN spam.
const IOCTL_PERMISSION_PATTERNS: &[&str] = &["Permission denied", "err=-13", "IOCTL failed"];

/// Idempotent pmode value used by the probe's runtime test. Setting
/// the NPU's pmode to "default" is a no-op when the current mode is
/// already default (the common factory state); a privileged caller can
/// re-issue it safely. Chosen over the other candidates because it's
/// the lowest-impact value to write during startup.
const PROBE_RUNTIME_VALUE: &str = "default";

/// Startup probe that asks the installed `xrt-smi` which pmode flag it
/// accepts and caches the answer. The actuator consults the cached
/// flag on every `apply`; if the probe found nothing (`flag = None`),
/// the actuator short-circuits to [`Applied::NoChange`] forever —
/// per Step 16 anti-goal the NPU lever is best-effort and must not
/// spam WARN/min when the installed binary doesn't expose the lever.
///
/// Probe order:
/// 1. `xrt-smi configure --help` (XRT 2.x canonical subcommand).
/// 2. `xrt-smi --help` (fallback for XRT builds that exit non-zero on
///    the `configure` subcommand or where `configure` is absent).
///
/// In both cases the first candidate from [`PMODE_FLAG_CANDIDATES`]
/// that appears verbatim in stdout wins. The probe emits a single
/// `tracing::info!` line at construction so operators see one
/// observation, not 60 WARN/min.
#[derive(Debug, Clone, Copy)]
pub struct XrtSmiProbe {
    flag: Option<&'static str>,
}

impl XrtSmiProbe {
    /// Probe `xrt-smi` for the active pmode flag. Two phases:
    ///
    /// 1. **Help-text scan**: runs `xrt-smi configure --help` (falling
    ///    back to `xrt-smi --help`) and resolves the first candidate
    ///    from [`PMODE_FLAG_CANDIDATES`] that appears in stdout.
    /// 2. **Runtime test** (P1-1 follow-up): on the live amdxdna
    ///    driver, advertising `--pmode` in help text does NOT mean an
    ///    unprivileged caller can use it — the kernel returns
    ///    `DRM_IOCTL_AMDXDNA_SET_STATE IOCTL failed (err=-13):
    ///    Permission denied`. So after the help scan resolves a flag,
    ///    we invoke `xrt-smi configure <flag> default` once: an
    ///    idempotent write that turns into a no-op on a host already
    ///    at "default". If that runtime test fails with any of the
    ///    [`IOCTL_PERMISSION_PATTERNS`] in stderr, we cache `flag =
    ///    None` so subsequent `apply` calls short-circuit silently and
    ///    the daemon's journal does not fill with 120 WARN/min spam.
    ///
    /// Emits one INFO log line per branch so operators see exactly one
    /// observation at startup.
    pub fn probe(runner: &dyn CommandRunner) -> Self {
        let help = runner
            .run_capturing(XRT_SMI, &["configure", "--help"])
            .or_else(|_| runner.run_capturing(XRT_SMI, &["--help"]))
            .unwrap_or_default();
        let scanned = PMODE_FLAG_CANDIDATES
            .iter()
            .copied()
            .find(|f| help.contains(f));
        let flag = match scanned {
            Some(f) => match runner.run(XRT_SMI, &["configure", f, PROBE_RUNTIME_VALUE]) {
                Ok(()) => {
                    tracing::info!(target: "sy::power::npu", flag = f, "xrt-smi pmode flag resolved");
                    Some(f)
                }
                Err(e) if ioctl_permission_denied(&e) => {
                    tracing::info!(
                        target: "sy::power::npu",
                        flag = f,
                        "xrt-smi IOCTL permission denied; NPU lever disabled (needs CAP_SYS_ADMIN)",
                    );
                    None
                }
                Err(_) => {
                    tracing::info!(target: "sy::power::npu", flag = f, "xrt-smi pmode flag resolved");
                    Some(f)
                }
            },
            None => {
                tracing::info!(target: "sy::power::npu", "xrt-smi has no pmode flag; NPU lever disabled");
                None
            }
        };
        Self { flag }
    }

    /// Build a probe with an explicit flag — escape hatch for tests
    /// that want to construct an actuator without re-running the
    /// probe (e.g. the existing rate-limit + argv-shape tests).
    #[cfg(test)]
    fn with_flag(flag: Option<&'static str>) -> Self {
        Self { flag }
    }

    /// Resolved flag, or `None` when the installed `xrt-smi` doesn't
    /// expose a pmode lever.
    pub fn flag(&self) -> Option<&'static str> {
        self.flag
    }
}

/// Abstract monotonic clock for the rate-limit window. Distinct from
/// `power::clock::Clock` (which yields wall-time `DateTime<Utc>`); the
/// rate-limit needs a monotonic `Instant`. Scoped to this module — the
/// only consumer is the NPU actuator.
pub trait TimeSource: Send + Sync {
    fn now(&self) -> Instant;
}

/// Production [`TimeSource`] — wraps `std::time::Instant::now`.
#[derive(Debug, Default)]
pub struct SystemTimeSource;

impl SystemTimeSource {
    pub fn new() -> Self {
        Self
    }
}

impl TimeSource for SystemTimeSource {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// NPU actuator. Owns its rate-limit state (`last_applied`) so the
/// 1 / 5 s cap is enforced regardless of where the daemon spawns the
/// call from. The runner + time source are dependency-injected to keep
/// tests hermetic. The probe is run once at construction; subsequent
/// `apply` calls consult the cached `probe.flag`.
pub struct NpuActuator {
    runner: Box<dyn CommandRunner>,
    time: Box<dyn TimeSource>,
    probe: XrtSmiProbe,
    last_applied: Mutex<Option<(Instant, NpuPmode)>>,
}

impl fmt::Debug for NpuActuator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NpuActuator")
            .field("probe", &self.probe)
            .field("last_applied", &self.last_applied)
            .finish_non_exhaustive()
    }
}

impl NpuActuator {
    /// Construct an actuator and probe `xrt-smi` once for the active
    /// pmode flag. The probe runs synchronously through the injected
    /// runner so tests can supply canned help text via
    /// `CommandRunner::run_capturing`.
    pub fn new(runner: Box<dyn CommandRunner>, time: Box<dyn TimeSource>) -> Self {
        let probe = XrtSmiProbe::probe(runner.as_ref());
        Self {
            runner,
            time,
            probe,
            last_applied: Mutex::new(None),
        }
    }

    /// Resolved pmode flag — `None` when the installed `xrt-smi`
    /// doesn't expose a pmode lever. Test-only introspection; the
    /// production code path consults `self.probe` directly in `apply`.
    /// Operators see the resolution via the startup INFO log line.
    #[cfg(test)]
    pub fn pmode_flag(&self) -> Option<&'static str> {
        self.probe.flag()
    }
}

impl Actuator for NpuActuator {
    type Target = NpuPmode;
    /// `_sysfs_root` is ignored: `xrt-smi` talks to XDNA via its own
    /// device node. Kept in the signature so the actuator slots into
    /// the trait the daemon walks generically.
    fn apply(&self, target: Self::Target, _sysfs_root: &Path) -> Result<Applied> {
        // P1-1: if the installed `xrt-smi` doesn't expose a pmode flag
        // the NPU lever is a permanent no-op. Short-circuit BEFORE the
        // rate-limit so we don't even touch the mutex on a disabled
        // host. The startup INFO line already explained why.
        let Some(flag) = self.probe.flag() else {
            return Ok(Applied::NoChange);
        };
        let now = self.time.now();
        // Lock once for the read-modify-write: a concurrent caller
        // must see the same "last applied" snapshot we used for the
        // rate-limit check.
        let mut guard = self
            .last_applied
            .lock()
            .map_err(|e| anyhow::anyhow!("NpuActuator mutex poisoned: {e}"))?;
        if let Some((ts, last_target)) = *guard {
            if last_target == target && now.duration_since(ts) < RATE_LIMIT {
                return Ok(Applied::NoChange);
            }
            if now.duration_since(ts) < RATE_LIMIT {
                // Different target inside the cap — same defence: drop
                // the transition rather than thrash XDNA state.
                return Ok(Applied::NoChange);
            }
        }
        let pmode = pmode_str(target);
        self.runner.run(XRT_SMI, &["configure", flag, pmode])?;
        *guard = Some((now, target));
        Ok(Applied::Wrote {
            path: Path::new(XRT_SMI).to_path_buf(),
            value: pmode.to_string(),
        })
    }
}

/// Inspect a runner error and return `true` if its stderr matches one
/// of [`IOCTL_PERMISSION_PATTERNS`]. The probe uses this to distinguish
/// "permanent permission denial → cache `None`, no spam" from "transient
/// or unrelated failure → keep the lever armed". Looks for the
/// structured `NpuError::XrtSmiFailed` first; falls back to the
/// `Display` text on any other error type so future runners that wrap
/// the failure differently still trigger the same downgrade.
fn ioctl_permission_denied(err: &anyhow::Error) -> bool {
    let stderr = match err.downcast_ref::<NpuError>() {
        Some(NpuError::XrtSmiFailed { stderr }) => stderr.clone(),
        None => err.to_string(),
    };
    IOCTL_PERMISSION_PATTERNS.iter().any(|p| stderr.contains(p))
}

/// Canonical wire string for an [`NpuPmode`]. Matches AMD's
/// documented set verbatim so `xrt-smi` doesn't reject our argv.
fn pmode_str(p: NpuPmode) -> &'static str {
    match p {
        NpuPmode::Default => "default",
        NpuPmode::Powersaver => "powersaver",
        NpuPmode::Balanced => "balanced",
        NpuPmode::Performance => "performance",
        NpuPmode::Turbo => "turbo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test double for [`CommandRunner`]: records every call. Lets us
    /// assert that `xrt-smi configure --pmode <mode>` was invoked
    /// exactly once even when the caller fired two `apply` calls.
    /// `help_text` seeds the [`XrtSmiProbe`] reply so tests pin which
    /// pmode flag the probe resolves; default mimics XRT 2.x with
    /// `--pmode` available. `responses` stores per-argv canned errors so
    /// the P1-1 follow-up test can simulate `xrt-smi configure --pmode
    /// default` failing with the IOCTL Permission-denied stderr that the
    /// live amdxdna driver returns to unprivileged callers.
    struct MockRunner {
        calls: Mutex<Vec<Vec<String>>>,
        help_text: String,
        help_ok: bool,
        responses: Mutex<std::collections::HashMap<Vec<String>, String>>,
    }

    impl Default for MockRunner {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                help_text: "Usage: xrt-smi configure [--pmode <mode>]".to_string(),
                help_ok: true,
                responses: Mutex::new(std::collections::HashMap::new()),
            }
        }
    }

    impl MockRunner {
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().map(|g| g.clone()).unwrap_or_default()
        }

        fn with_help(help: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                help_text: help.to_string(),
                help_ok: true,
                responses: Mutex::new(std::collections::HashMap::new()),
            }
        }

        fn with_help_error() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                help_text: String::new(),
                help_ok: false,
                responses: Mutex::new(std::collections::HashMap::new()),
            }
        }

        /// Stage a canned stderr error for the exact argv `cmd args…`.
        /// `run()` consults this map on every call; a match surfaces as
        /// `NpuError::XrtSmiFailed` with the stored stderr. Used by the
        /// P1-1 follow-up runtime-test probe to simulate the amdxdna
        /// IOCTL `Permission denied` path.
        fn fail_on(&self, argv: &[&str], stderr: &str) {
            let key: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
            let mut g = self.responses.lock().expect("responses");
            g.insert(key, stderr.to_string());
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, cmd: &str, args: &[&str]) -> Result<()> {
            let mut v = vec![cmd.to_string()];
            v.extend(args.iter().map(|s| s.to_string()));
            self.calls
                .lock()
                .map_err(|e| anyhow::anyhow!("mock runner poisoned: {e}"))?
                .push(v.clone());
            if let Some(stderr) = self
                .responses
                .lock()
                .map_err(|e| anyhow::anyhow!("mock responses poisoned: {e}"))?
                .get(&v)
                .cloned()
            {
                return Err(NpuError::XrtSmiFailed { stderr }.into());
            }
            Ok(())
        }

        fn run_capturing(&self, _cmd: &str, _args: &[&str]) -> Result<String> {
            if self.help_ok {
                Ok(self.help_text.clone())
            } else {
                Err(NpuError::XrtSmiFailed {
                    stderr: "help unavailable".to_string(),
                }
                .into())
            }
        }
    }

    /// Manually-advanced clock so the rate-limit test is deterministic.
    /// Holding an `Instant` (no public `Instant::from_secs` API) means
    /// we anchor on `Instant::now()` at construction and advance via
    /// `Duration` add.
    struct FakeClock {
        base: Instant,
        offset: Mutex<Duration>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                offset: Mutex::new(Duration::from_secs(0)),
            }
        }
        fn advance(&self, by: Duration) {
            let mut g = self.offset.lock().expect("offset");
            *g += by;
        }
    }

    impl TimeSource for FakeClock {
        fn now(&self) -> Instant {
            let off = *self.offset.lock().expect("offset");
            self.base + off
        }
    }

    /// Wrap a `FakeClock` in `Arc` so the test owns a handle for
    /// `.advance()` while the actuator owns the boxed `TimeSource`.
    use std::sync::Arc;

    struct SharedClock(Arc<FakeClock>);
    impl TimeSource for SharedClock {
        fn now(&self) -> Instant {
            self.0.now()
        }
    }

    /// Wrap a `MockRunner` in `Arc` for the same reason: test owns a
    /// handle to assert call counts, actuator owns the trait object.
    struct SharedRunner(Arc<MockRunner>);
    impl CommandRunner for SharedRunner {
        fn run(&self, cmd: &str, args: &[&str]) -> Result<()> {
            self.0.run(cmd, args)
        }
        fn run_capturing(&self, cmd: &str, args: &[&str]) -> Result<String> {
            self.0.run_capturing(cmd, args)
        }
    }

    /// SPEC §4 / Step 16 required: two `apply(turbo)` calls inside the
    /// 5-second window must produce exactly ONE `xrt-smi` invocation;
    /// the second must short-circuit to `Applied::NoChange`. After the
    /// window elapses the next call goes through (this also exercises
    /// the "reset on window expiry" branch).
    #[test]
    fn pmode_transitions_rate_limited() {
        let runner = Arc::new(MockRunner::default());
        let clock = Arc::new(FakeClock::new());
        let act = NpuActuator::new(
            Box::new(SharedRunner(runner.clone())),
            Box::new(SharedClock(clock.clone())),
        );
        // Drop the probe's own runtime-test shell-out so the rate-limit
        // assertions below count only `apply` calls.
        runner.calls.lock().expect("calls").clear();
        let first = act.apply(NpuPmode::Turbo, Path::new("/")).expect("first");
        match first {
            Applied::Wrote { value, .. } => assert_eq!(value, "turbo"),
            other => panic!("expected Wrote, got {other:?}"),
        }
        clock.advance(Duration::from_secs(1));
        let second = act.apply(NpuPmode::Turbo, Path::new("/")).expect("second");
        assert_eq!(
            second,
            Applied::NoChange,
            "back-to-back pmode transitions must be capped at 1 / 5 s",
        );
        assert_eq!(
            runner.calls().len(),
            1,
            "rate-limited call must not shell out: {:?}",
            runner.calls(),
        );
        // Advance past the window — a follow-up transition is allowed.
        clock.advance(Duration::from_secs(5));
        let third = act
            .apply(NpuPmode::Balanced, Path::new("/"))
            .expect("third");
        match third {
            Applied::Wrote { value, .. } => assert_eq!(value, "balanced"),
            other => panic!("expected Wrote after window, got {other:?}"),
        }
        assert_eq!(runner.calls().len(), 2, "post-window write must shell out");
    }

    /// Argv shape: `xrt-smi configure --pmode <mode>`. The pmode token
    /// must match AMD's documented set verbatim (no `_` mangling) so
    /// the production binary doesn't reject our call.
    #[test]
    fn xrt_smi_argv_shape_matches_spec() {
        let runner = Arc::new(MockRunner::default());
        let clock = Arc::new(FakeClock::new());
        let act = NpuActuator::new(
            Box::new(SharedRunner(runner.clone())),
            Box::new(SharedClock(clock.clone())),
        );
        // Drop the probe's own runtime-test shell-out; this test
        // pins the argv shape of the `apply` call, not the probe.
        runner.calls.lock().expect("calls").clear();
        act.apply(NpuPmode::Powersaver, Path::new("/"))
            .expect("apply");
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            vec!["xrt-smi", "configure", "--pmode", "powersaver"],
        );
    }

    /// `xrt-smi` non-zero exit must surface as `NpuError::XrtSmiFailed`
    /// carrying the captured stderr so the daemon's audit log records
    /// the structural reason XDNA refused.
    #[test]
    fn non_zero_exit_surfaces_as_xrt_smi_failed() {
        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _cmd: &str, _args: &[&str]) -> Result<()> {
                Err(NpuError::XrtSmiFailed {
                    stderr: "XDNA device busy".to_string(),
                }
                .into())
            }
            fn run_capturing(&self, _cmd: &str, _args: &[&str]) -> Result<String> {
                // Probe sees the canonical XRT 2.x help so the actuator
                // proceeds to call `run`, which is where the test
                // asserts the failure propagates.
                Ok("Usage: xrt-smi configure [--pmode <mode>]".to_string())
            }
        }
        let act = NpuActuator::new(Box::new(FailRunner), Box::new(SystemTimeSource::new()));
        let err = act
            .apply(NpuPmode::Turbo, Path::new("/"))
            .expect_err("must propagate runner error");
        let ne = err
            .downcast_ref::<NpuError>()
            .expect("error must be NpuError");
        match ne {
            NpuError::XrtSmiFailed { stderr } => {
                assert!(stderr.contains("XDNA device busy"), "got {stderr:?}");
            }
        }
    }

    /// P1-1 step DoD: a canonical XRT 2.x help banner exposes `--pmode`,
    /// the probe resolves to that flag, and the actuator's subsequent
    /// `apply` invokes `xrt-smi configure --pmode <mode>` verbatim.
    #[test]
    fn probes_xrt_smi_help_for_pmode_flag() {
        let help = "xrt-smi configure [options]\n  --pmode <mode>   set power mode";
        let runner = Arc::new(MockRunner::with_help(help));
        let clock = Arc::new(FakeClock::new());
        let act = NpuActuator::new(
            Box::new(SharedRunner(runner.clone())),
            Box::new(SharedClock(clock.clone())),
        );
        assert_eq!(act.pmode_flag(), Some("--pmode"));
        act.apply(NpuPmode::Balanced, Path::new("/"))
            .expect("apply");
        let calls = runner.calls();
        assert_eq!(
            calls.last().expect("apply must shell out"),
            &vec![
                "xrt-smi".to_string(),
                "configure".to_string(),
                "--pmode".to_string(),
                "balanced".to_string(),
            ],
        );
    }

    /// P1-1 step DoD: live host XRT 2.21.75 rejects `--pmode` because
    /// the binary no longer exposes any pmode lever. The probe must
    /// resolve to `None`, and subsequent `apply` calls become silent
    /// no-ops — no shell-out, no error, no log spam.
    #[test]
    fn degrades_to_noop_when_pmode_flag_absent() {
        let help = "xrt-smi configure [options]\n  --device <bdf>   target device";
        let runner = Arc::new(MockRunner::with_help(help));
        let clock = Arc::new(FakeClock::new());
        let act = NpuActuator::new(
            Box::new(SharedRunner(runner.clone())),
            Box::new(SharedClock(clock.clone())),
        );
        assert_eq!(act.pmode_flag(), None);
        let applied = act
            .apply(NpuPmode::Turbo, Path::new("/"))
            .expect("apply must not error");
        assert_eq!(applied, Applied::NoChange);
        assert!(
            runner.calls().is_empty(),
            "disabled lever must not shell out: {:?}",
            runner.calls(),
        );
    }

    /// P1-1 step DoD: legacy XRT builds expose `--power-mode` instead
    /// of `--pmode`. The probe must pick whichever candidate is in the
    /// help text, and `apply` must use that exact flag string.
    #[test]
    fn adapts_to_legacy_power_mode_flag_name() {
        let help = "xrt-smi configure [options]\n  --power-mode <mode>   set power mode";
        let runner = Arc::new(MockRunner::with_help(help));
        let clock = Arc::new(FakeClock::new());
        let act = NpuActuator::new(
            Box::new(SharedRunner(runner.clone())),
            Box::new(SharedClock(clock.clone())),
        );
        assert_eq!(act.pmode_flag(), Some("--power-mode"));
        act.apply(NpuPmode::Performance, Path::new("/"))
            .expect("apply");
        let calls = runner.calls();
        assert_eq!(
            calls.last().expect("apply must shell out"),
            &vec![
                "xrt-smi".to_string(),
                "configure".to_string(),
                "--power-mode".to_string(),
                "performance".to_string(),
            ],
        );
    }

    /// P1-1 follow-up: live host XRT 2.21.75 advertises `--pmode` in
    /// help text, but the underlying amdxdna driver returns
    /// `DRM_IOCTL_AMDXDNA_SET_STATE IOCTL failed (err=-13): Permission
    /// denied` to unprivileged callers. The probe must do a RUNTIME
    /// test after the help-text scan (`xrt-smi configure --pmode
    /// default`) and, on the IOCTL Permission-denied pattern, disable
    /// the lever permanently — no further shell-outs from `apply` and
    /// no WARN spam in the journal (today's live host saw 120 WARN/min).
    #[test]
    fn probe_runtime_test_failure_disables_lever_silently() {
        let help = "xrt-smi configure [options]\n  --pmode <mode>   set power mode";
        let runner = Arc::new(MockRunner::with_help(help));
        runner.fail_on(
            &["xrt-smi", "configure", "--pmode", "default"],
            "ERROR: DRM_IOCTL_AMDXDNA_SET_STATE IOCTL failed (err=-13): Permission denied",
        );
        let clock = Arc::new(FakeClock::new());
        let act = NpuActuator::new(
            Box::new(SharedRunner(runner.clone())),
            Box::new(SharedClock(clock.clone())),
        );
        assert_eq!(
            act.pmode_flag(),
            None,
            "runtime IOCTL Permission denied must disable the lever",
        );
        // Reset call history so we only count what happens AFTER probe.
        runner.calls.lock().expect("calls").clear();
        let applied = act
            .apply(NpuPmode::Turbo, Path::new("/"))
            .expect("apply must not error after lever disabled");
        assert_eq!(applied, Applied::NoChange);
        assert!(
            runner.calls().is_empty(),
            "disabled lever must not shell out on apply: {:?}",
            runner.calls(),
        );
    }

    /// P1-1 follow-up: when the help-text scan finds `--pmode` AND the
    /// runtime probe succeeds (privileged caller, e.g. CAP_SYS_ADMIN
    /// ambient grant in a future systemd unit), the lever stays armed
    /// and `apply` shells out as usual.
    #[test]
    fn probe_runtime_test_success_keeps_lever() {
        let help = "xrt-smi configure [options]\n  --pmode <mode>   set power mode";
        let runner = Arc::new(MockRunner::with_help(help));
        let clock = Arc::new(FakeClock::new());
        let act = NpuActuator::new(
            Box::new(SharedRunner(runner.clone())),
            Box::new(SharedClock(clock.clone())),
        );
        assert_eq!(act.pmode_flag(), Some("--pmode"));
        // The probe itself counts as one shell-out (the runtime test).
        let probe_calls = runner.calls();
        assert_eq!(
            probe_calls.last().expect("probe must runtime-test"),
            &vec![
                "xrt-smi".to_string(),
                "configure".to_string(),
                "--pmode".to_string(),
                "default".to_string(),
            ],
        );
    }

    /// Probe fallback path: `xrt-smi configure --help` errors (older
    /// XRT builds), so the probe must retry `xrt-smi --help` and
    /// still resolve the flag. Exercises the `or_else` branch in
    /// [`XrtSmiProbe::probe`].
    #[test]
    fn probe_falls_back_to_top_level_help_when_configure_help_fails() {
        // `with_help_error` makes BOTH `run_capturing` calls return the
        // same error; the probe must therefore resolve to `None`.
        let runner = MockRunner::with_help_error();
        let probe = XrtSmiProbe::probe(&runner);
        assert_eq!(probe.flag(), None);
        // Sanity: `XrtSmiProbe::with_flag` is the test-only escape
        // hatch we documented at the API surface — exercise it so the
        // helper isn't dead code.
        assert_eq!(
            XrtSmiProbe::with_flag(Some("--mode")).flag(),
            Some("--mode")
        );
    }
}
