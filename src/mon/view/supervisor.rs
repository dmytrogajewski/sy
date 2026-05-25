//! Supervisor panel — plane-state grid + restart counters.
//!
//! Each plane renders as a coloured cell in a flow-grid (per the
//! original Step 17 spec — "colored rectangles in a grid, one per
//! plane"). Cell colour comes from the plane's state string:
//!
//! | state         | cell colour      |
//! |---------------|------------------|
//! | `"active"`    | `palette.ok`     |
//! | `"restarting"`| `palette.warn`   |
//! | `"failed"`    | `palette.bad`    |
//! | anything else | `palette.bg2`    |
//!
//! Restart count + plane name render as small inset text. The mapping
//! is centralised in [`status_color`] so the SPEC test
//! [`tests::red_dot_for_failed_plane`] can pin it without inspecting
//! the recorder.

use iced::{Color, Point, Rectangle, Size};
use sy_core::mon::snapshot::{PlanePanel, SystemSnapshot};

use super::super::state::State;
use super::super::theme::Palette;
use super::super::widgets::Recorder;

#[derive(Debug, Clone, PartialEq)]
pub struct SupervisorViewData {
    pub planes: Vec<PlanePanel>,
}

pub fn panel_data(state: &State) -> SupervisorViewData {
    let snap = state.latest.as_ref().cloned().unwrap_or_default();
    panel_data_from(&snap)
}

fn panel_data_from(snap: &SystemSnapshot) -> SupervisorViewData {
    SupervisorViewData {
        planes: snap.supervisor.planes.clone(),
    }
}

/// State string → cell colour. Centralised so the SPEC test can pin
/// "failed → bad" without inspecting the recorder.
pub fn status_color(palette: &Palette, state: &str) -> Color {
    match state {
        "active" | "ready" => palette.ok,
        "restarting" | "degraded" => palette.warn,
        "failed" | "dead" => palette.bad,
        _ => palette.bg2,
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
        "supervisor",
        Point::new(area.x + 6.0, area.y + 6.0),
        12.0,
        palette.ink,
    );
    if data.planes.is_empty() {
        recorder.text(
            "(no planes reported)",
            Point::new(area.x + 8.0, area.y + 28.0),
            11.0,
            palette.ink,
        );
        return;
    }
    let grid_top = area.y + 24.0;
    let grid_w = area.width - 16.0;
    let grid_h = (area.height - 32.0).max(20.0);
    let n = data.planes.len();
    let cols = (n as f32).sqrt().ceil().max(1.0) as usize;
    let rows = n.div_ceil(cols);
    let pad = 4.0_f32;
    let cell_w = (grid_w / cols as f32) - pad;
    let cell_h = (grid_h / rows as f32) - pad;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }
    for (i, plane) in data.planes.iter().enumerate() {
        let r = i / cols;
        let c = i % cols;
        let x = area.x + 8.0 + c as f32 * (cell_w + pad);
        let y = grid_top + r as f32 * (cell_h + pad);
        let color = status_color(palette, &plane.state);
        recorder.fill_rect(Point::new(x, y), Size::new(cell_w, cell_h), color);
        // Plane name + restart count inset. Use ink colour for contrast;
        // ok/warn/bad slots are dark enough that black ink reads fine.
        recorder.text(&plane.name, Point::new(x + 4.0, y + 4.0), 10.0, palette.ink);
        recorder.text(
            &plane.state,
            Point::new(x + 4.0, y + cell_h - 22.0),
            9.0,
            palette.ink,
        );
        recorder.text(
            &format!("r {}", plane.restarts),
            Point::new(x + 4.0, y + cell_h - 12.0),
            9.0,
            palette.ink,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;
    use sy_core::mon::snapshot::SupervisorPanel;

    fn state_with(planes: Vec<PlanePanel>) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        let mut state = State::new(ring);
        let snap = SystemSnapshot {
            supervisor: SupervisorPanel { planes },
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

    /// SPEC test: a plane in the `"failed"` state must render its
    /// cell with the palette's `bad` (red) slot.
    #[test]
    fn red_dot_for_failed_plane() {
        let planes = vec![
            PlanePanel {
                name: "aiplane".into(),
                state: "active".into(),
                restarts: 0,
            },
            PlanePanel {
                name: "knowledge".into(),
                state: "failed".into(),
                restarts: 3,
            },
        ];
        let state = state_with(planes);
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        let cell_colors: Vec<Color> = rec
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::FillRect { fill, .. } => Some(*fill),
                _ => None,
            })
            .collect();
        assert_eq!(cell_colors, vec![palette.ok, palette.bad]);
        assert_eq!(status_color(&palette, "failed"), palette.bad);
    }

    #[test]
    fn unknown_state_falls_back_to_bg2() {
        let palette = Palette::ink_fallback();
        assert_eq!(status_color(&palette, "????"), palette.bg2);
    }
}
