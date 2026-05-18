// arch-supervision Step 3 (`specs/roadmaps/arch-supervision/ROADMAP.md`):
// `sy service logs <name>` — thin wrapper around
// `journalctl --user -u sy-<name>.service` with the four flags called
// out in SPEC §4.7:
//
//   -f / --follow         pass-through to `journalctl -f`
//   -n N                  pass-through to `journalctl -n N`
//   --since <TIME>        pass-through to `journalctl --since <TIME>`
//   --trace <ID>          appends `SY_TRACE_ID=<ID>` to the journalctl
//                         positional args — that's the supported way to
//                         filter journal entries by a structured field
//                         (`journalctl --user -u <unit> SY_TRACE_ID=<id>`).
//                         The trace_id field is stamped by zone 6 once
//                         it lands; before then the filter just yields
//                         zero rows (graceful empty result).
//
// We replace the current process with `journalctl` via `exec` when
// `--follow` is set so Ctrl-C terminates cleanly and the journal
// stream is fully untouched by Rust's stdout buffering.

use std::os::unix::process::CommandExt;

use crate::supervision::service::{exit, ServiceError};

/// Options accepted by `logs()`. Constructed from the `ServiceCmd::Logs`
/// variant in `service.rs::dispatch`.
#[derive(Debug, Default, Clone)]
pub struct LogsOpts {
    pub follow: bool,
    pub lines: Option<usize>,
    pub since: Option<String>,
    pub trace: Option<String>,
}

/// Build the argv vector for `journalctl --user -u <unit> …`. Pulled
/// out so unit tests can pin the flag ordering and the `SY_TRACE_ID=`
/// match form (SPEC §4.6 — trace_id lives on journal entries as a
/// structured field, not in the message text).
pub fn build_argv(unit: &str, opts: &LogsOpts) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "--user".into(),
        "-u".into(),
        unit.to_string(),
        // `-o cat` is the canonical "minimal stamp" output for sy logs;
        // operators who want JSON can pipe through `journalctl … -o
        // json` themselves — we keep our wrapper opinionated.
        "--no-pager".into(),
    ];
    if opts.follow {
        argv.push("-f".into());
    }
    if let Some(n) = opts.lines {
        argv.push("-n".into());
        argv.push(n.to_string());
    }
    if let Some(since) = &opts.since {
        argv.push("--since".into());
        argv.push(since.clone());
    }
    if let Some(id) = &opts.trace {
        argv.push(format!("SY_TRACE_ID={id}"));
    }
    argv
}

/// CLI entry: build the argv and either exec (`--follow`) or run-to-
/// completion. Both paths inherit stdio so the journal stream lands
/// on the operator's terminal.
pub fn run_cli(unit: &str, opts: LogsOpts) -> anyhow::Result<()> {
    let argv = build_argv(unit, &opts);
    let mut cmd = std::process::Command::new("journalctl");
    cmd.args(&argv);
    if opts.follow {
        // `exec` never returns on success; on failure we fall through
        // to a generic error so the CLI exits with SPEC §4.7 code 1.
        let err = cmd.exec();
        return Err(ServiceError {
            code: exit::GENERIC,
            msg: format!("exec journalctl --user -u {unit}: {err}"),
        }
        .into());
    }
    let st = cmd.status().map_err(|e| ServiceError {
        code: exit::GENERIC,
        msg: format!("spawn journalctl --user -u {unit}: {e}"),
    })?;
    if !st.success() {
        return Err(ServiceError {
            code: exit::GENERIC,
            msg: format!("journalctl --user -u {unit} exited with status {st}"),
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_defaults_are_minimal() {
        let argv = build_argv("sy-aiplane.service", &LogsOpts::default());
        assert_eq!(
            argv,
            vec![
                "--user".to_string(),
                "-u".into(),
                "sy-aiplane.service".into(),
                "--no-pager".into(),
            ]
        );
    }

    #[test]
    fn argv_with_follow_lines_since_and_trace() {
        let opts = LogsOpts {
            follow: true,
            lines: Some(200),
            since: Some("1h ago".into()),
            trace: Some("4f1d2c5b".into()),
        };
        let argv = build_argv("sy-knowledge.service", &opts);
        assert!(argv.contains(&"-f".to_string()));
        assert!(argv.windows(2).any(|w| w == ["-n", "200"]));
        assert!(argv.windows(2).any(|w| w == ["--since", "1h ago"]));
        assert!(
            argv.contains(&"SY_TRACE_ID=4f1d2c5b".to_string()),
            "trace filter must be passed as a structured match, got argv={argv:?}"
        );
    }
}
