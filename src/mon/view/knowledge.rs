//! Knowledge panel — embed-throughput + search-QPS gauges, plus
//! collection / indexed-doc counters as text (the latter are unbounded
//! integers; a gauge needs a bounded saturation point so we keep them
//! as text rows below the charts).

use iced::{Point, Rectangle, Size};
use sy_core::mon::snapshot::{KnowledgePanel, SystemSnapshot};

use super::super::state::State;
use super::super::theme::Palette;
use super::super::widgets::gauge::Gauge;
use super::super::widgets::Recorder;

/// Saturation thresholds for the two gauges. Anything above these pins
/// the sweep at 1.0; the numbers are starting-point heuristics for a
/// daily-driver laptop and can be tuned without breaking tests.
const EMBED_THROUGHPUT_FULL: f32 = 64.0; // docs/s
const SEARCH_QPS_FULL: f32 = 8.0; // queries/s

#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeViewData {
    pub collections: u32,
    pub docs_indexed: u64,
    pub embed_throughput_docs_per_s: f32,
    pub search_qps: f32,
}

pub fn panel_data(state: &State) -> KnowledgeViewData {
    let snap = state.latest.as_ref().cloned().unwrap_or_default();
    panel_data_from(&snap)
}

fn panel_data_from(snap: &SystemSnapshot) -> KnowledgeViewData {
    let KnowledgePanel {
        collections,
        docs_indexed,
        embed_throughput_docs_per_s,
        search_qps,
    } = snap.knowledge;
    KnowledgeViewData {
        collections,
        docs_indexed,
        embed_throughput_docs_per_s,
        search_qps,
    }
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
        "knowledge",
        Point::new(area.x + 6.0, area.y + 6.0),
        12.0,
        palette.ink,
    );
    // Two side-by-side gauges in the upper band.
    let gauge_band_h = (area.height * 0.55).max(40.0);
    let gauge_w = (area.width - 24.0) / 2.0;
    let embed_norm = (data.embed_throughput_docs_per_s / EMBED_THROUGHPUT_FULL).clamp(0.0, 1.0);
    let search_norm = (data.search_qps / SEARCH_QPS_FULL).clamp(0.0, 1.0);
    Gauge::new(embed_norm, "embed/s", palette.accent, palette.ink).draw_into(
        recorder,
        Rectangle {
            x: area.x + 8.0,
            y: area.y + 22.0,
            width: gauge_w,
            height: gauge_band_h,
        },
    );
    Gauge::new(search_norm, "qps", palette.ok, palette.ink).draw_into(
        recorder,
        Rectangle {
            x: area.x + 16.0 + gauge_w,
            y: area.y + 22.0,
            width: gauge_w,
            height: gauge_band_h,
        },
    );
    // Unbounded counters as text in the bottom band.
    let rows = [
        format!("collections: {}", data.collections),
        format!("docs indexed: {}", data.docs_indexed),
    ];
    let mut y = area.y + 22.0 + gauge_band_h + 6.0;
    for r in &rows {
        recorder.text(r, Point::new(area.x + 8.0, y), 11.0, palette.ink);
        y += 14.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;

    fn state_with(panel: KnowledgePanel) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        let mut state = State::new(ring);
        let snap = SystemSnapshot {
            knowledge: panel,
            ..Default::default()
        };
        state.latest = Some(snap);
        state
    }

    fn bounds() -> Rectangle {
        Rectangle {
            x: 0.0,
            y: 0.0,
            width: 400.0,
            height: 200.0,
        }
    }

    /// Two gauges → two arcs.
    #[test]
    fn renders_two_gauges() {
        let panel = KnowledgePanel {
            collections: 4,
            docs_indexed: 17_402,
            embed_throughput_docs_per_s: 32.1,
            search_qps: 0.4,
        };
        let state = state_with(panel);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        let arcs = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Arc { width, .. } if *width >= 3.0))
            .count();
        assert_eq!(arcs, 2, "embed + search → 2 gauges");
    }

    /// Counter rows still surface as text labels.
    #[test]
    fn counter_rows_render_as_text() {
        let panel = KnowledgePanel {
            collections: 4,
            docs_indexed: 17_402,
            embed_throughput_docs_per_s: 32.1,
            search_qps: 0.4,
        };
        let state = state_with(panel);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        let has_collections = rec.ops.iter().any(|op| match op {
            Op::Text { content, .. } => content.contains("collections:"),
            _ => false,
        });
        let has_docs = rec.ops.iter().any(|op| match op {
            Op::Text { content, .. } => content.contains("docs indexed:"),
            _ => false,
        });
        assert!(has_collections);
        assert!(has_docs);
    }
}
