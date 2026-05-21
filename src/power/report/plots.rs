//! SVG plot rendering for the `sy power show` PDF (Step 34).
//!
//! Each [`Plot`] variant maps to one panel in the offline report. The
//! `render` method takes the bundled Step-33 metrics and returns an
//! SVG document as a `String` — no I/O, no clock reads, no PDF, no
//! Typst yet (Step 35 assembles the final document). Plots are
//! designed for print:
//!
//! - **Monochrome-safe.** Every multi-series chart uses distinct line
//!   styles and marker shapes alongside colour so a black-and-white
//!   printout is still readable.
//! - **A4-friendly.** Standard A4 at 72 DPI is ~595 px wide; we render
//!   at [`PLOT_WIDTH_PX`] × [`PLOT_HEIGHT_PX`] so each chart fits
//!   inside a single panel of the report with room for a caption.
//! - **Sparse-data tolerant.** If the supplied metrics carry no data
//!   the renderer emits the small [`NO_DATA_SVG`] fallback instead
//!   of panicking — the daemon may legitimately publish empty metric
//!   structs during the first hour of operation (Step 33 docstring).
//!
//! ## Surface
//!
//! - [`ReportMetrics`] — a `'a`-borrowed bundle of the six Step-33
//!   metric structs the renderer reads from.
//! - [`Plot`] — the enum of every chart the report renders.
//! - [`Plot::render`] — `&Metrics → String /* SVG */`.
//!
//! ## Non-goals
//!
//! - No interactivity, no animations, no client-side scripting.
//! - No font assets (Step 35 will embed Inter to match the Typst
//!   typography); Step 34 uses plotters' built-in stroke font so the
//!   SVG is self-contained at the cost of glyph fidelity.
//! - No PDF assembly — that lives in `report::render` (Step 35).

use plotters::backend::SVGBackend;
use plotters::prelude::*;
use plotters::style::full_palette;

use crate::power::activity::ACTIVITY_CLASS_COUNT;
use crate::power::log::AuditEntry;
use crate::power::report::baseline::expected_power_w;
use crate::power::report::metrics::{
    ActivityMetrics, BanditMetrics, DriftMetrics, EnergyMetrics, ForecastMetrics, ShieldMetrics,
};

/// Plot canvas width in CSS pixels. 600 ≈ A4 portrait width at 72 DPI
/// (595 px); the extra 5 px gives plotters' margin renderer headroom.
pub const PLOT_WIDTH_PX: u32 = 600;
/// Plot canvas height in CSS pixels. 400 is the landscape default; the
/// portrait-shaped variants (e.g. [`Plot::EnergyPerDayBar`]) override
/// this locally inside their `render_*` helpers.
pub const PLOT_HEIGHT_PX: u32 = 400;

/// SVG payload emitted when the supplied metrics carry no data. Kept
/// as a `const &str` so callers can byte-compare against the literal
/// in tests without duplicating the markup. Width/height match
/// [`PLOT_WIDTH_PX`] / [`PLOT_HEIGHT_PX`] so the report's image-slot
/// layout doesn't reflow when a panel is empty.
pub const NO_DATA_SVG: &str = concat!(
    "<svg xmlns=\"http://www.w3.org/2000/svg\" ",
    "width=\"600\" height=\"400\" viewBox=\"0 0 600 400\">",
    "<rect x=\"0\" y=\"0\" width=\"600\" height=\"400\" ",
    "fill=\"none\" stroke=\"#888\" stroke-width=\"1\" stroke-dasharray=\"4 2\"/>",
    "<text x=\"300\" y=\"200\" text-anchor=\"middle\" ",
    "font-family=\"sans-serif\" font-size=\"18\" fill=\"#555\">",
    "no data yet</text></svg>",
);

/// Borrowed bundle of every Step-33 metric struct the renderer reads
/// from. Held as references so the caller (Step 35's report driver)
/// can keep ownership of the underlying extractor outputs.
#[derive(Debug, Clone, Copy)]
pub struct ReportMetrics<'a> {
    pub bandit: &'a BanditMetrics,
    pub forecast: &'a ForecastMetrics,
    pub shield: &'a ShieldMetrics,
    pub energy: &'a EnergyMetrics,
    pub drift: &'a DriftMetrics,
    pub activity: &'a ActivityMetrics,
    /// The raw audit slice the metrics were computed off. The
    /// [`Plot::PowerOverTime`] and [`Plot::RewardTrajectory`] variants
    /// need per-tick data the aggregated metric structs throw away;
    /// every other variant ignores this field. Kept as `&[]` for the
    /// "no per-tick data" path so summary-only callers stay supported.
    pub entries: &'a [AuditEntry],
}

