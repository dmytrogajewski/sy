//! Per-op progress row widget. Roadmap Step 28 (SPEC §3.3 item 5 — op
//! progress) + journey-J6 affordance.
//!
//! Composition: `Row[ verb_label | progress_bar | percent_label |
//! throughput_label ]`. The widget is `gui-iced`-gated because it
//! reaches for `iced::widget::{progress_bar, row, text}`; the pure
//! [`humanise_throughput`] helper exposed here is what the statusbar /
//! waybar pill formatter both read so the binary-prefix unit-table
//! stays single-sourced.
//!
//! The throughput-unit table is `B/s, KiB/s, MiB/s, GiB/s, TiB/s` —
//! the same binary-prefix shape `sy mon`'s net pane uses (see
//! `src/mon/view/net.rs::humanise`), so a user glancing between the
//! file-manager statusbar and the mon popup sees one vocabulary.

use iced::widget::{progress_bar, row, text};
use iced::{Element, Length};

use crate::file::app::Message;
use crate::file::ipc::OpRow;

/// Binary-prefix throughput unit table. Matches `sy mon`'s net pane
/// vocabulary (`src/mon/view/net.rs::humanise`).
const THROUGHPUT_UNITS: [&str; 5] = ["B/s", "KiB/s", "MiB/s", "GiB/s", "TiB/s"];

/// Render a single op's progress row. Pulls `done` + `total` off the
/// daemon's tracker (`OpRow`); the throughput field is derived from
/// the most recent `OpEvent::Progress::throughput_bps` the daemon
/// observed (Step 16's executor emits ≥10 Hz samples). When `total`
/// is `0` (the executor hasn't computed it yet, e.g. a trash op
/// where the size doesn't surface), the progress bar paints
/// indeterminate-style (`0.0..=1.0` with `0.0`).
pub fn progress_row<'a>(op_row: OpRow) -> Element<'a, Message> {
    let pct = if op_row.total > 0 {
        (op_row.done as f32) / (op_row.total as f32)
    } else {
        0.0
    };
    let percent_label = if op_row.total > 0 {
        format!("{:>3.0} %", pct * 100.0)
    } else {
        "  - %".to_owned()
    };
    // Step 28 today: `OpRow` doesn't carry throughput. The widget
    // surfaces the state label as a stand-in so a future iteration
    // that adds `OpRow::throughput_bps` doesn't need to reshape the
    // row composition.
    let trailing = text(op_row.state.clone());
    let bar =
        iced::widget::container(progress_bar(0.0..=1.0, pct.clamp(0.0, 1.0))).width(Length::Fill);
    row![
        text(format!("{}#{}", op_row.kind, op_row.op_id)),
        bar,
        text(percent_label),
        trailing,
    ]
    .spacing(8)
    .padding(4)
    .into()
}

/// Pure helper: format a bytes-per-second value as a human-readable
/// label. Binary-prefix table matches `sy mon`'s net pane so a glance
/// between the two surfaces stays vocabulary-stable.
///
/// Pinned shape:
/// * `0`             → `"0 B/s"` (NB: integer, no decimal — matches
///   the roadmap Step 28 brief's exact-match assertion);
/// * `1024`          → `"1.0 KiB/s"`;
/// * `1024 * 1024`   → `"1.0 MiB/s"`;
/// * `1 GiB`         → `"1.0 GiB/s"`.
pub fn humanise_throughput(bps: u64) -> String {
    if bps == 0 {
        return "0 B/s".to_owned();
    }
    let mut v = bps as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < THROUGHPUT_UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    format!("{:.1} {}", v, THROUGHPUT_UNITS[u])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roadmap Step 28 DoD pin: the throughput formatter matches the
    /// SPEC §3.3 row 5 progress vocabulary. Mirrors `sy mon`'s
    /// binary-prefix shape (`KiB/MiB/GiB`) so the statusbar + waybar
    /// pill speak the same units the popup paints.
    #[test]
    fn throughput_humanised() {
        assert_eq!(humanise_throughput(0), "0 B/s");
        assert_eq!(humanise_throughput(1024), "1.0 KiB/s");
        assert_eq!(humanise_throughput(1024 * 1024), "1.0 MiB/s");
        assert_eq!(humanise_throughput(1024 * 1024 * 1024), "1.0 GiB/s");
        // Sub-KiB ranges still render with one decimal so the label
        // doesn't oscillate as the executor's throughput climbs
        // through 512 → 1024.
        assert_eq!(humanise_throughput(512), "512.0 B/s");
    }
}
