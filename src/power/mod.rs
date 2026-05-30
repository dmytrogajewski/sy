//! `sy power` — ML-driven, intent-aware power orchestrator for Ryzen
//! AI HX 370. Scaffold landed by `sy-power` roadmap Step 1.
//!
//! See `specs/research/sy-power/SPEC.md` for the architecture and
//! `specs/roadmaps/sy-power/ROADMAP.md` for the step-by-step rollout.
//!
//! Step 1 ships only the CLI surface (`cli`) and config loader
//! (`config`). Sensors, daemon, bandit, shield, and the rest of the
//! tree arrive in Steps 2–38.

use std::fmt;

/// CLI-level error carrying a stable exit code per SPEC §4 ("Exit
/// codes: 0 ok / 1 err / 2 usage / 3 drift / 4 daemon unreachable /
/// 5 polkit denied / 6 unsupported hardware / 7 onboarding-not-complete").
/// `main.rs` downcasts the anyhow error and maps it to
/// `process::exit(code)`. Mirrors `agt::AgtError` / `stack::StackError`.
#[derive(Debug)]
pub struct PowerError {
    pub code: i32,
    pub msg: String,
}

impl fmt::Display for PowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for PowerError {}

/// SPEC §4 stable exit code: ADWIN drift alarm is active. Step 31
/// wires this for `sy power status` — when the daemon reports
/// `drift.adwin_alarm = true`, the CLI exits with this code so an
/// operator (or agent) can branch on "the model is degraded" without
/// parsing the JSON body. Distinct from `EXIT_DAEMON_UNREACHABLE`
/// (4); both are CLIG-friendly stable codes.
pub const EXIT_DRIFT_ACTIVE: i32 = 3;

/// SPEC §4 stable exit code: daemon socket present-but-refused or
/// missing. Step 11 wires this for `sy power status`; later commands
/// reuse it whenever an IPC dial fails.
pub const EXIT_DAEMON_UNREACHABLE: i32 = 4;

pub mod activity;
pub mod apply;
pub mod bandit;
pub mod checkpoint;
pub mod cli;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod drift;
pub mod forecast;
pub mod intent;
pub mod ipc;
pub mod labels;
pub mod log;
pub mod mcp;
pub mod onboarding;
pub mod policy;
pub mod ppd_shim;
pub mod report;
pub mod sensors;
pub mod shield;
pub mod snapshot;
pub mod status;
pub mod trainer;

pub use cli::{dispatch, PowerCmd};

/// Resolve `~/.local/state/sy/power/` for the daemon's NDJSON audit
/// log. Mirrors `cli::power_state_dir` — kept module-level so both
/// the read path (`sy power log`) and the write path (`sy-powerd`)
/// agree on the directory without a re-export dance. `XDG_STATE_HOME`
/// takes precedence over `$HOME`; both unset falls back to `/tmp` so
/// dev runs still produce a writable directory.
pub(crate) fn power_state_dir_for_daemon() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return std::path::PathBuf::from(xdg).join("sy/power");
    }
    if let Ok(home) = std::env::var("HOME") {
        return std::path::PathBuf::from(home).join(".local/state/sy/power");
    }
    std::path::PathBuf::from("/tmp/sy/power")
}