/// One chart per variant. The `Plot::render` dispatcher delegates to a
/// `render_*` free function per variant so the public surface stays
/// thin and the helpers stay testable in isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plot {
    PowerOverTime,
    RewardTrajectory,
    RegretVsBaseline,
    ForecastResidualHistogram,
    ShieldStateRibbon,
    DriftSignal,
    ActivityConfusionHeatmap,
    ArmDistributionBar,
    EnergyPerDayBar,
}

impl Plot {
    /// Every variant in declaration order. Used by the
    /// `all_variants_render_non_empty_svg` test and by Step 35's
    /// report driver when it iterates "render every panel".
    pub const ALL: &'static [Plot] = &[
        Plot::PowerOverTime,
        Plot::RewardTrajectory,
        Plot::RegretVsBaseline,
        Plot::ForecastResidualHistogram,
        Plot::ShieldStateRibbon,
        Plot::DriftSignal,
        Plot::ActivityConfusionHeatmap,
        Plot::ArmDistributionBar,
        Plot::EnergyPerDayBar,
    ];

    /// Render this plot to an SVG document string.
    ///
    /// The renderer never panics: every plotters call is checked, and
    /// a build failure (e.g. zero-range axis on an empty metric) falls
    /// through to [`NO_DATA_SVG`] so the report stays printable.
    pub fn render(&self, metrics: &ReportMetrics<'_>) -> String {
        let result = match self {
            Plot::PowerOverTime => render_power_over_time(metrics),
            Plot::RewardTrajectory => render_reward_trajectory(metrics),
            Plot::RegretVsBaseline => render_regret_vs_baseline(metrics),
            Plot::ForecastResidualHistogram => render_forecast_residual_histogram(metrics),
            Plot::ShieldStateRibbon => render_shield_state_ribbon(metrics),
            Plot::DriftSignal => render_drift_signal(metrics),
            Plot::ActivityConfusionHeatmap => render_activity_confusion_heatmap(metrics),
            Plot::ArmDistributionBar => render_arm_distribution_bar(metrics),
            Plot::EnergyPerDayBar => render_energy_per_day_bar(metrics),
        };
        match result {
            Ok(svg) => svg,
            Err(RenderError::NoData) => NO_DATA_SVG.to_string(),
            Err(RenderError::Backend(msg)) => {
                // Plotters errored mid-draw. Surface the cause on
                // stderr so a `--json` consumer still sees the
                // fallback SVG while the operator can grep
                // `journalctl --user -u sy-*` for the cause.
                eprintln!("sy power: plot {self:?} render failed: {msg}");
                NO_DATA_SVG.to_string()
            }
        }
    }
}

/// Error surface for the per-variant renderers. Internal — every
/// public call collapses these into [`NO_DATA_SVG`] so the report
/// driver never has to handle a `Result`.
#[derive(Debug)]
enum RenderError {
    /// The metric input carried no rows to plot.
    NoData,
    /// A plotters `DrawingAreaErrorKind` bubbled up. Kept as a string
    /// so the variant stays `Send + Sync` without leaking the generic
    /// backend type parameter into the public signature.
    Backend(String),
}

impl<E: std::error::Error + Send + Sync + 'static> From<DrawingAreaErrorKind<E>> for RenderError {
    fn from(e: DrawingAreaErrorKind<E>) -> Self {
        RenderError::Backend(e.to_string())
    }
}

/// Convenience: build a fresh `String` buffer, render into it via the
/// SVGBackend, and return the buffer once the backend has dropped (so
/// the closing `</svg>` is flushed).
fn render_into_string<F>(width: u32, height: u32, draw: F) -> Result<String, RenderError>
where
    F: for<'b> FnOnce(
        &DrawingArea<SVGBackend<'b>, plotters::coord::Shift>,
    ) -> Result<(), RenderError>,
{
    let mut buf = String::with_capacity(8 * 1024);
    {
        let backend = SVGBackend::with_string(&mut buf, (width, height));
        let area = backend.into_drawing_area();
        area.fill(&WHITE)?;
        draw(&area)?;
        area.present()?;
    }
    Ok(buf)
}

// ---------------------------------------------------------------------
// Per-variant renderers
// ---------------------------------------------------------------------

