//! Power panel — dwell-time gauges per arm + cumulative-regret sparkline.
//!
//! Regret line reads from ring column
//! [`crate::mon::collect::ipc::POWER_REGRET_COL`] so panel inputs flow
//! through the same projection the aggregator will eventually fill. The
//! SPEC test `regret_line_uses_history_window` pre-populates that
//! column and asserts the panel's projection echoes it.
//!
//! Dwell renders as one gauge per known arm; the current arm's gauge
//! gets the accent stroke so the operator sees which arm is live.

use iced::{Point, Rectangle, Size};
use sy_core::mon::snapshot::PowerPanel;

use super::super::cli::DEFAULT_HISTORY_SIZE;
use super::super::collect::ipc::POWER_REGRET_COL;
use super::super::state::State;
use super::super::theme::Palette;
use super::super::widgets::gauge::Gauge;
use super::super::widgets::sparkline::Sparkline;
use super::super::widgets::Recorder;

#[derive(Debug, Clone, PartialEq)]
pub struct PowerViewData {
    pub current_arm: String,
    pub dwell_pct: Vec<(String, f32)>,
    pub regret_cum: f32,
    pub regret_history: Vec<f32>,
}

pub fn panel_data(state: &State) -> PowerViewData {
    let snap = state.latest.as_ref().cloned().unwrap_or_default();
    let dwell_pct = snap
        .power
        .dwell_pct
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    let regret_history = state
        .history
        .read_metric(POWER_REGRET_COL, DEFAULT_HISTORY_SIZE as usize)
        .unwrap_or_default();
    let PowerPanel {
        current_arm,
        regret_cum,
        ..
    } = snap.power;
    PowerViewData {
        current_arm,
        dwell_pct,
        regret_cum,
        regret_history,
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
        "power",
        Point::new(area.x + 6.0, area.y + 6.0),
        12.0,
        palette.ink,
    );
    let arm_caption = if data.current_arm.is_empty() {
        "(arm: unset)".to_string()
    } else {
        format!("arm: {}", data.current_arm)
    };
    recorder.text(
        &arm_caption,
        Point::new(area.x + 8.0, area.y + 22.0),
        11.0,
        palette.accent,
    );
    if !data.dwell_pct.is_empty() {
        let band_top = area.y + 38.0;
        let band_h = (area.height * 0.55).max(40.0);
        let gw = (area.width - 16.0) / data.dwell_pct.len() as f32;
        for (i, (arm, pct)) in data.dwell_pct.iter().enumerate() {
            let stroke = if arm == &data.current_arm {
                palette.accent
            } else {
                palette.ink
            };
            Gauge::new(*pct, arm.as_str(), stroke, palette.ink).draw_into(
                recorder,
                Rectangle {
                    x: area.x + 8.0 + i as f32 * gw,
                    y: band_top,
                    width: gw - 4.0,
                    height: band_h,
                },
            );
        }
    }
    Sparkline::new(&data.regret_history, palette.accent).draw_into(
        recorder,
        Rectangle {
            x: area.x + 8.0,
            y: area.y + area.height - 24.0,
            width: area.width - 16.0,
            height: 14.0,
        },
    );
    recorder.text(
        &format!("regret cum: {:.3}", data.regret_cum),
        Point::new(area.x + 8.0, area.y + area.height - 8.0),
        10.0,
        palette.ink,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;

    fn state_with_regret_history(values: &[f32]) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let mut ring = Ring::open_or_rebuild(&path, DEFAULT_HISTORY_SIZE, 16).expect("ring");
        for v in values {
            let mut row = vec![0.0_f32; 16];
            row[POWER_REGRET_COL] = *v;
            ring.push(&row).expect("push");
        }
        State::new(ring)
    }

    fn bounds() -> Rectangle {
        Rectangle {
            x: 0.0,
            y: 0.0,
            width: 400.0,
            height: 200.0,
        }
    }

    /// SPEC: regret history slice equals the ring's regret column window.
    #[test]
    fn regret_line_uses_history_window() {
        let expected = vec![0.01_f32, 0.04, 0.05, 0.07, 0.11];
        let data = panel_data(&state_with_regret_history(&expected));
        assert_eq!(data.regret_history, expected);
    }

    #[test]
    fn empty_ring_produces_empty_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, DEFAULT_HISTORY_SIZE, 16).expect("ring");
        assert!(panel_data(&State::new(ring)).regret_history.is_empty());
    }

    /// One arc per arm.
    #[test]
    fn dwell_renders_one_gauge_per_arm() {
        use std::collections::BTreeMap;
        use sy_core::mon::snapshot::{PowerPanel, SystemSnapshot};
        let mut dwell = BTreeMap::new();
        dwell.insert("balanced".to_string(), 0.5_f32);
        dwell.insert("perf".to_string(), 0.3);
        dwell.insert("save".to_string(), 0.2);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, DEFAULT_HISTORY_SIZE, 16).expect("ring");
        let mut state = State::new(ring);
        state.latest = Some(SystemSnapshot {
            power: PowerPanel {
                current_arm: "balanced".into(),
                dwell_pct: dwell,
                regret_cum: 0.0,
            },
            ..Default::default()
        });
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        let arcs = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Arc { width, .. } if *width >= 3.0))
            .count();
        assert_eq!(arcs, 3);
    }
}
