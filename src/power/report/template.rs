//! Report-text template builder for the `sy power show` PDF (Step 35).
//!
//! The roadmap (Step 35) originally pinned `typst = "0.13"` + `typst-pdf =
//! "0.13"` and described this module as a "Typst-source generator". The
//! library-side typst API requires a full `World` implementation (font
//! book, source resolver, package manager) plus a ~6 MB bundled font set
//! the binary-size budget does not allow; we therefore emit the PDF
//! directly via `pdf-writer` (the same crate the typst project itself
//! uses to render its final bytes — see the `Cargo.toml` rationale).
//!
//! What survives the deviation is the **content shape** of the report:
//! header, executive summary, six metric panels, methodology footer. This
//! module owns the pure-function content layer; [`super::render`] owns the
//! PDF byte layer. Both are I/O-free, clock-free, and deterministic over
//! the same [`super::plots::ReportMetrics<'_>`] bundle Step 34 introduced.
//!
//! ## Surface
//!
//! - [`ReportTemplate`] — the eight-panel content struct the renderer
//!   walks line-by-line into the PDF.
//! - [`exec_summary`] — pure-fn that derives the three executive-summary
//!   bullets from a `ReportMetrics<'_>`. The wording is fixed per metric
//!   (no LLM, no randomness) so the same NDJSON window → same bullets.
//! - [`ReportTemplate::build`] — assembles a `ReportTemplate` from a
//!   `ReportMetrics<'_>` plus a header struct (host + window + version).

use super::plots::ReportMetrics;

/// Header line: the report's first text line under the title. Mirrors
/// the SPEC §RV.2 "host, kernel, dataset window, model.version_sha,
/// generation timestamp" requirement.
#[derive(Debug, Clone)]
pub struct ReportHeader {
    pub host: String,
    pub generated_at_rfc3339: String,
    pub window_days: f32,
    pub model_version_sha: String,
}

/// Bundle of every text line the PDF needs. The renderer
/// ([`super::render::compile_pdf`]) iterates each panel's bullets in
/// order and emits them one per line on the corresponding page. Plot
/// SVGs are produced by Step 34's [`super::plots::Plot::render`] and
/// embedded separately (the PDF renderer reuses the plot _names_ as
/// caption text since pdf-writer doesn't ship an SVG embedder).
#[derive(Debug, Clone)]
pub struct ReportTemplate {
    pub header: ReportHeader,
    pub exec_bullets: Vec<String>,
    pub bandit_lines: Vec<String>,
    pub forecast_lines: Vec<String>,
    pub shield_lines: Vec<String>,
    pub energy_lines: Vec<String>,
    pub drift_lines: Vec<String>,
    pub methodology_lines: Vec<String>,
}

impl ReportTemplate {
    /// Build a template from the live metrics + header. Pure-fn:
    /// deterministic over the same inputs so the report can round-trip
    /// byte-identical when the audit window doesn't change.
    pub fn build(metrics: &ReportMetrics<'_>, header: ReportHeader) -> Self {
        Self {
            header,
            exec_bullets: exec_summary(metrics),
            bandit_lines: bandit_panel(metrics),
            forecast_lines: forecast_panel(metrics),
            shield_lines: shield_panel(metrics),
            energy_lines: energy_panel(metrics),
            drift_lines: drift_panel(metrics),
            methodology_lines: methodology_footer(metrics),
        }
    }
}

/// Three executive-summary bullets. The wording is fixed per metric so
/// the golden-snapshot test pins exact strings and an LLM-free regress
/// detector trips immediately on a phrasing drift.
///
/// Ordering: bandit savings, drift status, shield meeting dwell. Mirrors
/// the SPEC §RV.2 example ("Bandit saved 4.2 % perf/W…", "No drift
/// alarms", "Shield held MEETING for 12 %") so an operator reading the
/// PDF sees the same three slots every time.
pub fn exec_summary(metrics: &ReportMetrics<'_>) -> Vec<String> {
    let mut bullets = Vec::with_capacity(3);
    // 1. Bandit savings vs rules baseline (negative regret = saved).
    let regret = metrics.bandit.cumulative_regret_vs_baseline;
    if metrics.bandit.total_decisions == 0 {
        bullets.push("Bandit had no decisions in this window".to_string());
    } else if regret < 0.0 {
        bullets.push(format!(
            "Bandit saved {:.1} W\u{00b7}s vs rules baseline over this window",
            regret.abs(),
        ));
    } else if regret > 0.0 {
        bullets.push(format!(
            "Bandit cost {:.1} W\u{00b7}s vs rules baseline over this window",
            regret,
        ));
    } else {
        bullets.push("Bandit matched rules baseline over this window".to_string());
    }
    // 2. Drift status — explicit "no alarms" rather than a zero number.
    if metrics.drift.adwin_alarms == 0 {
        bullets.push("No drift alarms".to_string());
    } else {
        let plural = if metrics.drift.adwin_alarms == 1 {
            ""
        } else {
            "s"
        };
        bullets.push(format!(
            "{} drift alarm{plural} triggered {} retrain{}",
            metrics.drift.adwin_alarms,
            metrics.drift.retrains_triggered,
            if metrics.drift.retrains_triggered == 1 {
                ""
            } else {
                "s"
            },
        ));
    }
    // 3. Shield meeting-dwell — surfaces "do not disturb" coverage.
    let meeting = metrics
        .shield
        .state_dwell_pct
        .get("MEETING")
        .copied()
        .unwrap_or(0.0);
    bullets.push(format!(
        "Shield held MEETING for {:.0}% of the window",
        meeting * 100.0,
    ));
    bullets
}