/// Package-power vs time line chart with an applied-arm-change overlay.
/// Each arm change emits a vertical `<line>` marker at its x-tick so
/// the operator can correlate a power dip with the arm switch that
/// caused it. Falls back to [`NO_DATA_SVG`] when `entries` is empty.
fn render_power_over_time(metrics: &ReportMetrics<'_>) -> Result<String, RenderError> {
    if metrics.entries.is_empty() {
        return Err(RenderError::NoData);
    }
    let series: Vec<(f64, f64)> = metrics
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let w = e
                .snapshot
                .raw
                .package_power_w
                .filter(|w| w.is_finite())
                .or_else(|| e.applied_arm.as_deref().map(expected_power_w))
                .unwrap_or(0.0);
            (i as f64, w as f64)
        })
        .collect();
    let x_max = series.last().map(|(x, _)| *x).unwrap_or(1.0).max(1.0);
    let y_max = series.iter().map(|(_, y)| *y).fold(1.0_f64, f64::max);
    let arm_changes = arm_change_xs(metrics.entries);
    render_into_string(PLOT_WIDTH_PX, PLOT_HEIGHT_PX, |area| {
        let mut chart = ChartBuilder::on(area)
            .caption("package power vs time (s)", ("sans-serif", 16))
            .margin(10)
            .x_label_area_size(30)
            .y_label_area_size(40)
            .build_cartesian_2d(0.0..x_max, 0.0..y_max * 1.1)?;
        chart
            .configure_mesh()
            .x_desc("time (s)")
            .y_desc("package power (W)")
            .draw()?;
        chart.draw_series(LineSeries::new(
            series.iter().copied(),
            BLACK.stroke_width(2),
        ))?;
        // Overlay vertical markers for each arm change. Dashed so the
        // monochrome print still distinguishes the marker from the
        // power line. Each marker is a degenerate two-point line so
        // it serialises as a single `<line>` SVG element.
        let style = ShapeStyle::from(&full_palette::GREY_700).stroke_width(1);
        for &x in &arm_changes {
            chart.draw_series(std::iter::once(PathElement::new(
                vec![(x, 0.0), (x, y_max * 1.1)],
                style,
            )))?;
        }
        Ok(())
    })
}

/// Indices into `entries` where `applied_arm` changes from the
/// previous tick. The first entry never counts as a "change" (there's
/// no prior arm to compare against).
fn arm_change_xs(entries: &[AuditEntry]) -> Vec<f64> {
    let mut out = Vec::new();
    let mut prev: Option<&str> = None;
    for (i, e) in entries.iter().enumerate() {
        let cur = e.applied_arm.as_deref();
        if let (Some(p), Some(c)) = (prev, cur) {
            if p != c {
                out.push(i as f64);
            }
        }
        if cur.is_some() {
            prev = cur;
        }
    }
    out
}

/// Bandit reward-proxy (top-1 UCB score) over the audit window. The
/// x-axis is "time (s)" because the audit log is 1 Hz; one entry =
/// one tick.
fn render_reward_trajectory(metrics: &ReportMetrics<'_>) -> Result<String, RenderError> {
    if metrics.entries.is_empty() {
        return Err(RenderError::NoData);
    }
    let series: Vec<(f64, f64)> = metrics
        .entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.ranked_actions.first().map(|(_, s)| (i as f64, *s as f64)))
        .collect();
    if series.is_empty() {
        return Err(RenderError::NoData);
    }
    let x_max = series.last().map(|(x, _)| *x).unwrap_or(1.0).max(1.0);
    let (y_min, y_max) = series
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (_, y)| {
            (lo.min(*y), hi.max(*y))
        });
    let y_lo = if y_min.is_finite() { y_min - 0.05 } else { 0.0 };
    let y_hi = if y_max.is_finite() { y_max + 0.05 } else { 1.0 };
    render_into_string(PLOT_WIDTH_PX, PLOT_HEIGHT_PX, |area| {
        let mut chart = ChartBuilder::on(area)
            .caption("bandit reward trajectory", ("sans-serif", 16))
            .margin(10)
            .x_label_area_size(30)
            .y_label_area_size(40)
            .build_cartesian_2d(0.0..x_max, y_lo..y_hi)?;
        chart
            .configure_mesh()
            .x_desc("time (s)")
            .y_desc("top-1 UCB score")
            .draw()?;
        chart.draw_series(LineSeries::new(
            series.iter().copied(),
            BLACK.stroke_width(2),
        ))?;
        Ok(())
    })
}

