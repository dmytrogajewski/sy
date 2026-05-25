//! Sparkline — single-series polyline scaled to the widget bounds.
//!
//! Constructor: `Sparkline { data: &'a [f32], stroke: Color }`.
//! Range auto-scales to the min/max of `data`; a constant series is
//! drawn as a flat line in the middle of the widget bounds.
//!
//! ## Segment convention
//!
//! For `n` samples the widget records `n` `MoveTo` + `n - 1` `LineTo`
//! ops — i.e. **N points, N-1 segments** (the natural polyline
//! reading). `n <= 1` records zero segments (no line possible). This
//! is what `tests::renders_n_path_segments` asserts.

use iced::{Color, Point, Rectangle};

use super::Recorder;

pub struct Sparkline<'a> {
    pub data: &'a [f32],
    pub stroke: Color,
}

impl<'a> Sparkline<'a> {
    pub fn new(data: &'a [f32], stroke: Color) -> Self {
        Self { data, stroke }
    }

    /// Compute `n - 1` polyline segments and route them through
    /// `recorder`. Production path also records the path as a single
    /// stroked `Path::new` for the wgpu backend — see
    /// [`Sparkline::draw_into_frame`].
    pub fn draw_into(&self, recorder: &mut dyn Recorder, bounds: Rectangle) {
        if self.data.len() < 2 {
            return;
        }
        let pts = self.layout(bounds);
        recorder.move_to(pts[0]);
        for p in &pts[1..] {
            recorder.line_to(*p, self.stroke);
        }
    }

    fn layout(&self, bounds: Rectangle) -> Vec<Point> {
        let n = self.data.len();
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for &v in self.data {
            if v.is_nan() {
                continue;
            }
            if v < lo {
                lo = v;
            }
            if v > hi {
                hi = v;
            }
        }
        if !lo.is_finite() || !hi.is_finite() {
            // All-NaN input — collapse to a flat midline so the widget
            // still renders something.
            lo = 0.0;
            hi = 1.0;
        }
        let span = (hi - lo).max(f32::EPSILON);
        let step = if n > 1 {
            bounds.width / (n - 1) as f32
        } else {
            0.0
        };
        (0..n)
            .map(|i| {
                let v = self.data[i];
                let v = if v.is_nan() { lo } else { v };
                let norm = (v - lo) / span;
                let y = bounds.y + bounds.height * (1.0 - norm);
                Point::new(bounds.x + step * i as f32, y)
            })
            .collect()
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
            width: 100.0,
            height: 20.0,
        }
    }

    /// SPEC test: `Frame` records N segments for N samples.
    /// Convention pinned by this module: `n` samples → `n - 1`
    /// `LineTo` ops (with one leading `MoveTo`).
    #[test]
    fn renders_n_path_segments() {
        let data = [1.0_f32, 2.0, 4.0, 3.0, 5.0]; // n = 5
        let s = Sparkline::new(&data, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        s.draw_into(&mut rec, bounds());

        let line_segments = rec.count(|op| matches!(op, Op::LineTo { .. }));
        assert_eq!(
            line_segments,
            data.len() - 1,
            "N={} samples must produce N-1={} segments",
            data.len(),
            data.len() - 1
        );
        let moves = rec.count(|op| matches!(op, Op::MoveTo(_)));
        assert_eq!(moves, 1, "one initial MoveTo");
    }

    #[test]
    fn empty_data_records_nothing() {
        let s = Sparkline::new(&[], Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        s.draw_into(&mut rec, bounds());
        assert!(rec.ops.is_empty());
    }

    #[test]
    fn single_sample_records_nothing() {
        let s = Sparkline::new(&[7.0_f32], Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        s.draw_into(&mut rec, bounds());
        assert!(rec.ops.is_empty(), "1 sample → no line possible");
    }

    #[test]
    fn constant_series_renders_flat_line() {
        let data = [5.0_f32; 4];
        let s = Sparkline::new(&data, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        s.draw_into(&mut rec, bounds());
        // All Y-coords identical (the EPSILON span guard avoids NaN).
        let ys: Vec<f32> = rec
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::MoveTo(p) | Op::LineTo { to: p, .. } => Some(p.y),
                _ => None,
            })
            .collect();
        let first = ys[0];
        for y in &ys[1..] {
            assert!((y - first).abs() < 0.001, "constant series → flat line");
        }
    }
}
