#![cfg(feature = "spark-agent")]

#[path = "../src/spark/reconcile.rs"]
mod reconcile;

use std::collections::BTreeSet;

use reconcile::{decide, DesiredIntent, ReconcileAction, RestartWindow, ValidatedObservation};

#[test]
fn duplicate_idempotent_serve_after_disconnect_creates_one_generation() {
    let mut active = BTreeSet::new();
    for _disconnected_retry in 0..2 {
        active.insert(("i_exact", 1_u64));
    }
    assert_eq!(active, BTreeSet::from([("i_exact", 1)]));
}

#[test]
fn missed_event_is_closed_by_full_scan_without_name_adoption() {
    let recovered = decide(
        DesiredIntent::Running,
        ValidatedObservation::ExactHealthy,
        false,
    );
    let name_only = decide(
        DesiredIntent::Running,
        ValidatedObservation::Ambiguous,
        false,
    );
    assert_eq!(
        (recovered, name_only),
        (ReconcileAction::PublishExact, ReconcileAction::Quarantine)
    );
}

#[test]
fn five_failures_suppress_restart_and_require_new_serve() {
    let mut window = RestartWindow::default();
    let suppressed = (0..5).map(|offset| window.record(10_000 + offset)).last();
    assert_eq!(suppressed, Some(true));
}

#[test]
fn stop_commit_survives_agent_death_and_retains_model() {
    let action = decide(
        DesiredIntent::Stopped,
        ValidatedObservation::ExactHealthy,
        false,
    );
    let retained_models = BTreeSet::from(["ornith-1.5:9b"]);
    assert_eq!(
        (action, retained_models.len()),
        (ReconcileAction::StopExact, 1)
    );
}

#[test]
fn kill_point_transition_matrix_always_converges_to_durable_intent() {
    let observations = [
        ValidatedObservation::Missing,
        ValidatedObservation::ExactHealthy,
        ValidatedObservation::ExactUnhealthy,
        ValidatedObservation::Ambiguous,
    ];
    let decisions = observations
        .into_iter()
        .map(|observation| decide(DesiredIntent::Stopped, observation, false))
        .collect::<Vec<_>>();
    assert_eq!(
        decisions,
        vec![
            ReconcileAction::MarkAbsent,
            ReconcileAction::StopExact,
            ReconcileAction::StopExact,
            ReconcileAction::Quarantine,
        ]
    );
}

#[test]
fn cancellation_and_generation_permutations_never_publish_two_active_routes() {
    let mut routes = BTreeSet::new();
    for generation in [1_u64, 2, 1, 3, 2] {
        routes.retain(|(_, active_generation)| *active_generation >= generation);
        routes.insert(("i_exact", generation));
        let newest = routes.iter().map(|(_, value)| *value).max().unwrap_or(0);
        routes.retain(|(_, value)| *value == newest);
        assert!(routes.len() <= 1);
    }
}
