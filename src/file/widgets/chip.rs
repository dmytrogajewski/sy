//! Statusbar-shaped "chip" widgets for the responsive layout ladder.
//! Roadmap Step 24.
//!
//! Two chips ship today:
//!
//! - [`selection_chip`] — `"{n} selected"` text inside a container.
//!   When `n == 0` the container is empty (zero-height) so the
//!   journey-J5 "nothing selected" state doesn't paint a stale label.
//!   Step 25's full statusbar will host this; Step 24 publishes the
//!   helper today so `view::root`'s shape stays observable.
//! - [`mode_chip`] — `"3-pane" / "2-pane" / "1-pane"` text inside a
//!   container. The journey-J7 reflow beat exposes the chip so the
//!   user (and the e2e) can confirm the responsive ladder fired.
//!
//! Both return `iced::Element<Message>` so the Step 25 statusbar can
//! glue them into a `row!` without re-wrapping.

use iced::widget::{container, text};
use iced::{Element, Length};

use crate::file::app::Message;
use crate::file::search::knowledge::KnowledgeStatus;
use crate::file::state::LayoutMode;

/// "{n} selected" chip. Returns an empty (zero-height) container when
/// `count == 0` so the statusbar row collapses naturally instead of
/// reserving space for a "0 selected" label nobody reads.
///
/// The container holds no [`iced::Theme`]-aware styling today; the
/// Step 25 statusbar wraps it in a tinted background as the rest of
/// the chrome lands. Pinning the text+visibility contract here means
/// the journey-J5 selection counter has a stable wire shape from
/// today onwards.
pub fn selection_chip<'a>(count: usize) -> Element<'a, Message> {
    if count == 0 {
        // Zero-height empty container — pin the shape so consumers
        // (Step 25 statusbar) can unconditionally `.push(chip)`
        // without a separate `if count > 0` branch.
        return container(text("")).width(Length::Shrink).into();
    }
    container(text(format!("{count} selected")))
        .padding(4)
        .into()
}

/// `"3-pane" / "2-pane" / "1-pane"` chip mirroring the layout ladder.
/// SPEC §6 risks call out "user doesn't know why the layout shrank";
/// the chip is the cheapest fix — paint the current mode in the
/// statusbar so the responsive transition is observable.
pub fn mode_chip<'a>(mode: LayoutMode) -> Element<'a, Message> {
    let label = mode_label(mode);
    container(text(label)).padding(4).into()
}

/// Stable text label for [`LayoutMode`]. Pulled out as a `pub fn` so
/// the Step 25 statusbar + the journey-J7 e2e can both reference the
/// same string table without re-deriving it.
pub fn mode_label(mode: LayoutMode) -> &'static str {
    match mode {
        LayoutMode::ThreePane => "3-pane",
        LayoutMode::TwoPane => "2-pane",
        LayoutMode::OnePane => "1-pane",
    }
}

/// Knowledge-reachability chip. Roadmap Step 30 (SPEC §3.3 item 10 +
/// SPEC §6 risk row 3). Paints the chip body via
/// [`crate::file::view::statusbar::knowledge_chip_label`]; the
/// reachability tooltip rides on the same label table. The chip is
/// always visible (unlike `selection_chip`) so the operator can
/// always see whether `sy-knowledge.service` is alive — SPEC §6 calls
/// this out as the cheapest fix for the "did my search hit nothing,
/// or is the daemon down?" confusion.
pub fn knowledge_chip<'a>(status: KnowledgeStatus, hits: usize) -> Element<'a, Message> {
    let label = crate::file::view::statusbar::knowledge_chip_label(status, hits);
    container(text(label)).padding(4).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mode-chip label table is the wire shape the journey-J7 e2e and
    /// Step 25's statusbar both read. Pinning the strings here defends
    /// against silent drift (e.g. "Three-Pane" / "ThreePane").
    #[test]
    fn mode_label_table_is_stable() {
        assert_eq!(mode_label(LayoutMode::ThreePane), "3-pane");
        assert_eq!(mode_label(LayoutMode::TwoPane), "2-pane");
        assert_eq!(mode_label(LayoutMode::OnePane), "1-pane");
    }

    /// Step 30 DoD bullet `chip flips dim-grey on unreachability`: the
    /// chip text changes from "knowledge: idle" → "knowledge:
    /// unreachable" when the backend reports unreachable. Pinning the
    /// label table here defends the operator-visible string against
    /// silent drift.
    #[test]
    fn knowledge_chip_label_flips_on_unreachable() {
        use crate::file::view::statusbar::knowledge_chip_label;
        assert_eq!(
            knowledge_chip_label(KnowledgeStatus::Reachable, 0),
            "knowledge: idle"
        );
        assert_eq!(
            knowledge_chip_label(KnowledgeStatus::Reachable, 3),
            "knowledge: 3 hits"
        );
        assert_eq!(
            knowledge_chip_label(KnowledgeStatus::Unreachable, 0),
            "knowledge: unreachable"
        );
        assert_eq!(
            knowledge_chip_label(KnowledgeStatus::Timeout, 0),
            "knowledge: timeout"
        );
    }
}
