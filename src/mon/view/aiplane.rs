//! Aiplane panel — queue-depth bars per `WorkloadKind`, warm-pool
//! gauges, and a p99 latency sparkline across kinds.
//!
//! Bar count matches the snapshot's `queue_depth` `BTreeMap` cardinality;
//! one [`Op::FillRect`] per workload kind. Warm-pool size per kind renders
//! as a small gauge below the bar so an operator can see at a glance
//! which workload is over-subscribed.

use std::collections::BTreeMap;

use iced::{Point, Rectangle, Size};
use sy_core::mon::snapshot::AiplanePanel;

use super::super::state::{metric_matches, State};
use super::super::theme::Palette;
use super::super::widgets::gauge::Gauge;
use super::super::widgets::sparkline::Sparkline;
use super::super::widgets::Recorder;

const WARM_FULL: f32 = 8.0;

#[derive(Debug, Clone, PartialEq)]
pub struct AiplaneViewData {
    pub queue_depth: BTreeMap<String, u32>,
    pub warm: BTreeMap<String, u32>,
    pub latency_p99_ms: BTreeMap<String, f32>,
    pub errors_total: u64,
}

pub fn panel_data(state: &State) -> AiplaneViewData {
    let aiplane = state
        .latest
        .as_ref()
        .map(|s| s.aiplane.clone())
        .unwrap_or_default();
    let AiplanePanel {
        queue_depth,
        warm,
        latency_p99_ms,
        errors_total,
    } = aiplane;
    let mut data = AiplaneViewData {
        queue_depth,
        warm,
        latency_p99_ms,
        errors_total,
    };
    if state.filter.is_some() {
        data.queue_depth
            .retain(|n, _| metric_matches(&state.filter, n));
        data.warm.retain(|n, _| metric_matches(&state.filter, n));
        data.latency_p99_ms
            .retain(|n, _| metric_matches(&state.filter, n));
    }
    data
}

pub fn draw_into(state: &State, palette: &Palette, area: Rectangle, recorder: &mut dyn Recorder) {
    let data = panel_data(state);
    recorder.stroke_rect(
        Point::new(area.x, area.y),
        Size::new(area.width, area.height),
        palette.ink,
        1.0,
    );
    recorder.text(
        "aiplane",
        Point::new(area.x + 6.0, area.y + 6.0),
        12.0,
        palette.ink,
    );
    let chart_top = area.y + 24.0;
    let chart_h = (area.height * 0.40).max(20.0);
    let n = data.queue_depth.len();
    if n == 0 {
        recorder.text(
            "(no workloads)",
            Point::new(area.x + 8.0, chart_top + 4.0),
            11.0,
            palette.ink,
        );
        return;
    }
    let max_depth = data.queue_depth.values().copied().max().unwrap_or(0).max(1) as f32;
    let bar_w = ((area.width - 16.0) / n as f32) - 4.0;
    for (i, (kind, depth)) in data.queue_depth.iter().enumerate() {
        let x = area.x + 8.0 + i as f32 * (bar_w + 4.0);
        let h = chart_h * (*depth as f32 / max_depth);
        recorder.fill_rect(
            Point::new(x, chart_top + (chart_h - h)),
            Size::new(bar_w, h.max(2.0)),
            palette.accent,
        );
        recorder.text(
            kind,
            Point::new(x, chart_top + chart_h + 2.0),
            9.0,
            palette.ink,
        );
        let warm = data.warm.get(kind).copied().unwrap_or(0) as f32;
        let warm_norm = (warm / WARM_FULL).clamp(0.0, 1.0);
        Gauge::new(warm_norm, "warm", palette.ok, palette.ink).draw_into(
            recorder,
            Rectangle {
                x,
                y: chart_top + chart_h + 14.0,
                width: bar_w,
                height: (area.height * 0.30).max(28.0),
            },
        );
    }
    // p99 latency across kinds. Lead with 0 so the polyline is visible
    // when at least one kind has nonzero p99.
    let mut p99: Vec<f32> = vec![0.0];
    p99.extend(data.latency_p99_ms.values().copied());
    Sparkline::new(&p99, palette.warn).draw_into(
        recorder,
        Rectangle {
            x: area.x + 8.0,
            y: area.y + area.height - 28.0,
            width: area.width - 16.0,
            height: 14.0,
        },
    );
    recorder.text(
        &format!("errors total: {}", data.errors_total),
        Point::new(area.x + 8.0, area.y + area.height - 12.0),
        10.0,
        palette.ink,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;
    use sy_core::mon::snapshot::SystemSnapshot;

    fn state_with(panel: AiplanePanel) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        let mut state = State::new(ring);
        state.latest = Some(SystemSnapshot {
            aiplane: panel,
            ..Default::default()
        });
        state
    }

    fn bounds() -> Rectangle {
        Rectangle {
            x: 0.0,
            y: 0.0,
            width: 400.0,
            height: 240.0,
        }
    }

    /// SPEC: one queue-depth bar per `WorkloadKind` in the snapshot.
    #[test]
    fn queue_depth_bars_per_kind() {
        let mut queue_depth = BTreeMap::new();
        queue_depth.insert("embed".to_string(), 3u32);
        queue_depth.insert("rerank".to_string(), 5u32);
        queue_depth.insert("ocr".to_string(), 1u32);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(
            &state_with(AiplanePanel {
                queue_depth: queue_depth.clone(),
                ..AiplanePanel::default()
            }),
            &palette,
            bounds(),
            &mut rec,
        );
        let bars = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::FillRect { .. }))
            .count();
        assert_eq!(bars, queue_depth.len(), "one fill_rect per WorkloadKind");
    }

    #[test]
    fn empty_queue_renders_no_workloads_label() {
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(
            &state_with(AiplanePanel::default()),
            &palette,
            bounds(),
            &mut rec,
        );
        assert!(rec.ops.iter().any(|op| matches!(op,
            Op::Text { content, .. } if content.contains("(no workloads)"))));
    }

    /// Each kind also gets a warm-pool gauge → one arc per kind.
    #[test]
    fn warm_gauge_per_kind() {
        let mut queue_depth = BTreeMap::new();
        queue_depth.insert("embed".to_string(), 2u32);
        queue_depth.insert("rerank".to_string(), 3u32);
        let mut warm = BTreeMap::new();
        warm.insert("embed".to_string(), 1u32);
        warm.insert("rerank".to_string(), 4u32);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(
            &state_with(AiplanePanel {
                queue_depth,
                warm,
                ..AiplanePanel::default()
            }),
            &palette,
            bounds(),
            &mut rec,
        );
        let arcs = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Arc { width, .. } if *width >= 3.0))
            .count();
        assert_eq!(arcs, 2);
    }
}
