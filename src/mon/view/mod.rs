//! `sy mon` popup view tree.
//!
//! Renders nine panels — one per SCOPE §4.4 row. The layout is a
//! 3 × 3 grid:
//!
//! | row | col 0     | col 1   | col 2       |
//! |-----|-----------|---------|-------------|
//! | 0   | host      | accel   | net         |
//! | 1   | disk      | aiplane | knowledge   |
//! | 2   | agents    | power   | supervisor  |
//!
//! Each panel module exposes:
//!
//! - `pub fn panel_data(state) -> XPanelData` — pure projection of
//!   the snapshot + history slice the panel paints. The SPEC tests
//!   call this directly so they don't need a wgpu adapter.
//! - `pub fn draw_into(state, palette, area, &mut dyn Recorder)` —
//!   issues primitive draw calls onto whichever `Recorder` impl is
//!   passed (production: `FrameRecorder` wrapping an iced
//!   `canvas::Frame`; tests: `MockRecorder` capturing into a `Vec`).
//!
//! `view::root` then hosts one `canvas::Canvas` covering the panel
//! area; the `canvas::Program::draw` impl carves the area into nine
//! rectangles and dispatches to each panel's `draw_into`. The header
//! row, banner, and tile chrome stay as native iced widgets so the
//! same `text::*` rendering path used elsewhere in `sy` applies (font
//! / shaping / kerning).

use iced::mouse;
use iced::widget::canvas::{Canvas, Geometry, Program};
use iced::widget::{canvas, column, container, row, text, Space};
use iced::{Background, Border, Element, Length, Padding, Rectangle, Renderer, Theme};

use super::app::Message;
use super::state::{view_data, BannerKind, PanelId, State, ViewData};
use super::theme::Palette;
use super::widgets::{FrameRecorder, Recorder};

pub mod accel;
pub mod agents;
pub mod aiplane;
pub mod disk;
pub mod filter;
pub mod host;
pub mod knowledge;
pub mod net;
pub mod power;
pub mod supervisor;

/// Render the popup. Pulls everything from [`view_data`] so the
/// data-flow contract is exercised in the same call the unit tests
/// pattern-match on.
pub fn root(state: &State) -> Element<'_, Message> {
    let palette = super::theme::load_or_ink();
    let data = view_data(state);
    let mut col = column![].spacing(8).padding(12);

    if let Some(banner) = &data.banner {
        col = col.push(banner_view(&palette, banner.kind, banner.last_seen_at_ms));
    }

    // Step 18: `/` filter overlay. Paints a textbox above the grid
    // showing the current pattern. Closed → `None`, so we only push
    // a row when the overlay is active.
    if let Some(overlay) = filter::overlay(state, &palette) {
        col = col.push(overlay);
    }

    col = col.push(header(&palette, &data));
    col = col.push(grid(state, palette));

    container(col)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_t: &Theme| container::Style {
            background: Some(Background::Color(palette.bg)),
            border: Border::default(),
            ..Default::default()
        })
        .into()
}

fn header<'a>(palette: &Palette, data: &ViewData) -> Element<'a, Message> {
    let timestamp = match data.latest_captured_at_ms {
        Some(ms) => format!("frame @ {ms} ms"),
        None => "(awaiting first frame; painting from ring history)".to_string(),
    };
    let history_label = format!(
        "ring history: {} sample(s)",
        data.cpu_sparkline_recent.len()
    );
    row![
        text("sy mon").size(20).color(palette.ink),
        Space::new().width(Length::Fill).height(1),
        text(timestamp).size(12).color(palette.ink),
        Space::new().width(Length::Fixed(16.0)).height(1),
        text(history_label).size(12).color(palette.ink),
    ]
    .padding(Padding::ZERO)
    .spacing(8)
    .into()
}

/// 3 × 3 panel grid. We host one `Canvas` covering the full grid area
/// and dispatch to each panel's `draw_into` from the canvas program.
/// Hosting one Canvas (instead of nine) keeps the iced widget tree
/// small and routes every panel through the same `FrameRecorder` shim.
fn grid(state: &State, palette: Palette) -> Element<'_, Message> {
    let prog = PanelGrid { state, palette };
    Canvas::new(prog)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Canvas program drawing all nine panels. Borrows `&State` so the
/// snapshot / ring history are read at render time without copying.
struct PanelGrid<'a> {
    state: &'a State,
    palette: Palette,
}

impl<'a> Program<Message> for PanelGrid<'a> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let mut rec = FrameRecorder::new(&mut frame);
        // `canvas::Frame` uses local 0,0-origin coordinates; the
        // `bounds` iced hands us is the canvas's absolute position
        // inside the popup. Passing raw `bounds` makes every panel
        // shift right + down by (bounds.x, bounds.y) and overflow
        // the frame's right + bottom edges — that's the "borders
        // cut off the bottom and right tiles" bug.
        let local = Rectangle {
            x: 0.0,
            y: 0.0,
            width: bounds.width,
            height: bounds.height,
        };
        draw_panels(self.state, &self.palette, local, &mut rec);
        vec![frame.into_geometry()]
    }
}

