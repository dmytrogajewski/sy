//! Deterministic reconciliation policy over validated durable intent and observations.

#[cfg(test)]
use std::collections::VecDeque;

pub const RESTART_FAILURE_LIMIT: usize = 5;
pub const RESTART_WINDOW_SECONDS: u64 = 10 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredIntent {
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidatedObservation {
    Missing,
    ExactHealthy,
    ExactUnhealthy,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    PublishExact,
    RestartExact,
    DisableAndRecordFailure,
    StopExact,
    MarkAbsent,
    KeepSuppressed,
    Quarantine,
}

pub fn decide(
    desired: DesiredIntent,
    observation: ValidatedObservation,
    restart_suppressed: bool,
) -> ReconcileAction {
    if observation == ValidatedObservation::Ambiguous {
        return ReconcileAction::Quarantine;
    }
    match (desired, observation, restart_suppressed) {
        (DesiredIntent::Stopped, ValidatedObservation::Missing, _) => ReconcileAction::MarkAbsent,
        (DesiredIntent::Stopped, _, _) => ReconcileAction::StopExact,
        (DesiredIntent::Running, ValidatedObservation::Missing, true) => {
            ReconcileAction::KeepSuppressed
        }
        (DesiredIntent::Running, ValidatedObservation::Missing, false) => {
            ReconcileAction::RestartExact
        }
        (DesiredIntent::Running, ValidatedObservation::ExactHealthy, false) => {
            ReconcileAction::PublishExact
        }
        (DesiredIntent::Running, ValidatedObservation::ExactHealthy, true) => {
            ReconcileAction::StopExact
        }
        (DesiredIntent::Running, ValidatedObservation::ExactUnhealthy, true) => {
            ReconcileAction::StopExact
        }
        (DesiredIntent::Running, ValidatedObservation::ExactUnhealthy, false) => {
            ReconcileAction::DisableAndRecordFailure
        }
        (_, ValidatedObservation::Ambiguous, _) => ReconcileAction::Quarantine,
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct RestartWindow {
    failures: VecDeque<u64>,
}

#[cfg(test)]
impl RestartWindow {
    pub fn record(&mut self, failed_at_unix_seconds: u64) -> bool {
        let oldest = failed_at_unix_seconds.saturating_sub(RESTART_WINDOW_SECONDS);
        while self
            .failures
            .front()
            .is_some_and(|failure| *failure < oldest)
        {
            self.failures.pop_front();
        }
        if self.failures.back() != Some(&failed_at_unix_seconds) {
            self.failures.push_back(failed_at_unix_seconds);
        }
        self.failures.len() >= RESTART_FAILURE_LIMIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_table_covers_every_desired_observed_and_suppression_input() {
        let desired = [DesiredIntent::Running, DesiredIntent::Stopped];
        let observed = [
            ValidatedObservation::Missing,
            ValidatedObservation::ExactHealthy,
            ValidatedObservation::ExactUnhealthy,
            ValidatedObservation::Ambiguous,
        ];
        let actions = desired
            .into_iter()
            .flat_map(|desired| {
                observed.into_iter().flat_map(move |observed| {
                    [false, true]
                        .into_iter()
                        .map(move |suppressed| decide(desired, observed, suppressed))
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(actions.len(), 16);
    }

    #[test]
    fn fifth_distinct_failure_inside_window_suppresses_restart() {
        let mut window = RestartWindow::default();
        assert!(!(0..4).any(|second| window.record(1_000 + second)));
        assert!(window.record(1_004));
    }
}
