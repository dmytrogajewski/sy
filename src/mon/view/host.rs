//! Host panel — CPU heatmap + RAM/swap gauges + 1 m load-average.
//!
//! Reads [`crate::mon::state::State`] for the latest [`SystemSnapshot`]
//! (per-core CPU util, mem totals) and the ring history for the CPU
//! sparkline. Empty inputs render the chrome without a value — the
//! aggregator-down banner is the operator-visible signal.

use iced::{Point, Rectangle, Size};
use sy_core::mon::snapshot::SystemSnapshot;

use super::super::state::State;
use super::super::theme::Palette;
use super::super::widgets::heatmap::Heatmap;
use super::super::widgets::Recorder;

/// Inputs the host panel paints. Built by [`panel_data`]; consumed by
/// [`draw_into`]. The split lets tests assert on the projection without
/// instantiating an iced canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct HostPanelData {
    /// Per-core utilisation, scaled to `[0.0, 1.0]` for the heatmap.
    pub cpu_norm: Vec<f32>,
    /// Memory used as a fraction of total — `[0.0, 1.0]`.
    pub mem_used_frac: f32,
    /// Swap used in MiB — rendered as a label below the mem gauge.
    pub swap_used_mib: u64,
    /// 1 m load average (the first of `cpu.load_avg`).
    pub load_avg_1m: f32,
}

/// Project [`State`] into the host-panel inputs.
pub fn panel_data(state: &State) -> HostPanelData {
    let snap = state.latest.as_ref().cloned().unwrap_or_default();
    panel_data_from(&snap)
}

fn panel_data_from(snap: &SystemSnapshot) -> HostPanelData {
    let cpu_norm: Vec<f32> = snap
        .cpu
        .per_core_util_pct
        .iter()
        .map(|p| (p / 100.0).clamp(0.0, 1.0))
        .collect();
    let mem_used_frac = if snap.mem.total_mib == 0 {
        0.0
    } else {
        (snap.mem.used_mib as f32 / snap.mem.total_mib as f32).clamp(0.0, 1.0)
    };
    HostPanelData {
        cpu_norm,
        mem_used_frac,
        swap_used_mib: snap.mem.swap_used_mib,
        load_avg_1m: snap.cpu.load_avg[0],
    }
}

/// Render the host panel into `recorder` within `area`. Heatmap on the
/// left, mem/swap text on the right.
pub fn draw_into(state: &State, palette: &Palette, area: Rectangle, recorder: &mut dyn Recorder) {
    let data = panel_data(state);
    // Tile chrome — 1 px ink border per SPEC §4 D-AESTHETIC.
    recorder.stroke_rect(
        Point::new(area.x, area.y),
        Size::new(area.width, area.height),
        palette.ink,
        1.0,
    );
    recorder.text(
        "host",
        Point::new(area.x + 6.0, area.y + 6.0),
        12.0,
        palette.ink,
    );
    // Left half: per-core heatmap.
    let heatmap_area = Rectangle {
        x: area.x + 8.0,
        y: area.y + 22.0,
        width: area.width * 0.55 - 12.0,
        height: area.height - 30.0,
    };
    Heatmap::new(&data.cpu_norm, palette.bg2, palette.accent).draw_into(recorder, heatmap_area);
    // Right half: RAM / swap / load numbers.
    let right_x = area.x + area.width * 0.55 + 4.0;
    recorder.text(
        &format!("RAM {:.0}%", data.mem_used_frac * 100.0),
        Point::new(right_x, area.y + 28.0),
        12.0,
        palette.ink,
    );
    recorder.text(
        &format!("swap {} MiB", data.swap_used_mib),
        Point::new(right_x, area.y + 44.0),
        12.0,
        palette.ink,
    );
    recorder.text(
        &format!("load {:.2}", data.load_avg_1m),
        Point::new(right_x, area.y + 60.0),
        12.0,
        palette.ink,
    );
    // Tiny mem fill ribbon — a single fill_rect so the gauge is
    // visible without monopolising the panel.
    let ribbon_w = (area.width - (right_x - area.x) - 8.0).max(0.0);
    if ribbon_w > 0.0 {
        let fill_w = ribbon_w * data.mem_used_frac;
        recorder.fill_rect(
            Point::new(right_x, area.y + 78.0),
            Size::new(fill_w, 6.0),
            palette.accent,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;
    use sy_core::mon::snapshot::{CpuPanel, MemPanel};

    fn state_with_cpu(cpu_pct: Vec<f32>) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        let mut state = State::new(ring);
        let snap = SystemSnapshot {
            cpu: CpuPanel {
                per_core_util_pct: cpu_pct,
                freq_mhz: Vec::new(),
                temp_c: 0.0,
                load_avg: [0.42, 0.0, 0.0],
            },
            mem: MemPanel {
                total_mib: 1024,
                used_mib: 512,
                swap_used_mib: 0,
            },
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

    /// SPEC test: 16-core sample → 16 heatmap cells (one `fill_rect`
    /// per cell). Pins the panel's `Heatmap` widget consumption.
    #[test]
    fn cpu_panel_uses_heatmap_widget() {
        let state = state_with_cpu(vec![25.0_f32; 16]);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        // Heatmap cells are the only fill_rect ops whose height
        // exceeds 20 px (the mem ribbon is 6 px tall). Filter on that
        // to count cells deterministically — 16 cores → 16 cells per
        // `Heatmap::cell_count_matches_cores`.
        let heatmap_cells = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::FillRect { size, .. } if size.height > 20.0))
            .count();
        assert_eq!(
            heatmap_cells, 16,
            "16-core sample must emit exactly 16 heatmap fill_rect ops, got {heatmap_cells}"
        );
    }

    #[test]
    fn panel_data_normalises_cpu() {
        let state = state_with_cpu(vec![50.0, 100.0, 0.0]);
        let d = panel_data(&state);
        assert_eq!(d.cpu_norm, vec![0.5, 1.0, 0.0]);
        assert!((d.mem_used_frac - 0.5).abs() < 1e-5);
        assert!((d.load_avg_1m - 0.42).abs() < 1e-5);
    }

    #[test]
    fn panel_data_handles_missing_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        let state = State::new(ring);
        let d = panel_data(&state);
        assert!(d.cpu_norm.is_empty());
        assert_eq!(d.mem_used_frac, 0.0);
    }
}
