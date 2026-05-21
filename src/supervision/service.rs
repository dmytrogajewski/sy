// arch-supervision Step 3 (`specs/roadmaps/arch-supervision/ROADMAP.md`):
// `sy service start|stop|restart|status|enable|disable|logs` — the SPEC
// §4.7 CLI surface. Wraps `systemctl --user` and `journalctl --user`;
// owns the canonical short-name → unit-name resolver and the verb
// dispatcher. State parsing + `--json` shape live in
// `crate::supervision::status`; log streaming in
// `crate::supervision::logs`.
//
// Exit codes (SPEC §4.7):
//   * 0 — success
//   * 1 — generic failure (unit not running when start was requested)
//   * 2 — usage error (unknown name, bad flags)
//   * 3 — drift (state mismatch detected by `status`)
//   * 4 — not ready (unit installed but `state != ready` when ready
//         was expected; future-use, declared now so callers can pin it)

use clap::Subcommand;

/// `sy service <verb>` — see SPEC §4.7. The dispatcher in
/// `dispatch()` forwards each variant to `systemctl --user` (or the
/// `logs`/`status` helpers under `crate::supervision`).
///
/// Examples:
///   sy service start aiplane                # systemctl --user start sy-aiplane.service
///   sy service status knowledge --json      # machine-readable §4.5 state
///   sy service logs aiplane -f -n 200       # journalctl --user -u … -f -n 200
///   sy service logs agentd --trace <uuid>   # filter by SY_TRACE_ID=<uuid>
///   sy service enable sy.target             # enable the whole group
#[derive(Debug, Subcommand)]
pub enum ServiceCmd {
    /// `systemctl --user start sy-<name>.service` (idempotent).
    Start { name: String },
    /// `systemctl --user stop sy-<name>.service`.
    Stop { name: String },
    /// `systemctl --user restart sy-<name>.service`.
    Restart { name: String },
    /// Map systemd state → SPEC §4.5 logical state; `--json` for agents.
    Status {
        name: String,
        /// Emit the stable JSON schema documented in
        /// `crate::supervision::status`.
        #[arg(long)]
        json: bool,
    },
    /// `systemctl --user enable sy-<name>.service` (idempotent).
    Enable { name: String },
    /// `systemctl --user disable sy-<name>.service`.
    Disable { name: String },
    /// Stream `journalctl --user -u sy-<name>.service`. Optional
    /// `-f`/`-n`/`--since`/`--trace` filters.
    Logs {
        name: String,
        /// `-f` — follow the log stream.
        #[arg(short, long)]
        follow: bool,
        /// `-n N` — show only the last N entries.
        #[arg(short = 'n', long, value_name = "N")]
        lines: Option<usize>,
        /// `--since <time>` — passed through verbatim to journalctl.
        #[arg(long, value_name = "TIME")]
        since: Option<String>,
        /// `--trace <id>` — filter to entries with `SY_TRACE_ID=<id>`.
        #[arg(long, value_name = "ID")]
        trace: Option<String>,
    },
}

