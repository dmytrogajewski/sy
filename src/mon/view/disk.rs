//! Disk panel — per-device IO sparkline + in-progress IO gauge.
//!
//! Sparkline tracks the read counter (synth flat-line until per-device
//! ring columns land); the gauge plots `io_in_progress` clamped against
//! a small saturation threshold so any nonzero inflight value renders
//! as a partial sweep. Saturation tinting (warn slot) carries through
//! the gauge stroke colour so the panel still flags busy devices.

use iced::{Point, Rectangle, Size};
use sy_core::mon::snapshot::DiskDevicePanel;

use super::super::state::State;
use super::super::theme::Palette;
use super::super::widgets::gauge::Gauge;
use super::super::widgets::sparkline::Sparkline;
use super::super::widgets::Recorder;

/// Saturation point for the inflight-IO gauge — 8 outstanding IOs fills it.
const INFLIGHT_FULL: f32 = 8.0;

#[derive(Debug, Clone, PartialEq)]
pub struct DiskPanelData {
    pub devices: Vec<DiskDevicePanel>,
}

pub fn panel_data(state: &State) -> DiskPanelData {
    DiskPanelData {
        devices: state
            .latest
            .as_ref()
            .map(|s| s.disk.clone())
            .unwrap_or_default(),
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
        "disk",
        Point::new(area.x + 6.0, area.y + 6.0),
        12.0,
        palette.ink,
    );
    if data.devices.is_empty() {
        recorder.text(
            "(no block devices)",
            Point::new(area.x + 8.0, area.y + 22.0),
            11.0,
            palette.ink,
        );
        return;
    }
    let top = area.y + 22.0;
    let usable_h = (area.height - 28.0).max(0.0);
    let per_dev_h = (usable_h / data.devices.len() as f32).max(24.0);
    let gauge_w = 64.0_f32.min(area.width * 0.25);
    for (i, dev) in data.devices.iter().enumerate() {
        let row_y = top + i as f32 * per_dev_h;
        if row_y + per_dev_h > area.y + area.height {
            break;
        }
        let active_color = if dev.io_in_progress > 0 {
            palette.warn
        } else {
            palette.ink
        };
        let label = format!(
            "{}  r {} / w {} / inflight {}",
            dev.name, dev.reads, dev.writes, dev.io_in_progress
        );
        recorder.text(&label, Point::new(area.x + 8.0, row_y), 10.0, active_color);
        let chart_y = row_y + 12.0;
        let chart_h = (per_dev_h - 14.0).max(6.0);
        let series = [dev.reads as f32; 8];
        Sparkline::new(&series, palette.accent).draw_into(
            recorder,
            Rectangle {
                x: area.x + 8.0,
                y: chart_y,
                width: area.width - 16.0 - gauge_w - 4.0,
                height: chart_h,
            },
        );
        let inflight_norm = (dev.io_in_progress as f32 / INFLIGHT_FULL).clamp(0.0, 1.0);
        let stroke = if dev.io_in_progress > 0 {
            palette.warn
        } else {
            palette.accent
        };
        Gauge::new(inflight_norm, "io", stroke, palette.ink).draw_into(
            recorder,
            Rectangle {
                x: area.x + area.width - gauge_w - 4.0,
                y: chart_y,
                width: gauge_w,
                height: chart_h,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;
    use sy_core::mon::snapshot::SystemSnapshot;

    fn state_with(devices: Vec<DiskDevicePanel>) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        let mut state = State::new(ring);
        state.latest = Some(SystemSnapshot {
            disk: devices,
            ..Default::default()
        });
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
    fn renders_sparkline_and_gauge_per_device() {
        let state = state_with(vec![DiskDevicePanel {
            name: "nvme0n1".into(),
            reads: 100,
            writes: 50,
            io_in_progress: 0,
        }]);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        let moves = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::MoveTo(_)))
            .count();
        let arcs = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Arc { width, .. } if *width >= 3.0))
            .count();
        assert_eq!(moves, 1, "1 device → 1 sparkline");
        assert_eq!(arcs, 1, "1 device → 1 gauge");
    }

    /// Inflight IO tints the gauge stroke with the warn slot.
    #[test]
    fn inflight_io_tints_with_warn_color() {
        let state = state_with(vec![DiskDevicePanel {
            name: "sda".into(),
            reads: 1,
            writes: 1,
            io_in_progress: 4,
        }]);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        let stroke = rec
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Arc { stroke, width, .. } if *width >= 3.0 => Some(*stroke),
                _ => None,
            })
            .expect("disk records a gauge arc");
        assert_eq!(stroke, palette.warn);
    }

    #[test]
    fn renders_device_label() {
        let state = state_with(vec![DiskDevicePanel {
            name: "nvme0n1".into(),
            reads: 100,
            writes: 50,
            io_in_progress: 0,
        }]);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        assert!(rec.ops.iter().any(|op| matches!(op,
            Op::Text { content, .. } if content.contains("nvme0n1"))));
    }
}
