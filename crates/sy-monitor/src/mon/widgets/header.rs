//! Header — panel title strip with a Nerd Font glyph.
//!
//! Renders two text ops: the glyph at the left edge, the title text a
//! few pixels to the right of it. The glyph is expected to be a Nerd
//! Font code point ("nf-fa-microchip", etc.); this widget doesn't own
//! the lookup — callers pass the rendered codepoint string.

use iced::{Color, Point, Rectangle};

use super::Recorder;

pub struct Header<'a> {
    pub title: &'a str,
    pub glyph: &'a str,
    pub ink: Color,
}

impl<'a> Header<'a> {
    pub fn new(title: &'a str, glyph: &'a str, ink: Color) -> Self {
        Self { title, glyph, ink }
    }

    pub fn draw_into(&self, recorder: &mut dyn Recorder, bounds: Rectangle) {
        let glyph_size = 14.0;
        let title_size = 12.0;
        let y = bounds.y + (bounds.height - title_size) * 0.5;
        recorder.text(
            self.glyph,
            Point::new(bounds.x + 4.0, y),
            glyph_size,
            self.ink,
        );
        // 4 px padding + glyph cell width (~16 px for typical Nerd Font glyphs).
        recorder.text(
            self.title,
            Point::new(bounds.x + 24.0, y),
            title_size,
            self.ink,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::Op;

    fn bounds() -> Rectangle {
        Rectangle {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 18.0,
        }
    }

    #[test]
    fn renders_glyph_and_title() {
        let h = Header::new("CPU", "\u{f2db}", Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        h.draw_into(&mut rec, bounds());
        let texts: Vec<&str> = rec
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Text { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["\u{f2db}", "CPU"]);
    }
}
