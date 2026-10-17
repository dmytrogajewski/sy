//! `sy mon` reusable Canvas widgets.
//!
//! Roadmap: `specs/roadmaps/sy-mon/ROADMAP.md` Step 15.
//! SPEC: `specs/research/sy-mon/SPEC.md` §3 SCOPE §4 (panel views),
//! §4 D-CHART (no `plotters-iced`), §4 D-AESTHETIC (1 px tile border),
//! §6 risk "Canvas test seam".
//!
//! ## The `Recorder` test seam
//!
//! iced 0.14's `canvas::Frame` is wgpu-backed — there's no public API
//! to introspect what was drawn. SPEC §6 spelled out the mitigation:
//! "mock through a `Recorder` trait local to the widget module".
//!
//! Every widget's `draw_into(&mut dyn Recorder, …)` method routes
//! through `Recorder` instead of writing to a `Frame` directly. The
//! production path uses [`FrameRecorder`] — a one-method shim that
//! forwards each call to a real `iced::widget::canvas::Frame`. Tests
//! use [`MockRecorder`], which captures every call into a `Vec<Op>`
//! the assertion can pattern-match on.
//!
//! This keeps the widgets headlessly testable on a CI box with no
//! wgpu adapter, while the production renderer still goes through
//! iced's normal wgpu/tiny-skia pipeline.

use iced::{Color, Point, Size};

pub mod area_chart;
pub mod frame_recorder;
pub mod gauge;
pub mod header;
pub mod heatmap;
pub mod probe;
pub mod sparkline;
pub mod tile;

pub use frame_recorder::FrameRecorder;

/// Minimum drawing surface every widget routes through. Implementors
/// either forward to a real `iced::widget::canvas::Frame` (production
/// path — landed alongside the popup view in Step 16/17) or capture
/// calls into a `Vec` (test path — [`MockRecorder`]). Keep this
/// surface as small as the widgets need — adding a primitive without
/// a test consumer is dead-code surface.
pub trait Recorder {
    /// Move the pen / start a new sub-path at `p`.
    fn move_to(&mut self, p: Point);
    /// Draw a stroked line segment from the current pen position to `p`,
    /// updating the pen position.
    fn line_to(&mut self, p: Point, stroke: Color);
    /// Fill a closed polygon (`pts` listed in order; implicit close
    /// from the last point back to the first).
    fn fill_polygon(&mut self, pts: &[Point], fill: Color);
    /// Fill an axis-aligned rectangle.
    fn fill_rect(&mut self, top_left: Point, size: Size, fill: Color);
    /// Stroke the outline of an axis-aligned rectangle.
    fn stroke_rect(&mut self, top_left: Point, size: Size, stroke: Color, width: f32);
    /// Stroke a circular arc centred at `center` of radius `radius`,
    /// sweeping from `start_angle` to `end_angle` (radians, clockwise
    /// convention matching iced 0.14's `Path::arc`).
    fn arc(
        &mut self,
        center: Point,
        radius: f32,
        start_angle: f32,
        end_angle: f32,
        stroke: Color,
        width: f32,
    );
    /// Draw a single line of text at the given position.
    fn text(&mut self, content: &str, position: Point, size: f32, color: Color);
}

/// Single captured drawing call — what [`MockRecorder`] stores.
///
/// The variants intentionally don't carry every `Color`/`f32` field of
/// the trait — only the fields the spec tests assert on. Adding a
/// field is cheap if a future test needs it; leaving them off keeps
/// fixtures terse.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    MoveTo(Point),
    LineTo {
        to: Point,
        stroke: Color,
    },
    FillPolygon {
        n_points: usize,
        fill: Color,
    },
    FillRect {
        top_left: Point,
        size: Size,
        fill: Color,
    },
    StrokeRect {
        top_left: Point,
        size: Size,
        stroke: Color,
        width: f32,
    },
    Arc {
        center: Point,
        radius: f32,
        start_angle: f32,
        end_angle: f32,
        stroke: Color,
        width: f32,
    },
    Text {
        content: String,
        position: Point,
        size: f32,
        color: Color,
    },
}

/// Test-only `Recorder` that captures every call. The widget tests
/// assert on `recorder.ops` (filter / count / pattern-match).
#[derive(Debug, Default)]
pub struct MockRecorder {
    pub ops: Vec<Op>,
}

impl MockRecorder {
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    /// Count ops matching `pred` — terse helper for tests. Gated to
    /// `#[cfg(test)]` because production callers (Step 16's view tree)
    /// iterate over `ops` directly; only the per-widget assertion code
    /// reaches for the predicate counter today.
    #[cfg(test)]
    pub fn count<F: Fn(&Op) -> bool>(&self, pred: F) -> usize {
        self.ops.iter().filter(|o| pred(o)).count()
    }
}

impl Recorder for MockRecorder {
    fn move_to(&mut self, p: Point) {
        self.ops.push(Op::MoveTo(p));
    }
    fn line_to(&mut self, p: Point, stroke: Color) {
        self.ops.push(Op::LineTo { to: p, stroke });
    }
    fn fill_polygon(&mut self, pts: &[Point], fill: Color) {
        self.ops.push(Op::FillPolygon {
            n_points: pts.len(),
            fill,
        });
    }
    fn fill_rect(&mut self, top_left: Point, size: Size, fill: Color) {
        self.ops.push(Op::FillRect {
            top_left,
            size,
            fill,
        });
    }
    fn stroke_rect(&mut self, top_left: Point, size: Size, stroke: Color, width: f32) {
        self.ops.push(Op::StrokeRect {
            top_left,
            size,
            stroke,
            width,
        });
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
        self.ops.push(Op::Arc {
            center,
            radius,
            start_angle,
            end_angle,
            stroke,
            width,
        });
    }
    fn text(&mut self, content: &str, position: Point, size: f32, color: Color) {
        self.ops.push(Op::Text {
            content: content.to_string(),
            position,
            size,
            color,
        });
    }
}

// Production-path `Recorder` lives in `frame_recorder.rs` —
// [`FrameRecorder`] forwards every primitive onto an
// `iced::widget::canvas::Frame`. Step 17's `view::root` instantiates
// one per `Canvas::draw` call and hands it to every panel's
// `draw_into`.
