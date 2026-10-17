//! Doctor report contract used by the standalone monitor.
use std::io::{self, Write};

use serde::Serialize;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

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

pub trait Check: Send + Sync {
    fn name(&self) -> &'static str;
    fn run(&self) -> CheckResult;
}

#[derive(Default)]
pub struct DoctorOpts {
    pub only: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub pass: u32,
    pub warn: u32,
    pub fail: u32,
    pub skip: u32,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub version: u32,
    pub summary: Summary,
    pub checks: Vec<CheckResult>,
}

impl DoctorReport {
    pub fn exit_code(&self) -> i32 {
        if self.summary.fail > 0 {
            EXIT_FAIL
        } else if self.summary.warn > 0 {
            3
        } else {
            EXIT_OK
        }
    }
}

pub struct Doctor {
    checks: Vec<Box<dyn Check>>,
}

impl Doctor {
    pub fn with_checks_public(checks: Vec<Box<dyn Check>>) -> Self {
        Self { checks }
    }

    pub fn run(&self, opts: &DoctorOpts) -> DoctorReport {
        let prefix = opts.only.as_deref().unwrap_or("");
        let checks: Vec<_> = self
            .checks
            .iter()
            .filter(|c| c.name().starts_with(prefix))
            .map(|c| c.run())
            .collect();
        let mut summary = Summary {
            pass: 0,
            warn: 0,
            fail: 0,
            skip: 0,
        };
        for check in &checks {
            match check.status {
                Status::Pass => summary.pass += 1,
                Status::Warn => summary.warn += 1,
                Status::Fail => summary.fail += 1,
            }
        }
        DoctorReport {
            version: 1,
            summary,
            checks,
        }
    }
}

pub fn write_human_public<W: Write>(out: &mut W, report: &DoctorReport) -> io::Result<()> {
    for check in &report.checks {
        let tag = match check.status {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        };
        match (&check.message, &check.fix) {
            (Some(message), Some(fix)) => {
                writeln!(out, "{tag} {} — {message}\n     fix: {fix}", check.name)?
            }
            (Some(message), None) => writeln!(out, "{tag} {} — {message}", check.name)?,
            (None, Some(fix)) => writeln!(out, "{tag} {}\n     fix: {fix}", check.name)?,
            (None, None) => writeln!(out, "{tag} {}", check.name)?,
        }
    }
    let s = &report.summary;
    writeln!(
        out,
        "\nsummary: pass={} warn={} fail={} skip={}",
        s.pass, s.warn, s.fail, s.skip
    )
}
