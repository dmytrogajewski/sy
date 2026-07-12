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

/// Stable exit code: the daemon answered but its response frame could
/// not be parsed (protocol / version mismatch, or a malformed field).
/// Distinct from [`EXIT_DAEMON_UNREACHABLE`] so an agent can tell a
/// down socket from a reachable-but-unparseable daemon
/// (BUG-20260712-1137).
pub const EXIT_DECODE_ERROR: i32 = 5;

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

/// Relative in-tree config path, shared by the cwd-dev branch and the
/// no-`$HOME` last resort so the literal lives in exactly one place.
const IN_TREE_POWER_CONFIG: &str = "configs/sy/power.toml";

/// Resolve the active `power.toml`. Precedence follows CLAUDE.md
/// (flags > env > config file > defaults):
///
/// 1. `$SY_ROOT/configs/sy/power.toml` — explicit repo override (dev /
///    `sy --root`).
/// 2. `./configs/sy/power.toml` *if it exists* — running from the repo
///    checkout (tests, `cargo run` from the tree).
/// 3. `$XDG_CONFIG_HOME/sy/power.toml` (else `$HOME/.config/sy/power.toml`)
///    — the location `sy power apply` installs to. This is what the
///    systemd `--user` service reads: its cwd is `$HOME`, so the
///    cwd-relative branch (2) misses and it MUST fall through to the
///    installed config rather than silently defaulting to an empty arm
///    table (BUG-20260608-2341).
pub(crate) fn power_config_path() -> std::path::PathBuf {
    let cwd_exists = std::path::Path::new(IN_TREE_POWER_CONFIG).exists();
    resolve_power_config_path(
        std::env::var("SY_ROOT").ok().as_deref(),
        cwd_exists,
        power_config_xdg_path(),
    )
}

/// Pure precedence logic behind [`power_config_path`], split out so the
/// branch order is unit-testable without touching process env or cwd.
fn resolve_power_config_path(
    sy_root: Option<&str>,
    cwd_exists: bool,
    xdg: std::path::PathBuf,
) -> std::path::PathBuf {
    if let Some(root) = sy_root {
        return std::path::PathBuf::from(root).join(IN_TREE_POWER_CONFIG);
    }
    if cwd_exists {
        return std::path::PathBuf::from(IN_TREE_POWER_CONFIG);
    }
    xdg
}

/// Installed config location: `$XDG_CONFIG_HOME/sy/power.toml`, else
/// `$HOME/.config/sy/power.toml`, else the in-tree relative path (only
/// reached when neither env var is set, e.g. a stripped-down container).
pub(crate) fn power_config_xdg_path() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return std::path::PathBuf::from(xdg).join("sy/power.toml");
    }
    if let Ok(home) = std::env::var("HOME") {
        return std::path::PathBuf::from(home).join(".config/sy/power.toml");
    }
    std::path::PathBuf::from(IN_TREE_POWER_CONFIG)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `$SY_ROOT` always wins, even when the cwd config exists — the
    /// explicit dev override must beat the ambient checkout.
    #[test]
    fn resolve_power_config_path_prefers_sy_root() {
        let got = resolve_power_config_path(Some("/repo"), true, PathBuf::from("/xdg/sy/power.toml"));
        assert_eq!(got, PathBuf::from("/repo/configs/sy/power.toml"));
    }

    /// No `$SY_ROOT` but the cwd holds the in-tree config ⇒ use it
    /// (running from the repo checkout / `cargo test`).
    #[test]
    fn resolve_power_config_path_uses_cwd_when_present() {
        let got = resolve_power_config_path(None, true, PathBuf::from("/xdg/sy/power.toml"));
        assert_eq!(got, PathBuf::from("configs/sy/power.toml"));
    }

    /// Regression for BUG-20260608-2341: no `$SY_ROOT`, cwd has no
    /// `configs/` (the systemd `--user` service, cwd `$HOME`) ⇒ fall
    /// through to the installed XDG config, NOT a nonexistent
    /// cwd-relative path that silently defaults to empty arms.
    #[test]
    fn resolve_power_config_path_falls_back_to_xdg() {
        let xdg = PathBuf::from("/home/u/.config/sy/power.toml");
        let got = resolve_power_config_path(None, false, xdg.clone());
        assert_eq!(got, xdg);
    }
}
