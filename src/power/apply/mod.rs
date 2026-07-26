//! `sy power apply` — installer (Step 13) plus the actuator surface
//! (Step 15) shared by every per-knob writer under `apply/<knob>.rs`.
//!
//! R1 cut: the three install artifacts (polkit rule, user systemd
//! unit, telemetry dir). R4 (Step 27) adds the grub drop-in for
//! `amd_dynamic_epp=disable`; R7 (Step 37) extends `apply` with the
//! PPD replacement shim. PPD detection lives here from R1 because we
//! need to *warn without disabling* — Step 36's shim is the only path
//! that touches PPD.
//!
//! Step 15 layers the [`Actuator`] trait + [`Applied`] outcome enum +
//! [`write_if_changed`] diff helper on top, so the platform-profile
//! and EPP writers (next door) ship as thin `Actuator` impls that
//! reuse a single read-compare-write primitive. Idempotent by
//! construction — a write that would match the current sysfs value is
//! short-circuited to [`Applied::NoChange`].
//!
//! Polkit story for Step 15: the rule from Step 13 grants
//! `org.sy.PowerProfile.SetProfile` to `wheel`. Step 36 wires that
//! into a D-Bus path; until then the actuator writes the sysfs node
//! directly and relies on the kernel-shipped `wheel:wheel rw-rw-r--`
//! mode on `/sys/firmware/acpi/platform_profile` (Fedora 43 +).
//!
//! Split into submodules because the install logic is non-trivial
//! (file diffing + idempotency + PPD detection) and we want each
//! actuator testable in isolation from the CLI plumbing in `cli.rs`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

pub mod cgroup;
pub mod epp;
pub mod igpu;
pub mod installer;
pub mod npu;
pub mod platform;

pub use cgroup::CgroupActuator;
pub use epp::EppActuator;
pub use igpu::IgpuActuator;
pub use installer::{install, ChangeRecord, InstallOpts, SystemRunner as InstallerSystemRunner};
pub use npu::{NpuActuator, SystemRunner, SystemTimeSource};
pub use platform::PlatformProfileActuator;

/// Outcome of one actuator write. Carries the touched sysfs path +
/// the value we wrote so the audit log (Step 23) can render a
/// deterministic "what changed" line; `NoChange` is the idempotent
/// re-apply marker mirroring [`ChangeRecord::AlreadyMatches`] on the
/// installer side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// We wrote `value` to `path`. The previous content (if any)
    /// differed; idempotency contract: a follow-up call with the
    /// same target must return [`Applied::NoChange`].
    Wrote { path: PathBuf, value: String },
    /// Sysfs already reported `value`; no write was performed.
    NoChange,
}

/// One writable knob. Implementors take a strongly-typed target
/// (`PlatformProfile`, `Epp`, …) and a `sysfs_root` so tests can
/// point at a tempdir mirror of `/sys`.
///
/// The trait is intentionally small: every actuator returns an
/// [`Applied`], never a raw `bool` / `()`, so the daemon can audit
/// the decision and the test fixtures can assert on the exact path
/// touched.
pub trait Actuator {
    /// The strongly-typed knob value (e.g. `PlatformProfile`).
    type Target;
    /// Apply `target` against `sysfs_root`. Idempotent: identical
    /// `target` on identical sysfs state must yield [`Applied::NoChange`].
    fn apply(&self, target: Self::Target, sysfs_root: &Path) -> Result<Applied>;
}

/// SPEC §4 NFR Reliability vendor defaults. On daemon crash / SIGTERM
/// / panic the daemon must hand the host back to the OEM's auto-mode
/// pair before exit — otherwise an aborted bandit pin (e.g.
/// `performance` + `flat-out`) would persist across the restart and
/// run the chassis hot until the operator notices. Both values are
/// the kernel-shipped defaults on Fedora 43 + every other distro the
/// SPEC §1 hardware list calls out.
const VENDOR_DEFAULT_PROFILE: &str = "balanced";
const VENDOR_DEFAULT_EPP: &str = "balance_performance";

