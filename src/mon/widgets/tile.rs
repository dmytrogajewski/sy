//! Tile — outer chrome for each panel.
//!
//! SPEC §4 D-AESTHETIC: 1 px border around panel content. The tile
//! widget is a layout primitive — it fills the background with `bg`
//! and strokes the border with `ink`, then defers content rendering
//! to the panel-level view code (this module doesn't compose children;
//! that lives in `src/mon/view/` once Step 17 lands).

use iced::{Color, Point, Rectangle, Size};

use super::Recorder;

pub struct Tile {
    pub bg: Color,
    pub ink: Color,
}

impl Tile {
    pub fn new(bg: Color, ink: Color) -> Self {
        Self { bg, ink }
    }

    pub fn draw_into(&self, recorder: &mut dyn Recorder, bounds: Rectangle) {
        recorder.fill_rect(
            Point::new(bounds.x, bounds.y),
            Size::new(bounds.width, bounds.height),
            self.bg,
        );
        recorder.stroke_rect(
            Point::new(bounds.x, bounds.y),
            Size::new(bounds.width, bounds.height),
            self.ink,
            1.0,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::Op;

    fn bounds() -> Rectangle {
        Rectangle {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
        }
    }

    /// SPEC D-AESTHETIC: 1 px border. The tile records exactly one
    /// `stroke_rect` (the border) and one `fill_rect` (the body).
    #[test]
    fn renders_one_pixel_border_and_body() {
        let t = Tile::new(Color::WHITE, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        t.draw_into(&mut rec, bounds());

        let strokes: Vec<f32> = rec
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::StrokeRect { width, .. } => Some(*width),
                _ => None,
            })
            .collect();
        assert_eq!(strokes.len(), 1, "exactly one border stroke");
        assert!((strokes[0] - 1.0).abs() < 1e-5, "1 px border");

        let fills = rec.count(|op| matches!(op, Op::FillRect { .. }));
        assert_eq!(fills, 1, "exactly one body fill");
    }
}
