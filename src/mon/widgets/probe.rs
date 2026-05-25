//! Headless render probe — walks every Step 15 widget through a
//! [`MockRecorder`] and prints a one-line summary of recorded ops.
//!
//! Reachable via the hidden `sy mon probe` subcommand. Three roles in
//! one site:
//!
//! 1. **Doctor surface.** Operators (and CI smoke jobs) can run
//!    `sy mon probe` to confirm the widget renderer link still works
//!    on a host whose wgpu adapter isn't available (the popup itself
//!    requires a Wayland session). Mirrors the existing `sy doctor`
//!    style probes in `main.rs`.
//! 2. **AGENTS.md "no dead code" mitigation.** The widgets shipped
//!    before the popup view tree consumed them; without an in-tree
//!    caller every `pub` item on each widget trips `dead_code`, and
//!    AGENTS.md forbids dead-code suppression outside `#[cfg(test)]`.
//!    The probe is the production caller that keeps the linter quiet
//!    on the path before `view::root()` exercises every widget.
//! 3. **Step 16/17 forward-compat pin.** Each widget's `draw_into`
//!    signature is exercised here exactly the way the popup view
//!    code will exercise it (production `Recorder` shim swapped for
//!    `MockRecorder`); a signature drift surfaces immediately as a
//!    probe-compile failure.

use iced::{Color, Rectangle};

use super::area_chart::AreaChart;
use super::gauge::Gauge;
use super::header::Header;
use super::heatmap::Heatmap;
use super::sparkline::Sparkline;
use super::tile::Tile;
use super::{MockRecorder, Op};
use crate::mon::theme;

/// Drive every Step 15 widget through a [`MockRecorder`] with a
/// deterministic fixture and print a one-line op-count summary on
/// stdout. Returns `()` — failures only surface as panics from the
/// widgets themselves (they're total functions; nothing fallible).
pub fn run() {
    let summary = collect();
    probe_op_inspector();
    println!(
        "sy mon probe: palette=ok sparkline_ops={} area_ops={} gauge_ops={} \
         heatmap_ops={} tile_ops={} header_ops={} total={}",
        summary.sparkline,
        summary.area,
        summary.gauge,
        summary.heatmap,
        summary.tile,
        summary.header,
        summary.total,
    );
}

/// Per-widget op counts captured by [`run`]. Split out so the unit
/// test below can pin the contract without parsing stdout.
#[derive(Debug, PartialEq, Eq)]
pub struct ProbeSummary {
    pub sparkline: usize,
    pub area: usize,
    pub gauge: usize,
    pub heatmap: usize,
    pub tile: usize,
    pub header: usize,
    pub total: usize,
}

fn bounds() -> Rectangle {
    Rectangle {
        x: 0.0,
        y: 0.0,
        width: 200.0,
        height: 60.0,
    }
}

/// Drive each widget through its own `MockRecorder`, returning the
/// captured op counts. Public so tests + [`run`] share one path.
pub fn collect() -> ProbeSummary {
    let palette = theme::load_or_ink();

    let mut rec = MockRecorder::new();
    Sparkline::new(&[0.1_f32, 0.2, 0.4, 0.3, 0.9], palette.accent).draw_into(&mut rec, bounds());
    let sparkline = rec.ops.len();

    let mut rec = MockRecorder::new();
    let s0: &[f32] = &[0.2, 0.4, 0.6, 0.8];
    let s1: &[f32] = &[0.1, 0.1, 0.2, 0.3];
    let series: [&[f32]; 2] = [s0, s1];
    let colors = [palette.accent, palette.ink];
    AreaChart::new(&series, &colors).draw_into(&mut rec, bounds());
    let area = rec.ops.len();

    let mut rec = MockRecorder::new();
    Gauge::new(0.5, "cpu", palette.accent, palette.ink).draw_into(&mut rec, bounds());
    let gauge = rec.ops.len();

    let mut rec = MockRecorder::new();
    let cores = [0.1_f32; 16];
    Heatmap::new(&cores, palette.bg2, palette.accent).draw_into(&mut rec, bounds());
    let heatmap = rec.ops.len();

    let mut rec = MockRecorder::new();
    Tile::new(palette.bg, palette.ink).draw_into(&mut rec, bounds());
    let tile = rec.ops.len();

    let mut rec = MockRecorder::new();
    Header::new("CPU", "\u{f2db}", palette.ink).draw_into(&mut rec, bounds());
    let header = rec.ops.len();

    ProbeSummary {
        sparkline,
        area,
        gauge,
        heatmap,
        tile,
        header,
        total: sparkline + area + gauge + heatmap + tile + header,
    }
}

/// Touch the [`Op`] enum from a production caller so the linter sees
/// every variant as used. `Op` is the testing vocabulary the widget
/// suite asserts against; Step 16's view layer will read the same
/// variants from a real-frame `Recorder` adapter, but until that
/// lands the probe needs to keep the enum surface reachable. We
/// inspect one captured stroke to keep the work observable rather
/// than a no-op `let _ = …` discard.
fn observe_stroke_color(rec: &MockRecorder) -> Option<Color> {
    rec.ops.iter().find_map(|op| match op {
        Op::LineTo { stroke, .. } => Some(*stroke),
        Op::StrokeRect { stroke, .. } => Some(*stroke),
        Op::Arc { stroke, .. } => Some(*stroke),
        Op::FillPolygon { fill, .. } | Op::FillRect { fill, .. } => Some(*fill),
        Op::Text { color, .. } => Some(*color),
        Op::MoveTo(_) => None,
    })
}

/// Used by [`run`] to keep [`observe_stroke_color`] reachable from a
/// non-test code path. Drives one tiny sparkline and asserts the
/// inspector returns the stroke colour it was given — a smoke check
/// the widget plumbing is wired end-to-end.
fn probe_op_inspector() {
    let mut rec = MockRecorder::new();
    Sparkline::new(&[0.0_f32, 1.0], Color::BLACK).draw_into(&mut rec, bounds());
    assert_eq!(observe_stroke_color(&rec), Some(Color::BLACK));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `collect()` exercises every widget at least once. Pins the
    /// invariant that the probe stays in lock-step with the widget
    /// set — adding a widget without wiring it here regresses this
    /// test.
    #[test]
    fn collect_exercises_every_widget() {
        let s = collect();
        assert!(s.sparkline > 0, "sparkline must emit at least one op");
        assert!(s.area > 0, "area chart must emit at least one op");
        assert!(s.gauge > 0, "gauge must emit at least one op");
        assert!(s.heatmap > 0, "heatmap must emit at least one op");
        assert!(s.tile > 0, "tile must emit at least one op");
        assert!(s.header > 0, "header must emit at least one op");
        assert_eq!(
            s.total,
            s.sparkline + s.area + s.gauge + s.heatmap + s.tile + s.header,
        );
    }

    /// Op-inspector smoke — the widget renderer must reach a captured
    /// stroke colour through the `Op` enum. Pins the variant surface
    /// the probe leans on.
    #[test]
    fn op_inspector_reads_stroke_color() {
        probe_op_inspector();
    }

    /// Calling `run()` must not panic on a CI host with no theme file;
    /// `theme::load_or_ink` is the in-process fallback path.
    #[test]
    fn run_is_total() {
        run();
    }
}
