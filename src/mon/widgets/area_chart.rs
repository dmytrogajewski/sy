//! Stacked-area chart — N series, each filled with its own colour.
//!
//! Layout: each series is normalised against the maximum stack sum,
//! then drawn as a filled polygon from the previous series's top
//! envelope up to the new cumulative envelope. With `n` series the
//! widget records exactly `n` `fill_polygon` ops — one per series.
//! Empty series or all-empty input records nothing.

use iced::{Color, Point, Rectangle};

use super::Recorder;

pub struct AreaChart<'a> {
    pub series: &'a [&'a [f32]],
    pub colors: &'a [Color],
}

impl<'a> AreaChart<'a> {
    pub fn new(series: &'a [&'a [f32]], colors: &'a [Color]) -> Self {
        Self { series, colors }
    }

    pub fn draw_into(&self, recorder: &mut dyn Recorder, bounds: Rectangle) {
        if self.series.is_empty() || self.colors.is_empty() {
            return;
        }
        // Width drives the X step; every series must have the same
        // length to stack — pick the shortest as the safe upper bound.
        let n = self.series.iter().map(|s| s.len()).min().unwrap_or(0);
        if n < 2 {
            return;
        }
        let mut cumulative = vec![0.0_f32; n];
        // First pass — compute peak so the chart fills the bounds
        // height regardless of magnitude.
        for series in self.series {
            for (i, &v) in series.iter().take(n).enumerate() {
                let v = if v.is_nan() { 0.0 } else { v.max(0.0) };
                cumulative[i] += v;
            }
        }
        let peak = cumulative.iter().cloned().fold(f32::EPSILON, f32::max);
        // Second pass — emit one filled polygon per series, on top of
        // the running envelope.
        let mut envelope = vec![0.0_f32; n];
        let step = bounds.width / (n - 1) as f32;
        for (idx, series) in self.series.iter().enumerate() {
            let color = self.colors[idx % self.colors.len()];
            let mut top = vec![0.0_f32; n];
            for (i, &v) in series.iter().take(n).enumerate() {
                let v = if v.is_nan() { 0.0 } else { v.max(0.0) };
                top[i] = envelope[i] + v;
            }
            // Polygon: bottom envelope left→right, top envelope right→left.
            let mut pts = Vec::with_capacity(n * 2);
            for (i, &e) in envelope.iter().enumerate().take(n) {
                let x = bounds.x + step * i as f32;
                let y = bounds.y + bounds.height * (1.0 - e / peak);
                pts.push(Point::new(x, y));
            }
            for (i, &t) in top.iter().enumerate().take(n).rev() {
                let x = bounds.x + step * i as f32;
                let y = bounds.y + bounds.height * (1.0 - t / peak);
                pts.push(Point::new(x, y));
            }
            recorder.fill_polygon(&pts, color);
            envelope = top;
        }
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
            height: 40.0,
        }
    }

    #[test]
    fn n_series_renders_n_fills() {
        let s0 = [1.0_f32, 2.0, 1.5, 3.0];
        let s1 = [0.5_f32, 0.5, 0.5, 0.5];
        let s2 = [2.0_f32, 1.0, 0.5, 1.0];
        let series: [&[f32]; 3] = [&s0, &s1, &s2];
        let colors = [Color::WHITE, Color::BLACK, Color::WHITE];
        let chart = AreaChart::new(&series, &colors);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        chart.draw_into(&mut rec, bounds());
        let fills = rec.count(|op| matches!(op, Op::FillPolygon { .. }));
        assert_eq!(fills, 3, "3 series → 3 filled polygons");
    }

    #[test]
    fn empty_series_records_nothing() {
        let series: [&[f32]; 0] = [];
        let colors = [Color::BLACK];
        let chart = AreaChart::new(&series, &colors);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        chart.draw_into(&mut rec, bounds());
        assert!(rec.ops.is_empty());
    }

    #[test]
    fn single_sample_per_series_records_nothing() {
        let s = [3.0_f32];
        let series: [&[f32]; 1] = [&s];
        let colors = [Color::BLACK];
        let chart = AreaChart::new(&series, &colors);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        chart.draw_into(&mut rec, bounds());
        // 1 sample → no polygon possible.
        assert!(rec.ops.is_empty());
    }

    #[test]
    fn polygon_has_two_n_points() {
        let s = [1.0_f32, 2.0, 1.0];
        let series: [&[f32]; 1] = [&s];
        let colors = [Color::BLACK];
        let chart = AreaChart::new(&series, &colors);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        chart.draw_into(&mut rec, bounds());
        match &rec.ops[0] {
            Op::FillPolygon { n_points, .. } => {
                assert_eq!(*n_points, 6, "3 samples → polygon with 2*3=6 points");
            }
            other => panic!("expected fill_polygon, got {other:?}"),
        }
    }
}