/// Bandit-panel summary lines: one-per-row table of the report's
/// reward / regret / arm-distribution numbers.
fn bandit_panel(metrics: &ReportMetrics<'_>) -> Vec<String> {
    let b = metrics.bandit;
    let mut lines = vec![
        format!("Decisions: {}", b.total_decisions),
        format!(
            "Reward mean / p50 / p95: {:.3} / {:.3} / {:.3}",
            b.reward_mean, b.reward_p50, b.reward_p95,
        ),
        format!(
            "Cumulative regret vs baseline: {:.2} W\u{00b7}s",
            b.cumulative_regret_vs_baseline,
        ),
        format!("Alpha violations: {}", b.alpha_violations_count),
    ];
    let mut arms: Vec<(&str, f32)> = b
        .arm_distribution
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    arms.sort_by(|a, b| a.0.cmp(b.0));
    for (name, share) in arms {
        lines.push(format!("  arm {name}: {:.1}%", share * 100.0));
    }
    lines
}

/// Forecast-panel summary lines: top-1 accuracy + per-class breakdown.
fn forecast_panel(metrics: &ReportMetrics<'_>) -> Vec<String> {
    let f = metrics.forecast;
    let mut lines = vec![format!("Top-1 accuracy: {:.1}%", f.top1_accuracy * 100.0)];
    let mut per_class: Vec<(String, f32)> = f
        .accuracy_per_class
        .iter()
        .map(|(k, v)| (format!("{k:?}"), *v))
        .collect();
    per_class.sort_by(|a, b| a.0.cmp(&b.0));
    for (label, acc) in per_class {
        lines.push(format!("  {label}: {:.1}%", acc * 100.0));
    }
    lines
}

/// Shield-panel summary lines: thrash + excursions + dwell histogram.
fn shield_panel(metrics: &ReportMetrics<'_>) -> Vec<String> {
    let s = metrics.shield;
    let mut lines = vec![
        format!("Thrash events: {}", s.thrash_events),
        format!("HOT excursions: {}", s.hot_excursions),
        format!("MEETING locks: {}", s.meeting_lock_count),
    ];
    let order = ["COOL_AC", "WARM_AC", "HOT", "BATTERY_LOW", "MEETING"];
    for name in order {
        if let Some(pct) = s.state_dwell_pct.get(name) {
            lines.push(format!("  {name}: {:.1}%", pct * 100.0));
        }
    }
    lines
}

/// Energy-panel summary lines: mean watts + total kJ + delta vs baseline.
fn energy_panel(metrics: &ReportMetrics<'_>) -> Vec<String> {
    let e = metrics.energy;
    vec![
        format!("Mean package power: {:.2} W", e.mean_package_power_w),
        format!("Total energy: {:.2} kJ", e.energy_kj_total),
        format!(
            "Saved vs rules baseline: {:.2} kJ ({:+.1}% perf/W)",
            e.energy_saved_vs_baseline_kj, e.perf_per_watt_delta_pct,
        ),
    ]
}

/// Drift-panel summary lines: alarm count + retrain count + last-alarm ts.
fn drift_panel(metrics: &ReportMetrics<'_>) -> Vec<String> {
    let d = metrics.drift;
    let mut lines = vec![
        format!("ADWIN alarms: {}", d.adwin_alarms),
        format!("Retrains triggered: {}", d.retrains_triggered),
    ];
    if let Some(ts) = d.last_alarm_at {
        lines.push(format!("Last alarm: {}", ts.to_rfc3339()));
    } else {
        lines.push("Last alarm: none".to_string());
    }
    lines
}