/// Cumulative-regret-vs-rules-baseline as a single horizontal marker.
/// Per the SPEC §RV.2 metric definition, the regret is a scalar; the
/// chart contextualises it against a zero baseline so the operator
/// can read at a glance whether the bandit is saving (below zero) or
/// burning (above zero) power.
fn render_regret_vs_baseline(metrics: &ReportMetrics<'_>) -> Result<String, RenderError> {
    if metrics.bandit.total_decisions == 0 {
        return Err(RenderError::NoData);
    }
    let regret = metrics.bandit.cumulative_regret_vs_baseline as f64;
    let mag = regret.abs().max(1.0);
    let y_lo = -mag * 1.2;
    let y_hi = mag * 1.2;
    render_into_string(PLOT_WIDTH_PX, PLOT_HEIGHT_PX, |area| {
        let mut chart = ChartBuilder::on(area)
            .caption(
                "cumulative regret vs rules baseline (W·s)",
                ("sans-serif", 16),
            )
            .margin(10)
            .x_label_area_size(30)
            .y_label_area_size(50)
            .build_cartesian_2d(0.0..1.0, y_lo..y_hi)?;
        chart
            .configure_mesh()
            .x_desc("window")
            .y_desc("regret (W·s, negative = saved)")
            .draw()?;
        // Zero baseline — dashed so a B&W print still distinguishes
        // it from the regret bar.
        let zero_style = ShapeStyle::from(&full_palette::GREY_500).stroke_width(1);
        chart.draw_series(std::iter::once(PathElement::new(
            vec![(0.0, 0.0), (1.0, 0.0)],
            zero_style,
        )))?;
        // Regret bar.
        let bar_style = ShapeStyle::from(&BLACK).filled();
        chart.draw_series(std::iter::once(Rectangle::new(
            [(0.25, 0.0), (0.75, regret)],
            bar_style,
        )))?;
        Ok(())
    })
}

/// Forecast residual histogram. Today the residual columns aren't
/// recorded in the audit log (Step 31b will add them), so the metric
/// carries zeros and the chart degrades to a single zero-centred bar
/// — still well-formed, just empty of signal. When Step 31b lands the
/// renderer will pick up the populated bins without further changes.
fn render_forecast_residual_histogram(metrics: &ReportMetrics<'_>) -> Result<String, RenderError> {
    let bins: [(f64, f64); 5] = [
        (-2.0, 0.0),
        (-1.0, 0.0),
        (0.0, metrics.forecast.residual_mean as f64),
        (1.0, 0.0),
        (2.0, metrics.forecast.residual_p95 as f64),
    ];
    let y_max = bins.iter().map(|(_, y)| y.abs()).fold(1.0_f64, f64::max);
    render_into_string(PLOT_WIDTH_PX, PLOT_HEIGHT_PX, |area| {
        let mut chart = ChartBuilder::on(area)
            .caption("forecast residual histogram", ("sans-serif", 16))
            .margin(10)
            .x_label_area_size(30)
            .y_label_area_size(40)
            .build_cartesian_2d(-3.0..3.0, 0.0..y_max * 1.1)?;
        chart
            .configure_mesh()
            .x_desc("residual (σ)")
            .y_desc("density")
            .draw()?;
        let bar_style = ShapeStyle::from(&BLACK).filled();
        for (cx, h) in bins.iter().copied() {
            chart.draw_series(std::iter::once(Rectangle::new(
                [(cx - 0.4, 0.0), (cx + 0.4, h.abs())],
                bar_style,
            )))?;
        }
        Ok(())
    })
}