/// Public so tests + production share one dispatch path. Splits the
/// canvas area into nine rectangles and hands each one to its
/// panel's `draw_into`. Pure projection of `area` → 9 sub-rectangles
/// plus pure dispatch — no iced types touched, so the function is
/// usable under any `Recorder`.
///
/// Step 18: when `state.expanded` is `Some(panel)` the dispatch
/// collapses to that one panel full-screen so the user can read
/// dense rows without the 3×3 layout's row budget.
pub fn draw_panels(state: &State, palette: &Palette, area: Rectangle, recorder: &mut dyn Recorder) {
    if let Some(expanded) = state.expanded {
        draw_panel(expanded, state, palette, area, recorder);
        return;
    }
    let cells = grid_cells(area);
    host::draw_into(state, palette, cells[0][0], recorder);
    accel::draw_into(state, palette, cells[0][1], recorder);
    net::draw_into(state, palette, cells[0][2], recorder);
    disk::draw_into(state, palette, cells[1][0], recorder);
    aiplane::draw_into(state, palette, cells[1][1], recorder);
    knowledge::draw_into(state, palette, cells[1][2], recorder);
    agents::draw_into(state, palette, cells[2][0], recorder);
    power::draw_into(state, palette, cells[2][1], recorder);
    supervisor::draw_into(state, palette, cells[2][2], recorder);
}

/// Dispatch to a single panel's `draw_into`. Used by [`draw_panels`]
/// when [`State::expanded`] is `Some`.
fn draw_panel(
    id: PanelId,
    state: &State,
    palette: &Palette,
    area: Rectangle,
    recorder: &mut dyn Recorder,
) {
    match id {
        PanelId::Host => host::draw_into(state, palette, area, recorder),
        PanelId::Accel => accel::draw_into(state, palette, area, recorder),
        PanelId::Net => net::draw_into(state, palette, area, recorder),
        PanelId::Disk => disk::draw_into(state, palette, area, recorder),
        PanelId::Aiplane => aiplane::draw_into(state, palette, area, recorder),
        PanelId::Knowledge => knowledge::draw_into(state, palette, area, recorder),
        PanelId::Agents => agents::draw_into(state, palette, area, recorder),
        PanelId::Power => power::draw_into(state, palette, area, recorder),
        PanelId::Supervisor => supervisor::draw_into(state, palette, area, recorder),
    }
}

fn grid_cells(area: Rectangle) -> [[Rectangle; 3]; 3] {
    let gap = 6.0_f32;
    let cell_w = ((area.width - gap * 2.0) / 3.0).max(0.0);
    let cell_h = ((area.height - gap * 2.0) / 3.0).max(0.0);
    let mut cells = [[Rectangle {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    }; 3]; 3];
    for (r, row_cells) in cells.iter_mut().enumerate() {
        for (c, cell) in row_cells.iter_mut().enumerate() {
            *cell = Rectangle {
                x: area.x + c as f32 * (cell_w + gap),
                y: area.y + r as f32 * (cell_h + gap),
                width: cell_w,
                height: cell_h,
            };
        }
    }
    cells
}

fn banner_view<'a>(
    palette: &Palette,
    kind: BannerKind,
    last_seen_at_ms: u64,
) -> Element<'a, Message> {
    let msg = match kind {
        BannerKind::AggregatorDown => {
            if last_seen_at_ms == 0 {
                "aggregator unreachable — no data yet. start `sy-mon-collect.service`.".to_string()
            } else {
                format!(
                    "aggregator unreachable — last frame at {last_seen_at_ms} ms (showing cached data)"
                )
            }
        }
    };
    let accent = palette.accent;
    container(text(msg).size(12).color(palette.ink))
        .padding(8)
        .width(Length::Fill)
        .style(move |_t: &Theme| container::Style {
            background: Some(Background::Color(accent)),
            border: Border::default(),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;

    fn empty_state() -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        State::new(ring)
    }

    fn area() -> Rectangle {
        Rectangle {
            x: 0.0,
            y: 0.0,
            width: 1200.0,
            height: 600.0,
        }
    }

    /// `draw_panels` must reach every panel — pin by checking each
    /// panel's title text appears in the recorder's op stream.
    #[test]
    fn draw_panels_dispatches_to_all_nine() {
        let state = empty_state();
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_panels(&state, &palette, area(), &mut rec);
        let titles = [
            "host",
            "accel",
            "net",
            "disk",
            "aiplane",
            "knowledge",
            "agents",
            "power",
            "supervisor",
        ];
        for t in titles {
            let hit = rec.ops.iter().any(|op| match op {
                Op::Text { content, .. } => content == t,
                _ => false,
            });
            assert!(hit, "panel {t:?} must render its title");
        }
    }

    /// Cells must tile the area without gaps overlapping.
    #[test]
    fn grid_cells_carve_into_three_by_three() {
        let cells = grid_cells(area());
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].len(), 3);
        // Bottom-right cell's far edges land within the source area.
        let last = cells[2][2];
        assert!(last.x + last.width <= area().x + area().width + 1.0);
        assert!(last.y + last.height <= area().y + area().height + 1.0);
    }
}
