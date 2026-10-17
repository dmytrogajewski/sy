//! Adapter from the independent monitor checks to the desktop doctor's report type.
//! The monitor crate owns probing; the desktop runner only registers the results.

use crate::doctor::{Check, CheckResult, Status};
use sy_monitor::doctor_support as monitor;

struct MonitorCheck(Box<dyn monitor::Check>);

impl Check for MonitorCheck {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn run(&self) -> CheckResult {
        let result = self.0.run();
        let status = match result.status {
            monitor::Status::Pass => Status::Pass,
            monitor::Status::Warn => Status::Warn,
            monitor::Status::Fail => Status::Fail,
        };
        CheckResult {
            name: result.name,
            status,
            message: result.message,
            fix: result.fix,
            details: result.details,
        }
    }
}

pub fn mon_doctor_checks() -> Vec<Box<dyn Check>> {
    sy_monitor::mon::doctor::mon_doctor_checks()
        .into_iter()
        .map(|check| Box::new(MonitorCheck(check)) as Box<dyn Check>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_doctor_registers_the_monitor_crates_checks() {
        let checks = mon_doctor_checks();
        assert_eq!(checks[0].name(), "mon.collect.running");
        assert_eq!(checks.last().unwrap().name(), "mon.history.writable");
    }
}