/// Sysfs path of the platform_profile knob, relative to `sysfs_root`.
/// Duplicated from `apply/platform.rs` so the crash-safe helper can
/// live above the per-actuator modules without a circular import.
const VENDOR_DEFAULT_PROFILE_PATH: &str = "firmware/acpi/platform_profile";
/// cpufreq policy root under `sysfs_root`. Each `policy<N>/` carries
/// the EPP leaf — mirrors `apply/epp.rs::CPUFREQ_DIR`.
const VENDOR_DEFAULT_EPP_DIR: &str = "devices/system/cpu/cpufreq";
const VENDOR_DEFAULT_EPP_LEAF: &str = "energy_performance_preference";

/// Write the vendor-default `platform_profile=balanced` and
/// `energy_performance_preference=balance_performance` synchronously.
/// Called from the daemon's `Drop` impl and `panic::set_hook` so a
/// crash never leaves the host pinned at `performance`/`flat-out`.
///
/// Best-effort by design: every write swallows its I/O error rather
/// than propagate it. The daemon is on its way out; a missing sysfs
/// node (containers, partial fixture trees, vendor mismatch) must not
/// abort the rest of the cleanup.
pub fn crash_safe_exit_defaults(sysfs_root: &Path) {
    let _ = fs::write(
        sysfs_root.join(VENDOR_DEFAULT_PROFILE_PATH),
        VENDOR_DEFAULT_PROFILE,
    );
    let cpufreq = sysfs_root.join(VENDOR_DEFAULT_EPP_DIR);
    let Ok(entries) = fs::read_dir(&cpufreq) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("policy") {
            continue;
        }
        let _ = fs::write(path.join(VENDOR_DEFAULT_EPP_LEAF), VENDOR_DEFAULT_EPP);
    }
}

/// Shared diff primitive — read the current sysfs value, compare,
/// only write on mismatch. `new` is written verbatim (no trailing
/// newline appended) because the kernel-side parsers accept either
/// form; reading back may include a `\n` so we compare on `trim()`.
///
/// Bubble I/O errors via `anyhow` so call sites get path context
/// without an extra `with_context`. ENOENT on the read is mapped to
/// "missing target", which is a hard error — actuators are wired up
/// only against knobs the matching `Sensor` confirmed are present.
pub fn write_if_changed(path: &Path, new: &str) -> Result<Applied> {
    let current =
        fs::read_to_string(path).with_context(|| format!("read {} for diff", path.display()))?;
    if current.trim() == new.trim() {
        return Ok(Applied::NoChange);
    }
    fs::write(path, new).with_context(|| format!("write {}", path.display()))?;
    Ok(Applied::Wrote {
        path: path.to_path_buf(),
        value: new.to_string(),
    })
}

/// Initial retry backoff after a lever first enters the failed state.
/// BUG-20260712-* Problem B: a persistently failing actuator (the iGPU
/// one on this host) used to retry and WARN every 1 Hz tick forever.
pub const LATCH_INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Backoff ceiling — a persistently failing lever is retried at most
/// once per this interval, so the journal never sees more than one WARN
/// per minute per lever even in the worst case.
pub const LATCH_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// What [`LeverLatch::step`] did this tick. The caller (`daemon::apply_arm`)
/// maps each variant to at most one journal line + exactly one
/// reason-chain token, so the audit log records the lever state every
/// tick while the journal only sees the failure/recovery *edges*.
pub enum LatchOutcome {
    /// Write attempted and succeeded; the lever was already healthy.
    Ok(Applied),
    /// Write attempted and succeeded after a failed spell — recovery
    /// edge. Caller logs a single INFO.
    Recovered(Applied),
    /// Write attempted and failed for the first time — failure edge.
    /// Caller logs a single WARN. Carries the error.
    Failed(anyhow::Error),
    /// Write attempted and failed again while already latched. Caller
    /// stays silent (the entry WARN already fired). Carries the error
    /// so the reason chain still names the failure.
    StillFailed(anyhow::Error),
    /// Inside the backoff window — no write was attempted this tick.
    /// Caller records a cheap reason token; no journal line.
    Skipped { backoff_secs: u64 },
}

/// Per-lever failure latch (BUG-20260712-* Problem B). Suppresses the
/// per-tick WARN storm from a persistently failing actuator: it logs
/// once on entering the failed state, retries on an exponential backoff
/// capped at [`LATCH_MAX_BACKOFF`], and logs once on recovery. The
/// backoff clock is the injected [`crate::power::clock::Clock`]'s
/// `DateTime<Utc>` so the state machine is hermetically testable.
#[derive(Debug, Default)]
pub struct LeverLatch {
    failed: bool,
    backoff: Duration,
    next_retry_at: Option<DateTime<Utc>>,
}

