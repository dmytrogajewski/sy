//! Net panel — per-interface rx/tx sparklines + cumulative byte totals.
//!
//! Step 17 ships the snapshot-only view; per-iface ring columns land in
//! a follow-up when the aggregator picks up per-plane data sources.
//! Until then, each sparkline is a flat line at the current rx/tx
//! value so the chart shape is visible and starts responding the
//! moment the aggregator extends its projection.

use iced::{Point, Rectangle, Size};
use sy_core::mon::snapshot::{NetIfacePanel, SystemSnapshot};

use super::super::state::State;
use super::super::theme::Palette;
use super::super::widgets::sparkline::Sparkline;
use super::super::widgets::Recorder;

#[derive(Debug, Clone, PartialEq)]
pub struct NetPanelData {
    pub ifaces: Vec<NetIfacePanel>,
}

pub fn panel_data(state: &State) -> NetPanelData {
    let snap = state.latest.as_ref().cloned().unwrap_or_default();
    panel_data_from(&snap)
}

fn panel_data_from(snap: &SystemSnapshot) -> NetPanelData {
    NetPanelData {
        ifaces: snap.net.clone(),
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
        "net",
        Point::new(area.x + 6.0, area.y + 6.0),
        12.0,
        palette.ink,
    );
    if data.ifaces.is_empty() {
        recorder.text(
            "(no interfaces)",
            Point::new(area.x + 8.0, area.y + 22.0),
            11.0,
            palette.ink,
        );
        return;
    }
    let top = area.y + 22.0;
    let usable_h = (area.height - 28.0).max(0.0);
    let per_iface_h = (usable_h / data.ifaces.len() as f32).max(20.0);
    for (i, iface) in data.ifaces.iter().enumerate() {
        let row_y = top + i as f32 * per_iface_h;
        if row_y + per_iface_h > area.y + area.height {
            break;
        }
        // Label row: iface name + cumulative totals.
        let label = format!(
            "{}  rx {} / tx {}",
            iface.name,
            humanise(iface.rx_bytes),
            humanise(iface.tx_bytes),
        );
        recorder.text(&label, Point::new(area.x + 8.0, row_y), 10.0, palette.ink);
        // Two stacked sparklines: rx (accent), tx (ok). Synthesize a
        // short flat-line series from the current scalar; the widget
        // needs ≥ 2 samples to record a segment.
        let chart_h = ((per_iface_h - 12.0) * 0.5).max(4.0);
        let rx_chart = Rectangle {
            x: area.x + 8.0,
            y: row_y + 12.0,
            width: area.width - 16.0,
            height: chart_h,
        };
        let tx_chart = Rectangle {
            x: area.x + 8.0,
            y: row_y + 12.0 + chart_h,
            width: area.width - 16.0,
            height: chart_h,
        };
        let rx = synth_series(iface.rx_bytes as f32);
        let tx = synth_series(iface.tx_bytes as f32);
        Sparkline::new(&rx, palette.accent).draw_into(recorder, rx_chart);
        Sparkline::new(&tx, palette.ok).draw_into(recorder, tx_chart);
    }
}

/// Flat-line series so the Sparkline widget paints a visible chart
/// even before per-iface ring columns land. 8 samples → 7 segments.
fn synth_series(v: f32) -> [f32; 8] {
    [v; 8]
}

fn humanise(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    format!("{:.1} {}", v, UNITS[u])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;

    fn state_with(ifaces: Vec<NetIfacePanel>) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        let mut state = State::new(ring);
        let snap = SystemSnapshot {
            net: ifaces,
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

    #[test]
    fn iface_renders_two_sparklines() {
        let state = state_with(vec![NetIfacePanel {
            name: "wlan0".into(),
            rx_bytes: 1_234_567,
            tx_bytes: 89_012,
        }]);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        // Each Sparkline records exactly one MoveTo + N-1 LineTo for N
        // synth samples → 2 MoveTo ops total (one per series).
        let moves = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::MoveTo(_)))
            .count();
        assert_eq!(moves, 2, "1 iface → rx + tx sparkline = 2 MoveTo ops");
    }

    #[test]
    fn iface_label_carries_name_and_totals() {
        let state = state_with(vec![NetIfacePanel {
            name: "wlan0".into(),
            rx_bytes: 1_234_567,
            tx_bytes: 89_012,
        }]);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        let hits = rec.ops.iter().any(|op| match op {
            Op::Text { content, .. } => content.contains("wlan0") && content.contains("rx"),
            _ => false,
        });
        assert!(hits);
    }

    #[test]
    fn humanises_bytes_in_units() {
        assert_eq!(humanise(0), "0.0 B");
        assert_eq!(humanise(1024), "1.0 KiB");
        assert!(humanise(1024 * 1024).starts_with("1.0 MiB"));
    }
}
