//! `sy doctor` — linear-checks health probe (SPEC §4.6 / §4.7,
//! ROADMAP `arch-observability` Step 5).
//!
//! Runs a fixed list of `Check`s in order, prints a stable JSON or
//! human summary, and returns a SPEC §4.7 exit code: 0 all-pass, 1
//! any-fail, 2 usage error, 3 warn-only.
//!
//! Public surface:
//! * [`Doctor`] — the runner; holds the registered checks.
//! * [`DoctorOpts`] — clap-facing options (`--json`, `--only`).
//! * [`DoctorReport`] — the serialised result, schema-stable.
//! * [`dispatch`] — `main.rs` entry point; runs and exits.
//!
//! Per-check status is `pass | warn | fail | skip` (kebab-case in
//! JSON). The check registry lives in [`checks`].

pub mod checks;

use std::io::{self, Write};

use anyhow::Result;
use serde::Serialize;

/// SPEC §4.6 schema version. Bumped only when the JSON shape changes
/// in a way that breaks existing consumers; never for added fields.
pub const SCHEMA_VERSION: u32 = 1;

/// Stable exit codes (SPEC §4.7). Kept here next to the runner so
/// consumers don't have to cross-reference `ipc_cli` constants.
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_DRIFT: i32 = 3;

/// Per-check status. Serialises as kebab-case (`"pass" | "warn" |
/// "fail" | "skip"`) so JSON consumers don't have to know Rust's
/// PascalCase convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

/// One check outcome. Field order matches SPEC §4.6: `name`,
/// `status`, then optional `message`/`fix`/`details`. `skip_serializing_if`
/// keeps absent fields out of the JSON for compactness without
/// changing the schema contract.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: &'static str,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Per-status count rollup. All four buckets are always present so
/// JSON consumers can index without `.get()`-then-`.unwrap_or(0)`.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct Summary {
    pub pass: u32,
    pub warn: u32,
    pub fail: u32,
    pub skip: u32,
}

/// Full doctor report. The single source of truth for the SPEC §4.6
/// JSON shape; `version` lets future consumers gate on the schema.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub version: u32,
    pub summary: Summary,
    pub checks: Vec<CheckResult>,
}

impl DoctorReport {
    /// Tally `checks` into a `Summary` and stamp the current schema
    /// version. Used by `Doctor::run` and the unit tests.
    pub fn from_checks(checks: Vec<CheckResult>) -> Self {
        let mut summary = Summary::default();
        for c in &checks {
            match c.status {
                Status::Pass => summary.pass += 1,
                Status::Warn => summary.warn += 1,
                Status::Fail => summary.fail += 1,
                Status::Skip => summary.skip += 1,
            }
        }
        Self {
            version: SCHEMA_VERSION,
            summary,
            checks,
        }
    }

    /// SPEC §4.7 exit-code mapping:
    /// * any `fail`              → `1`
    /// * no fail but ≥1 `warn`   → `3` (drift)
    /// * otherwise               → `0`
    ///
    /// `skip` does not influence the exit code (a skipped check is
    /// neither pass nor failure).
    pub fn exit_code(&self) -> i32 {
        if self.summary.fail > 0 {
            EXIT_FAIL
        } else if self.summary.warn > 0 {
            EXIT_DRIFT
        } else {
            EXIT_OK
        }
    }
}

/// One probe. Implementations are `Send + Sync` so the runner can
/// later parallelise without a type-system rewrite. For now checks
/// run serially in registration order (SPEC §4.6 "linear-checks").
pub trait Check: Send + Sync {
    /// Stable dot-separated identifier — matches the SPEC §4.6
    /// schema's `name` field and the `--only=<prefix>` filter.
    fn name(&self) -> &'static str;
    fn run(&self) -> CheckResult;
}

/// Options for one `sy doctor` invocation. Mirrors the clap-facing
/// flags so `main.rs` can pass through without an intermediate type.
#[derive(Debug, Clone, Default)]
pub struct DoctorOpts {
    /// Emit the SPEC §4.6 JSON shape on stdout (pretty-printed).
    pub json: bool,
    /// Run only checks whose `name()` starts with this prefix. An
    /// empty / `None` value runs everything.
    pub only: Option<String>,
}

/// The runner. Holds the static list of checks; mutated only via
/// `Doctor::with_checks` (tests) or `Doctor::default` (production).
pub struct Doctor {
    checks: Vec<Box<dyn Check>>,
}

impl Doctor {
    /// Production check list — the SPEC §4.6 "first batch" from the
    /// Step 5 roadmap entry.
    pub fn new() -> Self {
        Self {
            checks: checks::default_checks(),
        }
    }

