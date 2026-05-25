//! Agents panel — RSS gauge (fraction of system memory), policy-denial
//! sparkline (synth flat-line until a per-tick column lands), and a
//! small running-count label.

use iced::{Point, Rectangle, Size};
use sy_core::mon::snapshot::{AgentsPanel, SystemSnapshot};

use super::super::state::State;
use super::super::theme::Palette;
use super::super::widgets::gauge::Gauge;
use super::super::widgets::sparkline::Sparkline;
use super::super::widgets::Recorder;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentsViewData {
    pub running: u32,
    pub rss_total_mib: u64,
    pub policy_denials_recent: u32,
    /// System mem total in MiB — denominator for the RSS gauge.
    /// `0` falls back to a saturation point of 1 so the gauge value
    /// stays defined.
    pub mem_total_mib: u64,
}

pub fn panel_data(state: &State) -> AgentsViewData {
    let snap = state.latest.as_ref().cloned().unwrap_or_default();
    panel_data_from(&snap)
}

fn panel_data_from(snap: &SystemSnapshot) -> AgentsViewData {
    let AgentsPanel {
        running,
        rss_total_mib,
        policy_denials_recent,
    } = snap.agents;
    AgentsViewData {
        running,
        rss_total_mib,
        policy_denials_recent,
        mem_total_mib: snap.mem.total_mib,
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
        "agents",
        Point::new(area.x + 6.0, area.y + 6.0),
        12.0,
        palette.ink,
    );
    // Running count caption.
    recorder.text(
        &format!("running: {}", data.running),
        Point::new(area.x + 8.0, area.y + 22.0),
        11.0,
        palette.ink,
    );
    // RSS gauge — fraction of system memory.
    let rss_norm = if data.mem_total_mib == 0 {
        0.0
    } else {
        (data.rss_total_mib as f32 / data.mem_total_mib as f32).clamp(0.0, 1.0)
    };
    let gauge_w = (area.width - 16.0) * 0.5;
    let chart_top = area.y + 38.0;
    let chart_h = (area.height - 50.0).max(40.0);
    Gauge::new(rss_norm, "rss", palette.accent, palette.ink).draw_into(
        recorder,
        Rectangle {
            x: area.x + 8.0,
            y: chart_top,
            width: gauge_w,
            height: chart_h,
        },
    );
    // Policy-denial sparkline. Warn-tint stroke when nonzero so the
    // line itself carries the alert signal.
    let denial_color = if data.policy_denials_recent > 0 {
        palette.warn
    } else {
        palette.ok
    };
    let denial_series = synth_series(data.policy_denials_recent as f32);
    Sparkline::new(&denial_series, denial_color).draw_into(
        recorder,
        Rectangle {
            x: area.x + 16.0 + gauge_w,
            y: chart_top,
            width: gauge_w - 8.0,
            height: chart_h,
        },
    );
    // Inline label so the operator sees the actual count even at zero
    // (a flat sparkline reads identically at 0 or 100).
    recorder.text(
        &format!("denials: {}", data.policy_denials_recent),
        Point::new(area.x + 16.0 + gauge_w, area.y + area.height - 12.0),
        10.0,
        denial_color,
    );
}

fn synth_series(v: f32) -> [f32; 8] {
    // Sparkline ranges over min/max — a flat input renders as a midline.
    // Inject a small variation so a nonzero series visibly differs from
    // the zero baseline.
    if v == 0.0 {
        [0.0; 8]
    } else {
        [0.0, v * 0.5, v, v * 0.8, v, v * 0.9, v, v]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::widgets::{MockRecorder, Op};
    use sy_core::mon::ring::Ring;
    use sy_core::mon::snapshot::MemPanel;

    fn state_with(panel: AgentsPanel) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        let mut state = State::new(ring);
        let snap = SystemSnapshot {
            agents: panel,
            mem: MemPanel {
                total_mib: 1024,
                used_mib: 0,
                swap_used_mib: 0,
            },
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

    /// One gauge (RSS) → one arc.
    #[test]
    fn renders_one_gauge() {
        let state = state_with(AgentsPanel {
            running: 2,
            rss_total_mib: 256,
            policy_denials_recent: 0,
        });
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        let arcs = rec
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Arc { width, .. } if *width >= 3.0))
            .count();
        assert_eq!(arcs, 1);
    }

    /// Nonzero policy denials tint the sparkline stroke + denial label
    /// with the warn slot.
    #[test]
    fn denials_tint_warn_when_nonzero() {
        let state = state_with(AgentsPanel {
            running: 2,
            rss_total_mib: 256,
            policy_denials_recent: 3,
        });
        let palette = Palette::ink_fallback();
        let mut rec = MockRecorder::new();
        draw_into(&state, &palette, bounds(), &mut rec);
        let stroke_warn = rec.ops.iter().any(|op| {
            matches!(op,
            Op::LineTo { stroke, .. } if *stroke == palette.warn)
        });
        assert!(stroke_warn, "denial sparkline must use the warn slot");
    }
}
