//! Gauge — single-value arc with a label.
//!
//! Domain: `value: f32 ∈ [0.0, 1.0]` (callers normalise; clamped
//! defensively). Visual: a stroked arc sweeping from `π` (9 o'clock,
//! west) clockwise to `π + 2π × value`. A half-full gauge (`value =
//! 0.5`) sweeps exactly `π` radians, ending at `2π` — which `tests::
//! arc_sweeps_proportional` asserts on (end_angle - start_angle == π
//! for value = 0.5).
//!
//! NaN / negative / >1 inputs are clamped to `[0.0, 1.0]`.

use std::f32::consts::PI;

use iced::{Color, Point, Rectangle};

use super::Recorder;

pub struct Gauge<'a> {
    pub value: f32,
    pub label: &'a str,
    pub stroke: Color,
    pub ink: Color,
}

impl<'a> Gauge<'a> {
    pub fn new(value: f32, label: &'a str, stroke: Color, ink: Color) -> Self {
        Self {
            value,
            label,
            stroke,
            ink,
        }
    }

    /// Clamp `value` to `[0.0, 1.0]`, NaN → 0.
    fn clamped(&self) -> f32 {
        if self.value.is_nan() {
            0.0
        } else {
            self.value.clamp(0.0, 1.0)
        }
    }

    pub fn start_angle(&self) -> f32 {
        // 9 o'clock — full-sweep gauge ends back at 9 o'clock after 2π.
        PI
    }

    pub fn end_angle(&self) -> f32 {
        self.start_angle() + 2.0 * PI * self.clamped()
    }

    pub fn draw_into(&self, recorder: &mut dyn Recorder, bounds: Rectangle) {
        let center = Point::new(
            bounds.x + bounds.width * 0.5,
            bounds.y + bounds.height * 0.5,
        );
        let radius = (bounds.width.min(bounds.height) * 0.5) - 4.0;
        if radius <= 0.0 {
            return;
        }
        // Background track — full half-circle in a dim stroke so the
        // gauge stays visible at value=0 instead of disappearing.
        let track = iced::Color {
            r: self.ink.r,
            g: self.ink.g,
            b: self.ink.b,
            a: 0.25,
        };
        recorder.arc(
            center,
            radius,
            self.start_angle(),
            self.start_angle() + std::f32::consts::PI * 2.0,
            track,
            1.5,
        );
        recorder.arc(
            center,
            radius,
            self.start_angle(),
            self.end_angle(),
            self.stroke,
            3.0,
        );
        // Label: percentage rendered at the centre.
        let pct = (self.clamped() * 100.0).round() as i32;
        let text = if self.label.is_empty() {
            format!("{pct}%")
        } else {
            format!("{pct}%\n{}", self.label)
        };
        recorder.text(&text, center, 12.0, self.ink);
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
            width: 80.0,
            height: 80.0,
        }
    }

    /// SPEC test: 50 % gauge ⇒ arc end-angle = π (relative).
    #[test]
    fn arc_sweeps_proportional() {
        let g = Gauge::new(0.5, "cpu", Color::BLACK, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        g.draw_into(&mut rec, bounds());

        let arc = rec
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Arc {
                    start_angle,
                    end_angle,
                    width,
                    ..
                } if *width >= 3.0 => Some((*start_angle, *end_angle)),
                _ => None,
            })
            .expect("gauge records an arc");
        let sweep = arc.1 - arc.0;
        assert!(
            (sweep - PI).abs() < 1e-5,
            "50% gauge sweeps π radians; got {sweep}"
        );
    }

    #[test]
    fn full_sweep_at_value_one() {
        let g = Gauge::new(1.0, "", Color::BLACK, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        g.draw_into(&mut rec, bounds());
        let arc = rec
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Arc {
                    start_angle,
                    end_angle,
                    width,
                    ..
                } if *width >= 3.0 => Some(*end_angle - *start_angle),
                _ => None,
            })
            .unwrap();
        assert!((arc - 2.0 * PI).abs() < 1e-5);
    }

    #[test]
    fn nan_value_clamps_to_zero_sweep() {
        let g = Gauge::new(f32::NAN, "", Color::BLACK, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        g.draw_into(&mut rec, bounds());
        let arc = rec
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Arc {
                    start_angle,
                    end_angle,
                    width,
                    ..
                } if *width >= 3.0 => Some(*end_angle - *start_angle),
                _ => None,
            })
            .unwrap();
        assert!(arc.abs() < 1e-5, "NaN → 0 sweep");
    }

    #[test]
    fn out_of_range_clamps() {
        let g = Gauge::new(2.5, "", Color::BLACK, Color::BLACK);
        let mut rec = crate::mon::widgets::MockRecorder::new();
        g.draw_into(&mut rec, bounds());
        let arc = rec
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Arc {
                    start_angle,
                    end_angle,
                    width,
                    ..
                } if *width >= 3.0 => Some(*end_angle - *start_angle),
                _ => None,
            })
            .unwrap();
        assert!(
            (arc - 2.0 * PI).abs() < 1e-5,
            "value > 1 clamps to full sweep"
        );
    }
}
