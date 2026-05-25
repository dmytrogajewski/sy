//! Accelerator panel — per-GPU util/VRAM/temp gauges + NPU util gauge.
//!
//! Layout: GPUs stack as rows of three gauges (util, VRAM%, temp/100);
//! the NPU sits below with a util gauge plus a label showing firmware
//! version and active holders. The holders text is the operator-visible
//! "which plane is driving the NPU" signal pinned by
//! `npu_panel_shows_holders` and is unbounded (arbitrary process names),
//! so a gauge wouldn't carry the information.

use iced::{Point, Rectangle, Size};
use sy_core::mon::snapshot::{GpuPanel, NpuPanel};

use super::super::state::State;
use super::super::theme::Palette;
use super::super::widgets::gauge::Gauge;
use super::super::widgets::Recorder;

#[derive(Debug, Clone, PartialEq)]
pub struct AccelPanelData {
    pub gpus: Vec<GpuPanel>,
    pub npu: NpuPanel,
}

pub fn panel_data(state: &State) -> AccelPanelData {
    let snap = state.latest.as_ref();
    AccelPanelData {
        gpus: snap.map(|s| s.gpu.clone()).unwrap_or_default(),
        npu: snap.map(|s| s.npu.clone()).unwrap_or_default(),
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
        "accel",
        Point::new(area.x + 6.0, area.y + 6.0),
        12.0,
        palette.ink,
    );
    let mut y = area.y + 22.0;
    let row_h = 56.0_f32;
    let gauge_w = (area.width - 24.0) / 3.0;
    for gpu in &data.gpus {
        let util = (gpu.util_pct as f32 / 100.0).clamp(0.0, 1.0);
        let vram = if gpu.vram_total_mib == 0 {
            0.0
        } else {
            (gpu.vram_used_mib as f32 / gpu.vram_total_mib as f32).clamp(0.0, 1.0)
        };
        let temp = (gpu.temp_c / 100.0).clamp(0.0, 1.0);
        for (i, (label, value)) in [("util", util), ("vram", vram), ("temp", temp)]
            .iter()
            .enumerate()
        {
            let bounds = Rectangle {
                x: area.x + 8.0 + i as f32 * (gauge_w + 4.0),
                y,
                width: gauge_w,
                height: row_h,
            };
            Gauge::new(*value, label, palette.accent, palette.ink).draw_into(recorder, bounds);
        }
        y += row_h + 4.0;
    }
    if data.gpus.is_empty() {
        recorder.text(
            "(no gpu detected)",
            Point::new(area.x + 8.0, y),
            11.0,
            palette.ink,
        );
        y += 14.0;
    }
    let npu_util = (data.npu.util_pct as f32 / 100.0).clamp(0.0, 1.0);
    let npu_stroke = if data.npu.active {
        palette.ok
    } else {
        palette.ink
    };
    Gauge::new(npu_util, "npu", npu_stroke, palette.ink).draw_into(
        recorder,
        Rectangle {
            x: area.x + 8.0,
            y,
            width: gauge_w,
            height: row_h,
        },
    );
    let holders = if data.npu.holders.is_empty() {
        "(idle)".to_string()
    } else {
        data.npu.holders.join(", ")
    };
    let label = format!(
        "{} fw {} | {}",
        if data.npu.vendor.is_empty() {
            "(absent)"
        } else {
            data.npu.vendor.as_str()
        },
        if data.npu.fw_version.is_empty() {
            "?"
        } else {
            data.npu.fw_version.as_str()
        },
        holders,
    );
    recorder.text(
        &label,
        Point::new(area.x + gauge_w + 16.0, y + row_h * 0.5),
        11.0,
        palette.ink,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;
    use sy_core::mon::snapshot::SystemSnapshot;

    fn state_with(gpus: Vec<GpuPanel>, npu: NpuPanel) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        let mut state = State::new(ring);
        state.latest = Some(SystemSnapshot {
            gpu: gpus,
            npu,
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

    /// SPEC: `holders: ["sy-aiplane"]` surfaces in the NPU label.
    #[test]
    fn npu_panel_shows_holders() {
        let npu = NpuPanel {
            vendor: "amd-xdna".into(),
            util_pct: 73,
            active: true,
            fw_version: "1.5.10".into(),
            power_w: 4.2,
            holders: vec!["sy-aiplane".into()],
        };
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state_with(Vec::new(), npu), &palette, bounds(), &mut rec);
        assert!(rec.ops.iter().any(|op| matches!(op,
            Op::Text { content, .. } if content.contains("sy-aiplane"))));
    }

    /// One GPU (util/vram/temp) + NPU = 4 arcs.
    #[test]
    fn one_gpu_renders_four_gauges_with_npu() {
        let gpu = GpuPanel {
            vendor: "amd".into(),
            name: "780M".into(),
            util_pct: 50,
            vram_used_mib: 2048,
            vram_total_mib: 4096,
            temp_c: 50.0,
            power_w: 12.0,
        };
        let npu = NpuPanel {
            vendor: "amd-xdna".into(),
            util_pct: 25,
            active: true,
            fw_version: "1.5.10".into(),
            power_w: 1.0,
            holders: Vec::new(),
        };
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state_with(vec![gpu], npu), &palette, bounds(), &mut rec);
        let arcs = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Arc { width, .. } if *width >= 3.0))
            .count();
        assert_eq!(arcs, 4);
    }

    #[test]
    fn no_npu_present_renders_absent_label() {
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(
            &state_with(Vec::new(), NpuPanel::default()),
            &palette,
            bounds(),
            &mut rec,
        );
        assert!(rec.ops.iter().any(|op| matches!(op,
            Op::Text { content, .. } if content.contains("(absent)"))));
    }
}
