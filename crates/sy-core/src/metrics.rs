//! Core metric catalogue (SPEC §4.6 / arch-observability Step 7).
//!
//! Every counter / gauge / histogram emitted by `sy` is pre-declared
//! here so the metric name + kind survives even when no recorder is
//! installed (which is the production default until the Zone 6.2 UDS
//! prometheus exporter lands). [`register_core_metrics`] calls
//! `metrics::describe_*!` for every entry; consumers like the
//! scheduler and supervisor emit by calling `counter!` / `gauge!` /
//! `histogram!` with the name verbatim.
//!
//! The describe step is also how `system.describe.capabilities.metrics`
//! discovers what `sy` *can* report before anything has emitted —
//! useful for `sy doctor` and the future MCP capability surface.
//!
//! ## SPEC §4.6 catalogue
//!
//! - Counter `sy_workload_completed_total{kind}` — scheduler emits
//!   one per successful workload run.
//! - Counter `sy_workload_errors_total{kind, reason}` — scheduler
//!   emits one per failed/cancelled run, tagged with the failure
//!   reason class.
//! - Counter `sy_policy_denials_total{tool}` — Zone 4 sandbox audit
//!   layer emits one per Landlock / seccomp denial. **Producer not
//!   yet wired** (arch-agent-sandbox is unstarted as of Step 7); the
//!   name is described here so the catalogue is complete and the
//!   future audit module can attach without a Cargo.toml change.
//! - Counter `sy_ipc_errors_total{endpoint, kind}` — sy-ipc server
//!   emits one per `Response::Err`.
//! - Gauge `sy_models_warm{kind}` — supervisor emits the current
//!   warm-pool occupancy on every `ensure` / shutdown.
//! - Gauge `sy_queue_depth{class, kind}` — scheduler emits the
//!   per-class depth on every dispatch tick.
//! - Gauge `sy_npu_temp_celsius` — sysfs poller (Zone 6 follow-up)
//!   emits the latest reading. **Producer not yet wired**; the name
//!   is described so doctor can flag it as "no producer yet".
//! - Histogram `sy_workload_latency_seconds{kind}` — scheduler emits
//!   the dispatch-to-completion delta on every run.

use metrics::{describe_counter, describe_gauge, describe_histogram};

/// Three-way classification matching `metrics::Recorder`'s register
/// surface. Used by the in-process catalogue so tests can assert
/// counter-vs-gauge-vs-histogram intent without depending on
/// `metrics_util::MetricKind` from production code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

/// Single catalogue entry: the metric name (label dimensions live on
/// the call site, not the catalogue) and its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CoreMetric {
    pub name: &'static str,
    pub kind: MetricKind,
}

/// SPEC §4.6 "Metrics" block, verbatim. Order is the SPEC's order to
/// make a diff against the spec trivial.
pub const CORE_METRICS: &[CoreMetric] = &[
    CoreMetric {
        name: "sy_workload_completed_total",
        kind: MetricKind::Counter,
    },
    CoreMetric {
        name: "sy_workload_errors_total",
        kind: MetricKind::Counter,
    },
    CoreMetric {
        name: "sy_policy_denials_total",
        kind: MetricKind::Counter,
    },
    CoreMetric {
        name: "sy_ipc_errors_total",
        kind: MetricKind::Counter,
    },
    CoreMetric {
        name: "sy_models_warm",
        kind: MetricKind::Gauge,
    },
    CoreMetric {
        name: "sy_queue_depth",
        kind: MetricKind::Gauge,
    },
    CoreMetric {
        name: "sy_npu_temp_celsius",
        kind: MetricKind::Gauge,
    },
    CoreMetric {
        name: "sy_workload_latency_seconds",
        kind: MetricKind::Histogram,
    },
];

/// Pre-declare every catalogue entry with the installed `metrics`
/// recorder. Safe to call at any point (`metrics`'s describe surface
/// is a no-op until a recorder is installed; once installed,
/// repeated calls are idempotent — the last description wins). Each
/// binary's `obs::init` should call this exactly once so the
/// capability surface stays consistent across processes.
pub fn register_core_metrics() {
    for entry in CORE_METRICS {
        match entry.kind {
            MetricKind::Counter => {
                describe_counter!(entry.name, describe_text(entry.name));
            }
            MetricKind::Gauge => {
                describe_gauge!(entry.name, describe_text(entry.name));
            }
            MetricKind::Histogram => {
                describe_histogram!(entry.name, describe_text(entry.name));
            }
        }
    }
}

/// Per-metric description string. Lives next to the catalogue so the
/// SPEC §4.6 prose for each name has one source of truth.
fn describe_text(name: &str) -> &'static str {
    match name {
        "sy_workload_completed_total" => "successful workload runs, by workload kind",
        "sy_workload_errors_total" => "failed workload runs, by workload kind and reason class",
        "sy_policy_denials_total" => "sandbox policy denials, by tool",
        "sy_ipc_errors_total" => "IPC error responses, by endpoint and error kind",
        "sy_models_warm" => "current warm-pool occupancy, by workload kind",
        "sy_queue_depth" => "scheduler queue depth, by priority class and workload kind",
        "sy_npu_temp_celsius" => "NPU package temperature (latest sysfs reading)",
        "sy_workload_latency_seconds" => {
            "workload dispatch-to-completion latency, by workload kind"
        }
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// SPEC §4.6 lists the metric names verbatim. The catalogue must
    /// match the spec set-equally — adding a name without updating the
    /// spec (or vice versa) is a contract drift that this test
    /// catches before the recorder backend lands and the names go on
    /// the wire to operators.
    #[test]
    fn core_metric_names_match_spec() {
        let spec_names: HashSet<&'static str> = [
            "sy_workload_completed_total",
            "sy_workload_errors_total",
            "sy_workload_latency_seconds",
            "sy_queue_depth",
            "sy_models_warm",
            "sy_policy_denials_total",
            "sy_ipc_errors_total",
            "sy_npu_temp_celsius",
        ]
        .into_iter()
        .collect();
        let catalogue_names: HashSet<&'static str> = CORE_METRICS.iter().map(|m| m.name).collect();
        assert_eq!(
            catalogue_names, spec_names,
            "CORE_METRICS catalogue drifted from SPEC §4.6"
        );
    }

    /// `register_core_metrics` must be a no-op when no recorder is
    /// installed (the production default) — exercising the call here
    /// makes sure the describe macros don't panic on a noop recorder.
    #[test]
    fn register_core_metrics_is_safe_without_recorder() {
        register_core_metrics();
    }
}