impl LeverLatch {
    /// Drive one tick of the latch. When healthy (or once the backoff
    /// window has elapsed) `attempt` is called and its result decides
    /// the edge; while latched inside the backoff window `attempt` is
    /// **not** called (no sysfs I/O) and [`LatchOutcome::Skipped`] is
    /// returned.
    pub fn step(
        &mut self,
        now: DateTime<Utc>,
        attempt: impl FnOnce() -> Result<Applied>,
    ) -> LatchOutcome {
        if self.failed {
            if let Some(next) = self.next_retry_at {
                if now < next {
                    return LatchOutcome::Skipped {
                        backoff_secs: self.backoff.as_secs(),
                    };
                }
            }
        }
        match attempt() {
            Ok(applied) => {
                let was_failed = self.failed;
                self.reset();
                if was_failed {
                    LatchOutcome::Recovered(applied)
                } else {
                    LatchOutcome::Ok(applied)
                }
            }
            Err(e) => {
                if self.failed {
                    self.backoff = (self.backoff * 2).min(LATCH_MAX_BACKOFF);
                    self.next_retry_at = Some(now + backoff_chrono(self.backoff));
                    LatchOutcome::StillFailed(e)
                } else {
                    self.failed = true;
                    self.backoff = LATCH_INITIAL_BACKOFF;
                    self.next_retry_at = Some(now + backoff_chrono(self.backoff));
                    LatchOutcome::Failed(e)
                }
            }
        }
    }

    fn reset(&mut self) {
        self.failed = false;
        self.backoff = Duration::ZERO;
        self.next_retry_at = None;
    }
}

/// Convert a `std::time::Duration` backoff into a `chrono::Duration`,
/// clamping to [`LATCH_MAX_BACKOFF`] on the (unreachable) conversion
/// overflow so `next_retry_at` is always a sane future instant.
fn backoff_chrono(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d)
        .unwrap_or_else(|_| chrono::Duration::seconds(LATCH_MAX_BACKOFF.as_secs() as i64))
}

/// Owns one [`LeverLatch`] per actuator lever, keyed by lever name.
/// Lives across ticks in the daemon's tick loop so the backoff state
/// survives; `Default`-constructed levers start healthy.
#[derive(Debug, Default)]
pub struct ActuatorLatches {
    levers: HashMap<&'static str, LeverLatch>,
}