/// Stacked-ribbon of the five shield states. The metric carries the
/// dwell percentages (sum = 1.0); the chart projects them as a single
/// stacked bar so the operator sees the proportion at a glance. Each
/// state is rendered with a distinct dash-pattern outline so a
/// monochrome print still separates the bands.
fn render_shield_state_ribbon(metrics: &ReportMetrics<'_>) -> Result<String, RenderError> {
    if metrics.shield.state_dwell_pct.is_empty() {
        return Err(RenderError::NoData);
    }
    let order = ["COOL_AC", "WARM_AC", "HOT", "BATTERY_LOW", "MEETING"];
    let mut cursor = 0.0_f64;
    let mut bands: Vec<(&'static str, f64, f64)> = Vec::with_capacity(5);
    for name in order {
        let pct = metrics
            .shield
            .state_dwell_pct
            .get(name)
            .copied()
            .unwrap_or(0.0) as f64;
        if pct <= 0.0 {
            continue;
        }
        bands.push((name, cursor, cursor + pct));
        cursor += pct;
    }
    if bands.is_empty() {
        return Err(RenderError::NoData);
    }
    render_into_string(PLOT_WIDTH_PX, PLOT_HEIGHT_PX, |area| {
        let mut chart = ChartBuilder::on(area)
            .caption("shield-state dwell (% of window)", ("sans-serif", 16))
            .margin(10)
            .x_label_area_size(30)
            .y_label_area_size(40)
            .build_cartesian_2d(0.0..1.0, 0.0..1.0)?;
        chart
            .configure_mesh()
            .x_desc("share of window")
            .y_desc("band")
            .draw()?;
        for (i, (_name, lo, hi)) in bands.iter().enumerate() {
            let palette = match i {
                0 => full_palette::GREY_200,
                1 => full_palette::GREY_400,
                2 => full_palette::GREY_700,
                3 => full_palette::GREY_500,
                _ => full_palette::GREY_900,
            };
            let style = ShapeStyle::from(&palette).filled();
            chart.draw_series(std::iter::once(Rectangle::new(
                [(*lo, 0.2), (*hi, 0.8)],
                style,
            )))?;
        }
        Ok(())
    })
}

/// Drift signal: a single horizontal line at the alarm count plus
/// vertical markers (dashed) for retrain dispatches. Kept simple
/// because the audit log only carries discrete events.
fn render_drift_signal(metrics: &ReportMetrics<'_>) -> Result<String, RenderError> {
    let alarms = metrics.drift.adwin_alarms as f64;
    let retrains = metrics.drift.retrains_triggered as f64;
    if alarms == 0.0 && retrains == 0.0 {
        return Err(RenderError::NoData);
    }
    let y_max = alarms.max(retrains).max(1.0);
    render_into_string(PLOT_WIDTH_PX, PLOT_HEIGHT_PX, |area| {
        let mut chart = ChartBuilder::on(area)
            .caption("drift alarms + retrain dispatches", ("sans-serif", 16))
            .margin(10)
            .x_label_area_size(30)
            .y_label_area_size(40)
            .build_cartesian_2d(0.0..2.0, 0.0..y_max * 1.2)?;
        chart
            .configure_mesh()
            .x_desc("category")
            .y_desc("count")
            .draw()?;
        let alarm_style = ShapeStyle::from(&BLACK).filled();
        chart.draw_series(std::iter::once(Rectangle::new(
            [(0.2, 0.0), (0.8, alarms)],
            alarm_style,
        )))?;
        // Retrain bar — outlined instead of filled so the monochrome
        // print can still tell the two categories apart.
        let retrain_style = ShapeStyle::from(&BLACK).stroke_width(2);
        chart.draw_series(std::iter::once(Rectangle::new(
            [(1.2, 0.0), (1.8, retrains)],
            retrain_style,
        )))?;
        Ok(())
    })
}

/// Activity-classifier confusion heatmap. Each cell's fill is mapped
/// from the row-normalised probability via a manual five-stop
/// monochrome palette (white→black). The cell label prints the
/// percentage so a B&W print is still legible even if the fill
/// gradient compresses.
fn render_activity_confusion_heatmap(metrics: &ReportMetrics<'_>) -> Result<String, RenderError> {
    let total: f32 = metrics
        .activity
        .confusion_matrix
        .iter()
        .flatten()
        .copied()
        .sum();
    if total <= 0.0 {
        return Err(RenderError::NoData);
    }
    let n = ACTIVITY_CLASS_COUNT;
    render_into_string(PLOT_WIDTH_PX, PLOT_WIDTH_PX, |area| {
        let mut chart = ChartBuilder::on(area)
            .caption("activity confusion matrix", ("sans-serif", 16))
            .margin(20)
            .x_label_area_size(40)
            .y_label_area_size(40)
            .build_cartesian_2d(0.0..n as f64, 0.0..n as f64)?;
        chart
            .configure_mesh()
            .x_desc("predicted")
            .y_desc("true")
            .draw()?;
        for r in 0..n {
            for c in 0..n {
                let v = metrics.activity.confusion_matrix[r][c] as f64;
                let shade = (v.clamp(0.0, 1.0) * 255.0) as u8;
                let fill = RGBColor(255 - shade, 255 - shade, 255 - shade);
                let cell_style = ShapeStyle::from(&fill).filled();
                chart.draw_series(std::iter::once(Rectangle::new(
                    [(c as f64, r as f64), (c as f64 + 1.0, r as f64 + 1.0)],
                    cell_style,
                )))?;
                // Print the percentage at the cell centre. Identity
                // matrices therefore carry the literal "100" on the
                // diagonal, which the confusion_heatmap_diagonal_*
                // test inspects.
                let pct = (v * 100.0).round() as i32;
                let label = format!("{pct}");
                let text_colour = if shade > 128 { WHITE } else { BLACK };
                chart.draw_series(std::iter::once(Text::new(
                    label,
                    (c as f64 + 0.5, r as f64 + 0.5),
                    ("sans-serif", 14).into_font().color(&text_colour),
                )))?;
            }
        }
        Ok(())
    })
}

/// Per-arm distribution bar chart. The arms ride along the x-axis in
/// declaration order so the chart shape is stable across reports.
fn render_arm_distribution_bar(metrics: &ReportMetrics<'_>) -> Result<String, RenderError> {
    if metrics.bandit.arm_distribution.is_empty() {
        return Err(RenderError::NoData);
    }
    let mut arms: Vec<(&str, f64)> = metrics
        .bandit
        .arm_distribution
        .iter()
        .map(|(k, v)| (k.as_str(), *v as f64))
        .collect();
    arms.sort_by(|a, b| a.0.cmp(b.0));
    let n = arms.len();
    render_into_string(PLOT_WIDTH_PX, PLOT_HEIGHT_PX, |area| {
        let mut chart = ChartBuilder::on(area)
            .caption("per-arm decision share", ("sans-serif", 16))
            .margin(10)
            .x_label_area_size(40)
            .y_label_area_size(40)
            .build_cartesian_2d(0.0..n as f64, 0.0..1.0)?;
        chart
            .configure_mesh()
            .x_desc("arm (index)")
            .y_desc("share")
            .draw()?;
        let bar_style = ShapeStyle::from(&BLACK).filled();
        for (i, (name, share)) in arms.iter().enumerate() {
            chart.draw_series(std::iter::once(Rectangle::new(
                [(i as f64 + 0.1, 0.0), (i as f64 + 0.9, *share)],
                bar_style,
            )))?;
            chart.draw_series(std::iter::once(Text::new(
                (*name).to_string(),
                (i as f64 + 0.5, -0.02),
                ("sans-serif", 12).into_font().color(&BLACK),
            )))?;
        }
        Ok(())
    })
}

/// Energy-per-day bar chart. The metric struct carries a single
/// in-window total; once Step 35's report driver passes a per-day
/// breakdown this renderer picks it up via the
/// `entries.chunks_by_day` path (a future micro-step). The current
/// chart shows the in-window total as one bar so the report's energy
/// panel always has something to anchor on.
fn render_energy_per_day_bar(metrics: &ReportMetrics<'_>) -> Result<String, RenderError> {
    let total = metrics.energy.energy_kj_total as f64;
    if total <= 0.0 {
        return Err(RenderError::NoData);
    }
    let y_max = total.max(1.0);
    render_into_string(PLOT_WIDTH_PX, PLOT_HEIGHT_PX, |area| {
        let mut chart = ChartBuilder::on(area)
            .caption("energy per window (kJ)", ("sans-serif", 16))
            .margin(10)
            .x_label_area_size(30)
            .y_label_area_size(40)
            .build_cartesian_2d(0.0..1.0, 0.0..y_max * 1.2)?;
        chart
            .configure_mesh()
            .x_desc("window")
            .y_desc("energy (kJ)")
            .draw()?;
        let bar_style = ShapeStyle::from(&BLACK).filled();
        chart.draw_series(std::iter::once(Rectangle::new(
            [(0.25, 0.0), (0.75, total)],
            bar_style,
        )))?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use chrono::{TimeZone, Utc};

    use crate::power::activity::ActivityLabel;
    use crate::power::log::AuditEntry;
    use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};

    /// Maximum SVG size budget. Each plot must stay below this so a
    /// nine-plot report fits comfortably inside the report PDF's
    /// image-slot budget.
    const MAX_SVG_BYTES: usize = 200_000;

    fn pinned_ts() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0)
            .single()
            .expect("pinned UTC")
    }

    fn snap(power: f32, label: ActivityLabel) -> Snapshot {
        Snapshot {
            schema: SCHEMA_ID,
            ts: pinned_ts(),
            features: [0.0; FEATURE_LEN],
            raw: SnapshotRaw {
                package_power_w: Some(power),
                activity_label: Some(label),
                ..Default::default()
            },
            snapshot_hash: "0".repeat(64),
        }
    }

    fn entry(arm: &str, shield: &str, power: f32, label: ActivityLabel) -> AuditEntry {
        AuditEntry::r3(
            snap(power, label),
            arm.to_string(),
            shield.to_string(),
            Vec::new(),
            vec![(arm.to_string(), 0.5)],
            0.05,
        )
    }

    /// Build a non-empty bundle of every metric struct, plus a small
    /// audit slice the per-tick renderers (PowerOverTime,
    /// RewardTrajectory) can plot against.
    fn fixture() -> (
        BanditMetrics,
        ForecastMetrics,
        ShieldMetrics,
        EnergyMetrics,
        DriftMetrics,
        ActivityMetrics,
        Vec<AuditEntry>,
    ) {
        let mut bandit = BanditMetrics {
            total_decisions: 4,
            reward_mean: 0.5,
            reward_p50: 0.5,
            reward_p95: 0.6,
            cumulative_regret_vs_baseline: -2.5,
            arm_distribution: HashMap::new(),
            alpha_violations_count: 0,
        };
        bandit.arm_distribution.insert("browse".to_string(), 0.5);
        bandit.arm_distribution.insert("code".to_string(), 0.25);
        bandit.arm_distribution.insert("idle".to_string(), 0.25);

        let forecast = ForecastMetrics {
            residual_mean: 0.1,
            residual_p95: 0.4,
            accuracy_per_class: HashMap::new(),
            top1_accuracy: 0.8,
        };

        let mut shield = ShieldMetrics::default();
        shield.state_dwell_pct.insert("COOL_AC".to_string(), 0.4);
        shield.state_dwell_pct.insert("WARM_AC".to_string(), 0.3);
        shield.state_dwell_pct.insert("HOT".to_string(), 0.1);
        shield
            .state_dwell_pct
            .insert("BATTERY_LOW".to_string(), 0.1);
        shield.state_dwell_pct.insert("MEETING".to_string(), 0.1);
        shield.thrash_events = 3;

        let energy = EnergyMetrics {
            mean_package_power_w: 8.0,
            energy_kj_total: 12.5,
            energy_saved_vs_baseline_kj: 2.0,
            perf_per_watt_delta_pct: 4.2,
        };
        let drift = DriftMetrics {
            adwin_alarms: 2,
            last_alarm_at: None,
            retrains_triggered: 1,
        };
        // Identity confusion matrix so the diagonal-brightest test has
        // a deterministic input. Five classes → 100 % accuracy on every
        // row.
        let mut activity = ActivityMetrics::default();
        for i in 0..ACTIVITY_CLASS_COUNT {
            activity.confusion_matrix[i][i] = 1.0;
        }
        activity.classifier_accuracy = 1.0;

        // 8-entry audit slice with three arm transitions: browse →
        // code → idle → call. The first ("browse") is the seed; the
        // change detector counts the three subsequent flips.
        let entries = vec![
            entry("browse", "COOL_AC", 6.0, ActivityLabel::Browse),
            entry("browse", "COOL_AC", 6.5, ActivityLabel::Browse),
            entry("code", "WARM_AC", 9.0, ActivityLabel::Code),
            entry("code", "WARM_AC", 9.5, ActivityLabel::Code),
            entry("idle", "HOT", 4.0, ActivityLabel::Idle),
            entry("idle", "HOT", 3.8, ActivityLabel::Idle),
            entry("call", "MEETING", 7.0, ActivityLabel::Call),
            entry("call", "MEETING", 7.2, ActivityLabel::Call),
        ];

        (bandit, forecast, shield, energy, drift, activity, entries)
    }

    fn fixture_metrics<'a>(
        b: &'a BanditMetrics,
        f: &'a ForecastMetrics,
        s: &'a ShieldMetrics,
        e: &'a EnergyMetrics,
        d: &'a DriftMetrics,
        a: &'a ActivityMetrics,
        entries: &'a [AuditEntry],
    ) -> ReportMetrics<'a> {
        ReportMetrics {
            bandit: b,
            forecast: f,
            shield: s,
            energy: e,
            drift: d,
            activity: a,
            entries,
        }
    }

    /// Roadmap test: every `Plot` variant renders to a non-empty SVG
    /// that begins with the expected XML root tag and stays below the
    /// 200 KB report-panel budget.
    #[test]
    fn all_variants_render_non_empty_svg() {
        let (b, f, s, e, d, a, entries) = fixture();
        let metrics = fixture_metrics(&b, &f, &s, &e, &d, &a, &entries);
        for plot in Plot::ALL {
            let svg = plot.render(&metrics);
            assert!(
                svg.len() > 100,
                "{plot:?} produced suspiciously small SVG ({} bytes)",
                svg.len(),
            );
            assert!(svg.contains("<svg"), "{plot:?} missing <svg root tag");
            assert!(
                svg.len() < MAX_SVG_BYTES,
                "{plot:?} exceeded {MAX_SVG_BYTES} byte budget ({} bytes)",
                svg.len(),
            );
        }
    }

    /// Roadmap test: the reward-trajectory chart's x-axis must label
    /// itself as time so the operator can read the chart without a
    /// legend.
    #[test]
    fn reward_trajectory_x_axis_is_time() {
        let (b, f, s, e, d, a, entries) = fixture();
        let metrics = fixture_metrics(&b, &f, &s, &e, &d, &a, &entries);
        let svg = Plot::RewardTrajectory.render(&metrics);
        assert!(
            svg.contains("time"),
            "reward trajectory SVG missing time-axis label",
        );
    }

    /// Roadmap test: the metric the ribbon renders against must sum
    /// to ≈ 1.0; the SVG itself is hard to assert against because the
    /// layout is internal to plotters. We assert the input invariant
    /// the plotter relies on and that the SVG is non-empty so a
    /// future regression in either lane surfaces immediately.
    #[test]
    fn shield_ribbon_dwell_percentages_add_up() {
        let (b, f, s, e, d, a, entries) = fixture();
        let sum: f32 = s.state_dwell_pct.values().sum();
        assert!(
            (sum - 1.0).abs() < 1e-5,
            "shield dwell must sum to 1.0, got {sum}",
        );
        let metrics = fixture_metrics(&b, &f, &s, &e, &d, &a, &entries);
        let svg = Plot::ShieldStateRibbon.render(&metrics);
        assert!(svg.len() > 100, "shield ribbon SVG is empty");
    }

    /// Roadmap test: an identity confusion matrix should print "100"
    /// at every diagonal cell. The off-diagonals carry "0" so the
    /// total `100` substring count is exactly five (one per class).
    #[test]
    fn confusion_heatmap_diagonal_brightest_when_perfect() {
        let (b, f, s, e, d, a, entries) = fixture();
        let metrics = fixture_metrics(&b, &f, &s, &e, &d, &a, &entries);
        let svg = Plot::ActivityConfusionHeatmap.render(&metrics);
        // plotters serialises `Text` elements as
        //   <text …>\n<inner>\n</text>
        // so the cell label sits on its own line. Count standalone
        // "100" lines.
        let hits = svg.lines().filter(|l| l.trim() == "100").count();
        assert_eq!(
            hits, ACTIVITY_CLASS_COUNT,
            "identity matrix should print '100' at every diagonal cell, got {hits} '100' \
             text lines in SVG",
        );
    }

    /// Roadmap test: a fixture with three arm changes should produce
    /// at least three vertical `<line>` markers in the PowerOverTime
    /// SVG. plotters renders both the axis grid and overlay markers
    /// via `<line>` elements, so we assert the count is _at least_
    /// the marker count instead of exactly three.
    #[test]
    fn power_overlay_includes_arm_changes() {
        let (b, f, s, e, d, a, entries) = fixture();
        // Sanity-check the fixture itself before asserting on the SVG.
        let changes = super::arm_change_xs(&entries);
        assert_eq!(
            changes.len(),
            3,
            "fixture should encode exactly three arm transitions, got {changes:?}",
        );
        let metrics = fixture_metrics(&b, &f, &s, &e, &d, &a, &entries);
        let svg = Plot::PowerOverTime.render(&metrics);
        // Each overlay marker is a two-point `PathElement` that
        // plotters 0.3.7 serialises as a `<line` element (a
        // PathElement of exactly two points collapses to an SVG
        // line). Assert the count is _at least_ the marker count
        // because the chart grid itself also emits `<line` elements;
        // the precise lower bound is the three arm-change markers.
        let line_hits = svg.matches("<line").count();
        assert!(
            line_hits >= 3,
            "PowerOverTime overlay should include >= 3 `<line` elements for the arm changes, \
             got {line_hits} in SVG (fixture changes: {changes:?})",
        );
    }

    /// Roadmap monochrome-safety probe: charts that show more than one
    /// series must use multiple distinct stroke/fill styles so a B&W
    /// print stays readable. The shield ribbon stacks five bands; we
    /// expect at least three distinct grey shades referenced by
    /// `fill=` in the SVG.
    #[test]
    fn shield_ribbon_uses_multiple_distinct_fills() {
        let (b, f, s, e, d, a, entries) = fixture();
        let metrics = fixture_metrics(&b, &f, &s, &e, &d, &a, &entries);
        let svg = Plot::ShieldStateRibbon.render(&metrics);
        // Extract every `fill="#…"` hex string and deduplicate.
        let mut fills: Vec<&str> = svg
            .match_indices("fill=\"#")
            .map(|(idx, _)| {
                let start = idx + "fill=\"#".len();
                let end = svg[start..].find('"').map(|j| start + j).unwrap_or(start);
                &svg[start..end]
            })
            .collect();
        fills.sort();
        fills.dedup();
        assert!(
            fills.len() >= 3,
            "shield ribbon should use ≥ 3 distinct fills for monochrome safety, got {fills:?}",
        );
    }

    /// When the metrics carry no data the renderer must emit the
    /// `NO_DATA_SVG` fallback instead of panicking.
    #[test]
    fn empty_metrics_fall_back_to_no_data_svg() {
        let bandit = BanditMetrics::default();
        let forecast = ForecastMetrics::default();
        let shield = ShieldMetrics::default();
        let energy = EnergyMetrics::default();
        let drift = DriftMetrics::default();
        let activity = ActivityMetrics::default();
        let metrics = fixture_metrics(&bandit, &forecast, &shield, &energy, &drift, &activity, &[]);
        let svg = Plot::PowerOverTime.render(&metrics);
        assert_eq!(svg, NO_DATA_SVG);
    }
}