/// Stable exit-coded error returned by every `crate::supervision` path
/// that talks to systemd. Mapped to `std::process::exit` in `main.rs`.
#[derive(Debug)]
pub struct ServiceError {
    pub code: i32,
    pub msg: String,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::error::Error for ServiceError {}

/// Stable exit codes per SPEC §4.7. `Ok(())` from a `dispatch` arm
/// implicitly returns 0 (`success`), so the success code lives only
/// in the SPEC; the four non-zero codes are constants here so the
/// status / wrapper paths can pin them.
pub mod exit {
    pub const GENERIC: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const DRIFT: i32 = 3;
    pub const NOT_READY: i32 = 4;
}

/// Canonical short names recognised by `sy service`. Every other unit
/// must be passed by its full `sy-<name>.service` (or `sy.target`)
/// name; anything else triggers a usage error.
const CANONICAL: &[&str] = &[
    "aiplane",
    "knowledge",
    "qdrant",
    "stack-bar",
    "agentd",
    "powerd",
];

/// Resolve a user-supplied short name to the full systemd unit name.
///
/// Accepts:
///   * one of the five canonical short names → `sy-<name>.service`
///   * `sy.target` (and `sy-<x>.target`) verbatim
///   * a full `sy-<x>.service` / `sy-<x>.socket` verbatim
///
/// Anything else is rejected with a `ServiceError` carrying exit code
/// `USAGE` so the CLI exits with 2 per SPEC §4.7.
pub fn name_to_unit(name: &str) -> Result<String, ServiceError> {
    if CANONICAL.contains(&name) {
        return Ok(format!("sy-{name}.service"));
    }
    let is_passthrough = name == "sy.target"
        || (name.starts_with("sy-")
            && (name.ends_with(".service")
                || name.ends_with(".socket")
                || name.ends_with(".target")));
    if is_passthrough {
        return Ok(name.to_string());
    }
    Err(ServiceError {
        code: exit::USAGE,
        msg: format!(
            "unknown service '{name}'; expected one of {:?} or a full sy-<name>.service",
            CANONICAL
        ),
    })
}

/// Forward a verb to `systemctl --user`. Used by start/stop/restart/
/// enable/disable. Maps a non-zero exit status to `ServiceError` with
/// `GENERIC` so callers see SPEC §4.7's exit code 1.
fn systemctl(verb: &str, unit: &str) -> Result<(), ServiceError> {
    let st = std::process::Command::new("systemctl")
        .args(["--user", verb, unit])
        .status()
        .map_err(|e| ServiceError {
            code: exit::GENERIC,
            msg: format!("spawn systemctl --user {verb} {unit}: {e}"),
        })?;
    if !st.success() {
        return Err(ServiceError {
            code: exit::GENERIC,
            msg: format!("systemctl --user {verb} {unit} exited with status {st}"),
        });
    }
    Ok(())
}

/// Dispatch a `ServiceCmd` to the matching `systemctl --user` /
/// `journalctl --user` invocation. Pulled out of `main.rs` so the file
/// stays under the LOC ceiling enforced by `check_main_rs_loc.sh`.
pub fn dispatch(cmd: ServiceCmd) -> anyhow::Result<()> {
    match cmd {
        ServiceCmd::Start { name } => {
            let unit = name_to_unit(&name)?;
            systemctl("start", &unit)?;
            Ok(())
        }
        ServiceCmd::Stop { name } => {
            let unit = name_to_unit(&name)?;
            systemctl("stop", &unit)?;
            Ok(())
        }
        ServiceCmd::Restart { name } => {
            let unit = name_to_unit(&name)?;
            systemctl("restart", &unit)?;
            Ok(())
        }
        ServiceCmd::Enable { name } => {
            let unit = name_to_unit(&name)?;
            systemctl("enable", &unit)?;
            Ok(())
        }
        ServiceCmd::Disable { name } => {
            let unit = name_to_unit(&name)?;
            systemctl("disable", &unit)?;
            Ok(())
        }
        ServiceCmd::Status { name, json } => {
            let unit = name_to_unit(&name)?;
            crate::supervision::status::run_cli(&name, &unit, json)
        }
        ServiceCmd::Logs {
            name,
            follow,
            lines,
            since,
            trace,
        } => {
            let unit = name_to_unit(&name)?;
            crate::supervision::logs::run_cli(
                &unit,
                crate::supervision::logs::LogsOpts {
                    follow,
                    lines,
                    since,
                    trace,
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_to_unit_resolves_canonical_names() {
        // The five canonical short names from SPEC §4.5 all map to the
        // matching `sy-<name>.service` unit.
        for (short, full) in &[
            ("aiplane", "sy-aiplane.service"),
            ("knowledge", "sy-knowledge.service"),
            ("qdrant", "sy-qdrant.service"),
            ("stack-bar", "sy-stack-bar.service"),
            ("agentd", "sy-agentd.service"),
            // Step 10: sy-powerd joins the canonical roster.
            ("powerd", "sy-powerd.service"),
        ] {
            assert_eq!(
                name_to_unit(short).unwrap(),
                *full,
                "short name {short:?} should resolve to {full}"
            );
        }
        // `sy.target` and full unit names pass through verbatim so
        // operators can target arbitrary `sy-*` units (e.g. the
        // knowledge socket).
        assert_eq!(name_to_unit("sy.target").unwrap(), "sy.target");
        assert_eq!(
            name_to_unit("sy-knowledge.service").unwrap(),
            "sy-knowledge.service"
        );
        assert_eq!(
            name_to_unit("sy-knowledge.socket").unwrap(),
            "sy-knowledge.socket"
        );
    }

    #[test]
    fn unknown_name_exits_usage_error() {
        let err = name_to_unit("foobar").expect_err("unknown name must be rejected");
        assert_eq!(
            err.code,
            exit::USAGE,
            "unknown names must exit with SPEC §4.7 usage code 2"
        );
        assert!(
            err.msg.contains("foobar"),
            "error message should name the offending input, got {:?}",
            err.msg
        );
    }

    #[test]
    #[ignore = "requires a live `systemctl --user` (user manager); run manually"]
    fn e2e_status_for_unit_not_started() {
        // Real-host probe: a unit that has never been loaded reports
        // `NotInstalled`. Gated `#[ignore]` so `make test` stays
        // hermetic on CI / containers without a user manager.
        let rec = crate::supervision::status::status_record(
            "nonexistent-unit-3f1c",
            "nonexistent-unit-3f1c.service",
        )
        .expect("status_record() must Ok");
        assert_eq!(
            rec.state,
            crate::supervision::status::ServiceStatus::NotInstalled
        );
    }
}
