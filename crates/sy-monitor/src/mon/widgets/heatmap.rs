//! Heatmap — per-core CPU utilisation grid.
//!
//! Layout: `cols = ceil(sqrt(n))`, `rows = ceil(n / cols)`. So a
//! 16-core sample renders a 4×4 grid; 12 cores → 4×3; 24 → 5×5 with
//! the last row partially filled. Each cell is one `fill_rect` call
//! tinted from `cool` → `warm` proportional to the cell value
//! (clamped to `[0.0, 1.0]`).
//!
//! `tests::cell_count_matches_cores` pins the SPEC contract: 16-core
//! sample → 16 `fill_rect` ops.

use iced::{Color, Point, Rectangle, Size};

use super::Recorder;

pub struct Heatmap<'a> {
    pub data: &'a [f32],
    pub cool: Color,
    pub warm: Color,
}

impl<'a> Heatmap<'a> {
    pub fn new(data: &'a [f32], cool: Color, warm: Color) -> Self {
        Self { data, cool, warm }
    }

    fn columns(&self) -> usize {
        let n = self.data.len();
        if n == 0 {
            return 0;
        }
        (n as f32).sqrt().ceil() as usize
    }

    fn rows(&self) -> usize {
        let n = self.data.len();
        let cols = self.columns();
        if cols == 0 {
            return 0;
        }
        n.div_ceil(cols)
    }

    pub fn draw_into(&self, recorder: &mut dyn Recorder, bounds: Rectangle) {
        let n = self.data.len();
        if n == 0 {
            return;
        }
        let cols = self.columns();
        let rows = self.rows();
        let pad = 1.0_f32;
        let cell_w = (bounds.width / cols as f32) - pad;
        let cell_h = (bounds.height / rows as f32) - pad;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }
        for (i, &v) in self.data.iter().enumerate() {
            let r = i / cols;
            let c = i % cols;
            let x = bounds.x + c as f32 * (cell_w + pad);
            let y = bounds.y + r as f32 * (cell_h + pad);
            let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
            let fill = lerp_color(self.cool, self.warm, v);
            recorder.fill_rect(Point::new(x, y), Size::new(cell_w, cell_h), fill);
        }
    }
}

fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
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
            height: 200.0,
        }
    }

    /// SPEC test: 16-core sample → 16 cells.
    #[test]
    fn cell_count_matches_cores() {
        let data = vec![0.3_f32; 16];
        let h = Heatmap::new(&data, Color::WHITE, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        h.draw_into(&mut rec, bounds());

        let cells = rec.count(|op| matches!(op, Op::FillRect { .. }));
        assert_eq!(cells, 16, "16 cores → 16 cells");
    }

    /// 16 cores → 4×4 (square root case).
    #[test]
    fn sixteen_cores_lay_out_four_by_four() {
        let data = vec![0.0_f32; 16];
        let h = Heatmap::new(&data, Color::WHITE, Color::BLACK);
        assert_eq!(h.columns(), 4);
        assert_eq!(h.rows(), 4);
    }

    /// 12 cores → 4×3.
    #[test]
    fn twelve_cores_lay_out_four_by_three() {
        let data = vec![0.0_f32; 12];
        let h = Heatmap::new(&data, Color::WHITE, Color::BLACK);
        assert_eq!(h.columns(), 4);
        assert_eq!(h.rows(), 3);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        h.draw_into(&mut rec, bounds());
        assert_eq!(rec.count(|op| matches!(op, Op::FillRect { .. })), 12);
    }

    /// 24 cores → 5×5 (last row partially filled).
    #[test]
    fn twenty_four_cores_lay_out_five_by_five() {
        let data = vec![0.0_f32; 24];
        let h = Heatmap::new(&data, Color::WHITE, Color::BLACK);
        assert_eq!(h.columns(), 5);
        assert_eq!(h.rows(), 5);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        h.draw_into(&mut rec, bounds());
        assert_eq!(rec.count(|op| matches!(op, Op::FillRect { .. })), 24);
    }

    #[test]
    fn empty_renders_nothing() {
        let h = Heatmap::new(&[], Color::WHITE, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        h.draw_into(&mut rec, bounds());
        assert!(rec.ops.is_empty());
    }

    #[test]
    fn cool_to_warm_lerp_at_value_one() {
        let data = [1.0_f32];
        let h = Heatmap::new(&data, Color::WHITE, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        h.draw_into(&mut rec, bounds());
        match &rec.ops[0] {
            Op::FillRect { fill, .. } => assert_eq!(*fill, Color::BLACK),
            other => panic!("expected fill_rect, got {other:?}"),
        }
    }
}