/// Methodology footer per SPEC §RV.2 ("counterfactual baseline
/// limitation, sample size, version SHA of `sy power`").
fn methodology_footer(metrics: &ReportMetrics<'_>) -> Vec<String> {
    vec![
        format!(
            "Sample size: {} audit entries (1 Hz)",
            metrics.entries.len()
        ),
        "Baseline: counterfactual replay against the rules-only policy".to_string(),
        "Limitation: per-arm power is an offline lookup, not an A/B".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::power::report::metrics::{
        ActivityMetrics, BanditMetrics, DriftMetrics, EnergyMetrics, ForecastMetrics, ShieldMetrics,
    };

    /// Build a fixture metric bundle with realistic values. Used by the
    /// golden-snapshot bullets test below and the round-trip tests in
    /// `render.rs`.
    fn fixture_metrics() -> (
        BanditMetrics,
        ForecastMetrics,
        ShieldMetrics,
        EnergyMetrics,
        DriftMetrics,
        ActivityMetrics,
    ) {
        let mut bandit = BanditMetrics {
            total_decisions: 100,
            reward_mean: 0.42,
            reward_p50: 0.40,
            reward_p95: 0.62,
            cumulative_regret_vs_baseline: -42.5,
            ..Default::default()
        };
        bandit.arm_distribution.insert("browse".to_string(), 0.5);
        bandit.arm_distribution.insert("code".to_string(), 0.3);
        bandit.arm_distribution.insert("idle".to_string(), 0.2);
        let forecast = ForecastMetrics {
            residual_mean: 0.0,
            residual_p95: 0.0,
            accuracy_per_class: HashMap::new(),
            top1_accuracy: 0.85,
        };
        let mut shield = ShieldMetrics {
            thrash_events: 5,
            hot_excursions: 2,
            meeting_lock_count: 3,
            ..Default::default()
        };
        shield.state_dwell_pct.insert("COOL_AC".to_string(), 0.5);
        shield.state_dwell_pct.insert("WARM_AC".to_string(), 0.25);
        shield.state_dwell_pct.insert("HOT".to_string(), 0.1);
        shield.state_dwell_pct.insert("MEETING".to_string(), 0.12);
        shield
            .state_dwell_pct
            .insert("BATTERY_LOW".to_string(), 0.03);
        let energy = EnergyMetrics {
            mean_package_power_w: 8.5,
            energy_kj_total: 51.4,
            energy_saved_vs_baseline_kj: 4.2,
            perf_per_watt_delta_pct: 4.2,
        };
        let drift = DriftMetrics {
            adwin_alarms: 0,
            last_alarm_at: None,
            retrains_triggered: 0,
        };
        let activity = ActivityMetrics::default();
        (bandit, forecast, shield, energy, drift, activity)
    }

    /// Roadmap test (renamed from `generates_well_formed_typst` after
    /// the typst→pdf-writer pivot — see [`super::super::render`] preamble).
    /// `ReportTemplate::build` never panics, every panel emits at
    /// least one line, and the executive summary always carries
    /// exactly three bullets so the PDF layout (one line per bullet
    /// under "Executive summary") stays predictable.
    #[test]
    fn build_produces_well_formed_template() {
        let (b, f, s, e, d, a) = fixture_metrics();
        let metrics = ReportMetrics {
            bandit: &b,
            forecast: &f,
            shield: &s,
            energy: &e,
            drift: &d,
            activity: &a,
            entries: &[],
        };
        let tmpl = ReportTemplate::build(
            &metrics,
            ReportHeader {
                host: "host-fixture".to_string(),
                generated_at_rfc3339: "2026-05-20T12:00:00Z".to_string(),
                window_days: 7.0,
                model_version_sha: "rules-baseline".to_string(),
            },
        );
        assert_eq!(tmpl.exec_bullets.len(), 3);
        assert!(!tmpl.bandit_lines.is_empty());
        assert!(!tmpl.forecast_lines.is_empty());
        assert!(!tmpl.shield_lines.is_empty());
        assert!(!tmpl.energy_lines.is_empty());
        assert!(!tmpl.drift_lines.is_empty());
        assert!(!tmpl.methodology_lines.is_empty());
        for line in tmpl
            .exec_bullets
            .iter()
            .chain(tmpl.bandit_lines.iter())
            .chain(tmpl.methodology_lines.iter())
        {
            assert!(!line.is_empty(), "no panel line should be empty");
        }
    }

    /// Golden-snapshot: the three executive-summary bullets read as
    /// coherent English. Mirrors the SPEC §RV.2 example wording so a
    /// future regression (e.g. swapping "saved" for "improved") trips
    /// this test before the PDF lands on disk.
    #[test]
    fn exec_summary_bullets_match_golden_snapshot() {
        let (b, f, s, e, d, a) = fixture_metrics();
        let metrics = ReportMetrics {
            bandit: &b,
            forecast: &f,
            shield: &s,
            energy: &e,
            drift: &d,
            activity: &a,
            entries: &[],
        };
        let bullets = exec_summary(&metrics);
        assert_eq!(bullets.len(), 3, "exec summary must produce 3 bullets");
        assert_eq!(
            bullets[0],
            "Bandit saved 42.5 W\u{00b7}s vs rules baseline over this window",
        );
        assert_eq!(bullets[1], "No drift alarms");
        assert_eq!(bullets[2], "Shield held MEETING for 12% of the window");
    }
}