    /// Test-only constructor: hand in a custom set of checks. The
    /// runner is sealed in production — operators get the SPEC §4.6
    /// "first batch" via `Doctor::new()`; only `#[cfg(test)]` code
    /// substitutes synthetic checks for deterministic exit-code tests.
    #[cfg(test)]
    fn with_checks(checks: Vec<Box<dyn Check>>) -> Self {
        Self { checks }
    }

    /// Run all registered checks honoring `opts.only`. Returns the
    /// report; the caller decides what to do with the exit code.
    pub fn run(&self, opts: &DoctorOpts) -> DoctorReport {
        let prefix = opts.only.as_deref().unwrap_or("");
        let results: Vec<CheckResult> = self
            .checks
            .iter()
            .filter(|c| prefix.is_empty() || c.name().starts_with(prefix))
            .map(|c| c.run())
            .collect();
        DoctorReport::from_checks(results)
    }
}

impl Default for Doctor {
    fn default() -> Self {
        Self::new()
    }
}

/// `main.rs` entry point. Resolves opts, runs the doctor, prints in
/// the chosen format, then `std::process::exit`s with the SPEC §4.7
/// code. The "no checks matched the prefix" branch returns
/// [`EXIT_USAGE`] (2) per the orchestrator's spec for Step 5.
pub fn dispatch(opts: DoctorOpts) -> Result<()> {
    let doctor = Doctor::new();
    let report = doctor.run(&opts);

    if let Some(prefix) = opts.only.as_deref() {
        if !prefix.is_empty() && report.checks.is_empty() {
            eprintln!("no checks matched prefix {prefix:?}");
            std::process::exit(EXIT_USAGE);
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if opts.json {
        let s = serde_json::to_string_pretty(&report)?;
        writeln!(out, "{s}")?;
    } else {
        write_human(&mut out, &report)?;
    }
    drop(out);
    std::process::exit(report.exit_code());
}

/// Human-readable rendering. Linear-list grouped by `name` prefix
/// (subsystem) per SPEC §4.6 "Default TTY view = colored linear list".
/// ANSI colour is intentionally not emitted yet — CLIG §`NO_COLOR`
/// requires opting in only when stdout is a TTY, and that gating is
/// orthogonal to the schema work this step lands.
fn write_human<W: Write>(w: &mut W, report: &DoctorReport) -> io::Result<()> {
    for c in &report.checks {
        let tag = match c.status {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        };
        match (&c.message, &c.fix) {
            (Some(m), Some(fix)) => writeln!(w, "{tag} {} — {m}\n     fix: {fix}", c.name)?,
            (Some(m), None) => writeln!(w, "{tag} {} — {m}", c.name)?,
            (None, Some(fix)) => writeln!(w, "{tag} {}\n     fix: {fix}", c.name)?,
            (None, None) => writeln!(w, "{tag} {}", c.name)?,
        }
    }
    let s = &report.summary;
    writeln!(
        w,
        "\nsummary: pass={} warn={} fail={} skip={}",
        s.pass, s.warn, s.fail, s.skip
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Synthetic check used by the runner-level unit tests below so
    /// they don't depend on real `/dev/accel` or socket presence.
    struct StaticCheck {
        name: &'static str,
        status: Status,
    }

    impl Check for StaticCheck {
        fn name(&self) -> &'static str {
            self.name
        }
        fn run(&self) -> CheckResult {
            CheckResult {
                name: self.name,
                status: self.status,
                message: None,
                fix: None,
                details: None,
            }
        }
    }

    fn synth(name: &'static str, status: Status) -> Box<dyn Check> {
        Box::new(StaticCheck { name, status })
    }

    #[test]
    fn report_serialises_per_spec_schema() {
        // SPEC §4.6: doctor output must carry `version`, `summary{pass,
        // warn, fail, skip}`, and a `checks` array of `{name, status,
        // message?, fix?, details?}`. Lock the field shape down so
        // future drift breaks this test before it breaks operators.
        let report = DoctorReport::from_checks(vec![
            CheckResult {
                name: "ipc.knowledge_sock",
                status: Status::Pass,
                message: Some("ready".into()),
                fix: None,
                details: None,
            },
            CheckResult {
                name: "kernel.landlock_version",
                status: Status::Warn,
                message: Some("missing".into()),
                fix: Some("upgrade kernel to 5.13+".into()),
                details: Some(serde_json::json!({"abi": null})),
            },
            CheckResult {
                name: "supervision.user_units_present",
                status: Status::Skip,
                message: None,
                fix: None,
                details: None,
            },
        ]);

        let v: Value = serde_json::to_value(&report).expect("serialise");
        assert_eq!(v["version"], Value::from(1));
        // Summary tallies exactly the inputs.
        assert_eq!(v["summary"]["pass"], Value::from(1));
        assert_eq!(v["summary"]["warn"], Value::from(1));
        assert_eq!(v["summary"]["fail"], Value::from(0));
        assert_eq!(v["summary"]["skip"], Value::from(1));
        // Check entries surface the kebab-case status and the named
        // fields we promised.
        let checks = v["checks"].as_array().expect("checks array");
        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0]["name"], "ipc.knowledge_sock");
        assert_eq!(checks[0]["status"], "pass");
        assert_eq!(checks[0]["message"], "ready");
        // `fix` and `details` are absent on a pass; the
        // `skip_serializing_if` contract keeps them out of the JSON.
        assert!(checks[0].get("fix").is_none());
        assert!(checks[0].get("details").is_none());
        assert_eq!(checks[1]["status"], "warn");
        assert_eq!(checks[1]["fix"], "upgrade kernel to 5.13+");
        assert_eq!(checks[1]["details"], serde_json::json!({"abi": null}));
        assert_eq!(checks[2]["status"], "skip");
    }

    #[test]
    fn any_fail_exits_one() {
        // SPEC §4.7: a single fail dominates the exit code regardless
        // of how many warns or passes flank it.
        let doctor = Doctor::with_checks(vec![
            synth("a.pass", Status::Pass),
            synth("b.warn", Status::Warn),
            synth("c.fail", Status::Fail),
        ]);
        let report = doctor.run(&DoctorOpts::default());
        assert_eq!(report.exit_code(), EXIT_FAIL);
    }

    #[test]
    fn warn_only_exits_three() {
        // SPEC §4.7: warn-only is the "drift" exit, distinct from
        // fail so CI/automation can treat advisories separately.
        let doctor = Doctor::with_checks(vec![
            synth("a.pass", Status::Pass),
            synth("b.warn", Status::Warn),
        ]);
        let report = doctor.run(&DoctorOpts::default());
        assert_eq!(report.exit_code(), EXIT_DRIFT);
    }

    #[test]
    fn all_pass_exits_zero() {
        // Companion to the warn/fail tests so the happy path is
        // explicitly locked in (and so future refactors of
        // `exit_code` can't accidentally invert the polarity).
        let doctor = Doctor::with_checks(vec![
            synth("a.pass", Status::Pass),
            synth("b.pass", Status::Pass),
        ]);
        let report = doctor.run(&DoctorOpts::default());
        assert_eq!(report.exit_code(), EXIT_OK);
    }

    #[test]
    fn only_prefix_filters_checks() {
        // `--only=ipc.` runs only checks under the ipc subsystem.
        let doctor = Doctor::with_checks(vec![
            synth("ipc.knowledge_sock", Status::Pass),
            synth("ipc.aiplane_sock", Status::Pass),
            synth("kernel.landlock_version", Status::Warn),
        ]);
        let opts = DoctorOpts {
            json: false,
            only: Some("ipc.".into()),
        };
        let report = doctor.run(&opts);
        assert_eq!(report.checks.len(), 2);
        assert_eq!(report.summary.warn, 0);
        for c in &report.checks {
            assert!(c.name.starts_with("ipc."));
        }
    }

    #[test]
    fn e2e_runs_and_emits_summary() {
        // Drives the real `Doctor::new()` registry end-to-end (no
        // mocked checks). The point isn't to assert pass-vs-fail on
        // this host — it's to confirm the runner walks the full list
        // without panicking and that the resulting JSON parses back
        // into the SPEC §4.6 shape.
        let doctor = Doctor::new();
        let report = doctor.run(&DoctorOpts::default());
        let total =
            report.summary.pass + report.summary.warn + report.summary.fail + report.summary.skip;
        assert_eq!(report.checks.len() as u32, total);
        let s = serde_json::to_string_pretty(&report).expect("serialise");
        let v: Value = serde_json::from_str(&s).expect("parse round-trip");
        assert_eq!(v["version"], Value::from(1));
        assert!(v["summary"].is_object());
        assert!(v["checks"].is_array());
    }

    #[test]
    fn human_renders_status_and_summary() {
        let mut buf = Vec::new();
        let report = DoctorReport::from_checks(vec![
            CheckResult {
                name: "a.pass",
                status: Status::Pass,
                message: Some("ok".into()),
                fix: None,
                details: None,
            },
            CheckResult {
                name: "b.fail",
                status: Status::Fail,
                message: Some("nope".into()),
                fix: Some("try x".into()),
                details: None,
            },
        ]);
        write_human(&mut buf, &report).expect("write");
        let s = String::from_utf8(buf).expect("utf8");
        assert!(s.contains("PASS a.pass"));
        assert!(s.contains("FAIL b.fail"));
        assert!(s.contains("fix: try x"));
        assert!(s.contains("summary: pass=1 warn=0 fail=1 skip=0"));
    }
}
