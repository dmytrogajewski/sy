//! Command-bar view. Roadmap Step 25 (SPEC §3.3 item 4 + item 7).
//!
//! When `state.commandbar.mode == CommandMode::Closed` the bar
//! returns a zero-height empty container so the journey-J2 default
//! layout doesn't reserve a status row. When opened (Filter or
//! Palette), it paints a single `text_input` row plus — for Palette
//! mode — a completion list of verbs whose name starts with the
//! current query.
//!
//! The pure helper [`completions_for`] (re-exported from
//! [`crate::file::state::commandbar`]) is the Step 25 introspection
//! hook the unit test reads directly so the completion-list
//! assertion doesn't need iced introspection.

use iced::widget::{button, column, container, text, text_input};
use iced::{Element, Length};

use crate::file::app::Message;
use crate::file::search::knowledge::KnowledgeStatus;
use crate::file::state::{CommandMode, State};

pub use crate::file::state::commandbar::completions_for;

/// Prompt shown inside the empty `text_input` for the `/` filter.
pub const FILTER_PROMPT: &str = "filter…";
/// Prompt for the `:` palette.
pub const PALETTE_PROMPT: &str = "verb (k / copy / move / trash / mkdir / …)";

/// SPEC §6 risk-row 3 hint shown under the palette when the operator
/// types `:k <query>` and `sy-knowledge.service` is unreachable. The
/// literal text is the runbook entry point the operator follows to
/// repair the dial (the journey-J4 fallback narrative).
pub const INDEX_HINT: &str = "sy-knowledge unreachable — try `:index .` to index this cwd";

/// `:k <q>` palette-prefix detector + body extractor live in the
/// state slice ([`crate::file::state::commandbar`]) so the
/// integration-test crate can read them without dragging iced in.
pub use crate::file::state::commandbar::{is_knowledge_query, knowledge_query_body};
/// `:m <filter>` palette-prefix detectors. Step 32 — the mounts
/// overlay paints when the operator is composing a `:m` query in
/// 2-pane / 1-pane modes (SPEC §3.3 item 14). Re-exported through
/// the view-layer so consumers reach for one path.
pub use crate::file::state::commandbar::{is_mounts_query, mounts_filter_body};

/// Render the commandbar element. Returns a zero-height container
/// when closed.
pub fn commandbar(state: &State) -> Element<'_, Message> {
    let bar = &state.commandbar;
    if !bar.is_open() {
        // Zero-height empty container — keeps the parent column's
        // shape stable so toggling open/closed doesn't shift the
        // pane composition vertically.
        return container(text("")).width(Length::Shrink).into();
    }
    let prompt = match bar.mode {
        CommandMode::Filter => FILTER_PROMPT,
        CommandMode::Palette => PALETTE_PROMPT,
        CommandMode::Closed => "",
    };
    // Step 30: Enter in `:k <query>` mode submits the knowledge query
    // instead of closing the bar. The reducer arm dispatches the
    // async `query` call.
    let submit_msg = if bar.mode == CommandMode::Palette && is_knowledge_query(&bar.query) {
        Message::KnowledgeQuery(knowledge_query_body(&bar.query).to_owned())
    } else {
        Message::CommandClose
    };
    let input = text_input(prompt, &bar.query)
        .on_input(Message::CommandQueryChanged)
        .on_submit(submit_msg)
        .padding(4)
        .width(Length::Fill);
    let body: Element<'_, Message> = if bar.mode == CommandMode::Palette {
        let comps = completions_for(&bar.query);
        let mut col = column![input].spacing(2);
        for verb in comps {
            // Each completion is a `button` so a mouse click fires
            // `Message::CommandSelectVerb(verb)`. The reducer plants
            // the verb on `state.commandbar.selected_verb`; Step 26+
            // will route the Enter key to the same arm.
            col = col.push(button(text(verb.clone())).on_press(Message::CommandSelectVerb(verb)));
        }
        // Step 30 SPEC §6 risk-row 3 — when the operator is composing
        // a `:k <query>` and the knowledge backend is reachable=false,
        // paint the `:index .` hint inline so the operator sees the
        // repair runbook entry point without leaving the palette.
        if is_knowledge_query(&bar.query) && state.knowledge.status != KnowledgeStatus::Reachable {
            col = col.push(text(INDEX_HINT));
        }
        // Step 32 SPEC §3.3 item 14 — when the operator is composing
        // a `:m <filter>` and the layout has collapsed away from
        // `ThreePane` (so the sidebar isn't already painted), drop in
        // the mounts overlay so the operator can still navigate to
        // any mount from the palette.
        if is_mounts_query(&bar.query) {
            let filter = mounts_filter_body(&bar.query);
            col = col.push(super::mounts_panel::mounts_overlay(&state.mounts, filter));
        }
        col.into()
    } else {
        input.into()
    };
    container(body).width(Length::Fill).padding(4).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roadmap Step 25 pin: opening the palette with query `"k"`
    /// surfaces `k` at the head of the completion list. The view
    /// layer reads this via [`completions_for`] so the assertion
    /// pins the wire shape without needing iced introspection.
    #[test]
    fn tab_completion_offers_known_verbs() {
        let comps = completions_for("k");
        assert!(
            comps.iter().any(|v| v == "k"),
            "completion list under prefix 'k' must contain 'k', got {comps:?}"
        );
        assert_eq!(
            comps[0], "k",
            "'k' must rank first under the 'k' prefix, got {comps:?}"
        );
    }

    /// An empty `query` (palette just opened) returns every known
    /// verb — the user sees the full menu before typing.
    #[test]
    fn empty_query_lists_all_known_verbs() {
        let comps = completions_for("");
        // Lower-bound on size: roadmap pins ≥8 verbs.
        assert!(
            comps.len() >= 8,
            "empty prefix must surface the full verb table, got {comps:?}"
        );
    }
}
