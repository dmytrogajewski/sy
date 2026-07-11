//! `sy power` report — pure-function metrics + (future) plot/PDF
//! rendering surfaces (Roadmap Phase RV).
//!
//! Step 33 — this module — only ships the numerical layer:
//! [`metrics`] for the six metric structs + their per-`AuditEntry`
//! extractors, and [`baseline`] for the counterfactual-replay denominator
//! every "vs rules baseline" number reads off.
//!
//! Step 34 adds the SVG plotter ([`plots`]); later steps wire the
//! Typst report driver ([`render`], Step 35) and a `sy power show`
//! subcommand (Step 36). The plot renderer is pure-function too —
//! `&ReportMetrics → String /* SVG */` — so the same numerical floor
//! Step 33 laid still carries the rest of Phase RV.
//!
//! ## Why pure functions
//!
//! The audit log is the canonical record; every metric is a function
//! of `&[AuditEntry]`. No clock reads, no I/O, no global state. The
//! same input bytes therefore produce the same output bytes — which
//! is what makes the determinism test
//! (`baseline::tests::counterfactual_replay_deterministic`)
//! achievable and what lets `sy power status` re-use the bandit /
//! shield extractors for its "last-1 h" tooltip without flake risk.

pub mod baseline;
pub mod metrics;
pub mod plots;
pub mod render;
pub mod template;

pub use baseline::compute_counterfactual_baseline;
pub use metrics::{
    extract_activity_metrics, extract_bandit_metrics, extract_drift_metrics,
    extract_energy_metrics, extract_forecast_metrics, extract_shield_metrics,
};
pub use plots::{Plot, ReportMetrics};
pub use render::compile_pdf;
pub use template::{ReportHeader, ReportTemplate};
