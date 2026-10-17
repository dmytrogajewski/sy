//! Production `Recorder` shim — forwards every call onto a real
//! `iced::widget::canvas::Frame`.
//!
//! Step 17 wires the per-panel `draw_into(state, palette, area,
//! recorder)` entry points from `view::root`. The view layer needs a
//! `Recorder` impl that issues real draw calls instead of capturing
//! ops into a `Vec`. iced 0.14's `Frame` exposes `fill`, `stroke`,
//! `fill_rectangle`, `stroke_rectangle`, and `fill_text` (`iced_graphics::
//! geometry::Frame`); we map every `Recorder` primitive to the
//! smallest sufficient combination.
//!
//! ## Mapping
//!
//! | `Recorder` op    | `Frame` call                                            |
//! |------------------|---------------------------------------------------------|
//! | `move_to`        | buffer the point, no draw call until `line_to` arrives  |
//! | `line_to`        | `frame.stroke(&Path::line(prev, p), Stroke{..})`        |
//! | `fill_polygon`   | `frame.fill(&Path::new(\|b\| moves + closes), Fill{..})`  |
//! | `fill_rect`      | `frame.fill_rectangle(top_left, size, fill)`            |
//! | `stroke_rect`    | `frame.stroke_rectangle(top_left, size, Stroke{..})`    |
//! | `arc`            | `frame.stroke(&Path::new(\|b\| b.arc(..)), Stroke{..})`   |
//! | `text`           | `frame.fill_text(canvas::Text { .. })`                  |
//!
//! `move_to` carries no draw call on its own — it just updates the
//! pen state. `line_to` consumes the saved pen position to build a
//! one-segment path. This matches `Sparkline::draw_into`'s contract
//! (`move_to` once, then `N-1` `line_to` calls).

use iced::widget::canvas::path::{Arc, Builder};
use iced::widget::canvas::{Frame, Path, Stroke, Text};
use iced::{Color, Point, Radians, Size};

use super::Recorder;

/// One-method shim wrapping a mutable `iced::widget::canvas::Frame`.
///
/// The borrow lifetime is the parent frame's lifetime; callers
/// instantiate one per `Canvas::draw` call and let it drop when the
/// frame is consumed by `into_geometry`.
pub struct FrameRecorder<'a> {
    frame: &'a mut Frame,
    pen: Option<Point>,
}

impl<'a> FrameRecorder<'a> {
    pub fn new(frame: &'a mut Frame) -> Self {
        Self { frame, pen: None }
    }
}

impl<'a> Recorder for FrameRecorder<'a> {
    fn move_to(&mut self, p: Point) {
        self.pen = Some(p);
    }

    fn line_to(&mut self, p: Point, stroke: Color) {
        // If `move_to` was skipped (test paths sometimes call
        // `line_to` first), treat the previous draw target as the
        // origin — same behaviour as `Path::Builder::line_to` without
        // a preceding `move_to`. Production callers (`Sparkline`,
        // future polyline widgets) always pair a `move_to` first.
        let from = self.pen.unwrap_or(p);
        let line = Path::line(from, p);
        self.frame
            .stroke(&line, Stroke::default().with_color(stroke).with_width(1.5));
        self.pen = Some(p);
    }

    fn fill_polygon(&mut self, pts: &[Point], fill: Color) {
        if pts.is_empty() {
            return;
        }
        let path = Path::new(|b: &mut Builder| {
            b.move_to(pts[0]);
            for p in &pts[1..] {
                b.line_to(*p);
            }
            b.close();
        });
        self.frame.fill(&path, fill);
    }

    fn fill_rect(&mut self, top_left: Point, size: Size, fill: Color) {
        self.frame.fill_rectangle(top_left, size, fill);
    }

    fn stroke_rect(&mut self, top_left: Point, size: Size, stroke: Color, width: f32) {
        self.frame.stroke_rectangle(
            top_left,
            size,
            Stroke::default().with_color(stroke).with_width(width),
        );
    }

    fn arc(
        &mut self,
        center: Point,
        radius: f32,
        start_angle: f32,
        end_angle: f32,
        stroke: Color,
        width: f32,
    ) {
        let arc = Arc {
            center,
            radius,
            start_angle: Radians(start_angle),
            end_angle: Radians(end_angle),
        };
        let path = Path::new(|b: &mut Builder| b.arc(arc));
        self.frame.stroke(
            &path,
            Stroke::default().with_color(stroke).with_width(width),
        );
    }

    fn text(&mut self, content: &str, position: Point, size: f32, color: Color) {
        self.frame.fill_text(Text {
            content: content.to_string(),
            position,
            color,
            size: size.into(),
            ..Default::default()
        });
    }
}