impl ActuatorLatches {
    /// Borrow (creating on first use) the latch for `lever`.
    pub fn lever(&mut self, lever: &'static str) -> &mut LeverLatch {
        self.levers.entry(lever).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    /// BUG-20260712-* Problem B: a persistently failing lever must WARN
    /// exactly once on entry, skip the sysfs write while backing off
    /// (exponential, capped at 60 s), and surface a single recovery
    /// edge when the write finally succeeds.
    #[test]
    fn lever_latch_logs_once_backs_off_and_recovers() {
        use std::cell::Cell;
        let t0 = chrono::Utc
            .with_ymd_and_hms(2026, 7, 12, 0, 0, 0)
            .single()
            .unwrap();
        let mut latch = LeverLatch::default();
        let attempts = Cell::new(0_u32);
        let fail = |attempts: &Cell<u32>| {
            attempts.set(attempts.get() + 1);
            Err::<Applied, anyhow::Error>(anyhow::anyhow!("boom"))
        };

        // First failure → Failed edge (caller WARNs), backoff = 1 s.
        assert!(matches!(
            latch.step(t0, || fail(&attempts)),
            LatchOutcome::Failed(_)
        ));
        assert_eq!(attempts.get(), 1);

        // Still inside the 1 s window → Skipped, no attempt made.
        assert!(matches!(
            latch.step(t0, || fail(&attempts)),
            LatchOutcome::Skipped { backoff_secs: 1 }
        ));
        assert_eq!(attempts.get(), 1, "backoff window must not touch sysfs");

        // Window elapsed, still failing → StillFailed (silent), backoff
        // doubles to 2 s.
        let t1 = t0 + chrono::Duration::seconds(1);
        assert!(matches!(
            latch.step(t1, || fail(&attempts)),
            LatchOutcome::StillFailed(_)
        ));
        assert_eq!(attempts.get(), 2);
        assert!(matches!(
            latch.step(t1, || fail(&attempts)),
            LatchOutcome::Skipped { backoff_secs: 2 }
        ));

        // Recovery: window elapsed and the write now succeeds →
        // Recovered edge, latch resets.
        let t2 = t1 + chrono::Duration::seconds(2);
        assert!(matches!(
            latch.step(t2, || Ok(Applied::NoChange)),
            LatchOutcome::Recovered(_)
        ));
        // Healthy again: a subsequent success is a plain Ok.
        assert!(matches!(
            latch.step(t2, || Ok(Applied::NoChange)),
            LatchOutcome::Ok(_)
        ));
    }

    /// The backoff is capped at [`LATCH_MAX_BACKOFF`] so a lever that
    /// fails for hours is still retried once a minute (never longer).
    #[test]
    fn lever_latch_backoff_caps_at_max() {
        let mut latch = LeverLatch::default();
        let mut now = chrono::Utc
            .with_ymd_and_hms(2026, 7, 12, 0, 0, 0)
            .single()
            .unwrap();
        // Drive many consecutive failures, always advancing past the
        // current backoff so each step re-attempts.
        for _ in 0..12 {
            let _ = latch.step(now, || Err::<Applied, _>(anyhow::anyhow!("boom")));
            now += chrono::Duration::seconds(LATCH_MAX_BACKOFF.as_secs() as i64 + 1);
        }
        assert_eq!(
            latch.backoff, LATCH_MAX_BACKOFF,
            "backoff must saturate at the cap"
        );
    }

    /// Idempotency contract: a second `write_if_changed` against the
    /// same value short-circuits to `NoChange` and leaves the file
    /// untouched (mtime stays put — checked via byte-equality).
    #[test]
    fn write_if_changed_skips_on_match() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("knob");
        fs::write(&p, "balanced\n").expect("seed knob");
        let out = write_if_changed(&p, "balanced").expect("diff");
        assert_eq!(out, Applied::NoChange);
        assert_eq!(fs::read_to_string(&p).expect("read"), "balanced\n");
    }

    /// SPEC §4 NFR Reliability: the crash-safe exit handler must
    /// write `balanced` to `firmware/acpi/platform_profile` and
    /// `balance_performance` to every `cpufreq/policy*/energy_performance_preference`
    /// leaf, synchronously, before the daemon process exits.
    #[test]
    fn crash_safe_exit_writes_vendor_defaults() {
        const POLICIES: usize = 3;
        let td = TempDir::new().expect("tempdir");
        let root = td.path();
        let acpi = root.join("firmware/acpi");
        fs::create_dir_all(&acpi).expect("mkdir acpi");
        fs::write(acpi.join("platform_profile"), "performance\n").expect("seed profile");
        let cpufreq = root.join("devices/system/cpu/cpufreq");
        fs::create_dir_all(&cpufreq).expect("mkdir cpufreq");
        for i in 0..POLICIES {
            let p = cpufreq.join(format!("policy{i}"));
            fs::create_dir_all(&p).expect("mkdir policy");
            fs::write(p.join("energy_performance_preference"), "performance\n").expect("seed epp");
        }
        crash_safe_exit_defaults(root);
        let profile = fs::read_to_string(acpi.join("platform_profile")).expect("read profile");
        assert_eq!(profile.trim(), "balanced");
        for i in 0..POLICIES {
            let leaf = cpufreq
                .join(format!("policy{i}"))
                .join("energy_performance_preference");
            let got = fs::read_to_string(&leaf).expect("read epp");
            assert_eq!(
                got.trim(),
                "balance_performance",
                "policy{i} must revert to vendor default",
            );
        }
    }

    /// On mismatch we write the new value and report the path.
    /// Trailing whitespace is irrelevant for the *match* test but the
    /// payload is written verbatim — sysfs nodes accept either shape.
    #[test]
    fn write_if_changed_writes_on_mismatch() {
        let td = TempDir::new().expect("tempdir");
        let p = td.path().join("knob");
        fs::write(&p, "balanced\n").expect("seed knob");
        let out = write_if_changed(&p, "performance").expect("diff");
        match out {
            Applied::Wrote { path, value } => {
                assert_eq!(path, p);
                assert_eq!(value, "performance");
            }
            other => panic!("expected Wrote, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&p).expect("read"), "performance");
    }
}
