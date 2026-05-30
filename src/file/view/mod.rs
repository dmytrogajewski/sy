//! Responsive layout root for the `sy file` xdg-toplevel window.
//! Roadmap Step 24.
//!
//! `view::root(state)` is the iced `view` callback the app reactor
//! invokes once per frame. The function picks the layout ladder
//! ([`LayoutMode::ThreePane`] / [`TwoPane`] / [`OnePane`]) off
//! [`State::mode`] (which the Step 24 `Message::WindowResized`
//! reducer already wrote), then composes the appropriate
//! 1 / 2 / 3 [`pane`] widgets inside a top-level
//! [`iced::widget::Row`] using [`Length::FillPortion`] so iced spreads
//! the available width per pane.
//!
//! ## Width thresholds
//!
//! SPEC §3.2 row 2 pins:
//!
//! | width | mode        |
//! |-------|-------------|
//! | ≥1100 | `ThreePane` |
//! | ≥720  | `TwoPane`   |
//! | <720  | `OnePane`   |
//!
//! [`mode_for_width`] is the pure function that does the mapping; the
//! reducer in `app::update` calls it on every `WindowResized` event.
//!
//! ## Pure-Rust descriptor
//!
//! [`root_descriptor`] returns a [`ViewDescriptor`] — a pure-Rust
//! shape mirroring the same composition decision [`root`] makes. The
//! Step 24 e2e reads this instead of round-tripping through iced's
//! runtime (iced 0.14's `Element` has no public introspection
//! surface). The two functions stay in sync because [`root`] derives
//! its `pane_count` from [`ViewDescriptor::pane_count`] too.

pub mod commandbar;
// Step 32 (SPEC §3.3 item 14) — mounts sidebar + `:m` palette
// overlay. The leftmost column of the 3-pane layout reaches for
// `mounts_panel::mounts_panel`; the `:m` palette mode paints
// `mounts_panel::mounts_overlay` instead.
pub mod mounts_panel;
pub mod pane;
pub mod preview;
pub mod statusbar;

use iced::widget::{column, container, row, Row};
use iced::{Element, Length};

use crate::file::app::Message;
use crate::file::state::{LayoutMode, State};
use crate::file::widgets::chip::{mode_chip, selection_chip};

/// SPEC §3.2 row 2 lower bound for the 3-pane ladder. Pulled out as
/// a `pub const` so the e2e + the unit tests in this module share
/// the same threshold table.
pub const THREE_PANE_MIN_WIDTH_PX: u32 = 1100;
/// SPEC §3.2 row 2 lower bound for the 2-pane ladder.
pub const TWO_PANE_MIN_WIDTH_PX: u32 = 720;

/// Pure projection of window width → [`LayoutMode`]. Called from the
/// `Message::WindowResized` reducer in `app::update`; pinning it as a
/// stand-alone function (rather than inline in the reducer) means the
/// Step 24 unit tests can pin the threshold table without touching
/// the iced runtime.
pub fn mode_for_width(width_px: u32) -> LayoutMode {
    if width_px >= THREE_PANE_MIN_WIDTH_PX {
        LayoutMode::ThreePane
    } else if width_px >= TWO_PANE_MIN_WIDTH_PX {
        LayoutMode::TwoPane
    } else {
        LayoutMode::OnePane
    }
}

/// Pure-Rust descriptor for what [`root`] composed this frame. Step
/// 24's e2e asserts on this shape because iced 0.14's `Element` does
/// not expose a public introspection surface — the
/// roadmap-mandated "expand scope inline if iced's Element doesn't
/// support the introspection" escape hatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDescriptor {
    /// Layout mode that fed the composition. Mirrors `state.mode`.
    pub mode: LayoutMode,
    /// Count of `pane()` widgets [`root`] composed under the top-level
    /// row. 3 for `ThreePane`, 2 for `TwoPane`, 1 for `OnePane`.
    pub pane_count: usize,
    /// Per-pane sub-descriptors in left→right order so the e2e can
    /// assert "no entries lost or duplicated across reflow" by
    /// summing `entry_count` across the descriptors.
    pub panes: Vec<pane::PaneDescriptor>,
    /// Step 32 (SPEC §3.3 item 14) — whether the mounts sidebar is
    /// painted in the current composition. `true` only when
    /// `mode == LayoutMode::ThreePane`; in 2-pane / 1-pane modes the
    /// sidebar collapses and the `:m` palette overlay takes over.
    pub mounts_shown: bool,
}

/// Derive the descriptor from a [`State`]. Honest projection — the
/// fields match exactly what [`root`] would compose under the same
/// state.
pub fn root_descriptor(state: &State) -> ViewDescriptor {
    let mode = state.mode;
    let panes = panes_for_mode(state, mode);
    let mounts_shown = matches!(mode, LayoutMode::ThreePane);
    ViewDescriptor {
        mode,
        pane_count: panes.len(),
        panes,
        mounts_shown,
    }
}

/// Build the per-pane descriptors for the given mode. Pulled out so
/// [`root`] and [`root_descriptor`] both call into one source of
/// truth.
fn panes_for_mode(state: &State, mode: LayoutMode) -> Vec<pane::PaneDescriptor> {
    match mode {
        LayoutMode::ThreePane => vec![
            pane::descriptor_for(&state.panes.parent, false),
            pane::descriptor_for(&state.panes.current, true),
            pane::descriptor_for(&state.panes.preview, false),
        ],
        LayoutMode::TwoPane => vec![
            pane::descriptor_for(&state.panes.current, true),
            pane::descriptor_for(&state.panes.preview, false),
        ],
        LayoutMode::OnePane => vec![pane::descriptor_for(&state.panes.current, true)],
    }
}

/// Iced view callback. Replaces the Step 23 scaffold body
/// (`container(text("sy file — ready"))`) with the real 1 / 2 / 3-pane
/// composition, plus a thin top-row carrying the mode + selection
/// chips so the SPEC §6 "user doesn't know why the layout shrank"
/// risk is mitigated from frame 0.
///
/// Routes through [`root_descriptor`] for the composition decision so
/// the production paint path and the e2e introspection surface stay
/// in lockstep — a future change to one is visibly forced through
/// the other (Rule 2 "expand scope inline" hygiene).
pub fn root(state: &State) -> Element<'_, Message> {
    let descriptor = root_descriptor(state);
    let chrome = row![
        mode_chip(descriptor.mode),
        selection_chip(state.selection.len()),
    ]
    .spacing(8);
    let composed: Row<'_, Message> = match descriptor.mode {
        // Step 32 (SPEC §3.3 item 14) — leftmost slot is the mounts
        // sidebar. `FillPortion(1)` keeps it thin so the existing
        // parent · current · preview ratios stay readable.
        LayoutMode::ThreePane => row![
            mounts_slot(&state.mounts, 1),
            pane_slot(&state.panes.parent, false, 1),
            pane_slot(&state.panes.current, true, 2),
            preview_slot(state, 2),
        ]
        .spacing(4),
        LayoutMode::TwoPane => row![
            pane_slot(&state.panes.current, true, 3),
            preview_slot(state, 2),
        ]
        .spacing(4),
        LayoutMode::OnePane => row![pane_slot(&state.panes.current, true, 1)],
    };
    container(column![chrome, composed].spacing(6))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(8)
        .into()
}

/// Wrap one [`pane::pane`] in a fill-portion container so the
/// top-level row spreads space the SPEC §3.3 ratios (`parent · current
/// · preview` weighted 1·2·2) call for. iced's `FillPortion` rounds
/// surprisingly at small sizes (SPEC §6 risk); the width-400
/// `OnePane` test pins the no-portion-needed branch.
fn pane_slot<'a>(
    p: &'a crate::file::state::Pane,
    focused: bool,
    portion: u16,
) -> Element<'a, Message> {
    container(pane::pane(p, focused))
        .width(Length::FillPortion(portion))
        .height(Length::Fill)
        .into()
}

/// Step 32 (SPEC §3.3 item 14) — wrap [`mounts_panel::mounts_panel`]
/// in a fill-portion container so the 3-pane layout's leftmost slot
/// sits inside the same row-spread shape `pane_slot` uses.
fn mounts_slot<'a>(
    mounts: &'a [crate::file::fs::mounts::Mount],
    portion: u16,
) -> Element<'a, Message> {
    container(mounts_panel::mounts_panel(mounts, false))
        .width(Length::FillPortion(portion))
        .height(Length::Fill)
        .into()
}

/// Step 26: the preview slot replaces what used to be a third
/// `pane_slot(&state.panes.preview, …)` (a child-listing pane) when a
/// file is hovered. Routes through [`preview::preview`] which
/// dispatches by MIME (image → `iced::widget::image`, text →
/// syntect-highlighted column, otherwise the Step 27 plugin
/// fallback). Falls back to the legacy child-listing pane when the
/// cursor sits on a directory (or the entries are empty).
///
/// The MIME routing reads the cursor entry's `mime_hint` (Step 19's
/// `fs::walk` fills it), or the `fs::mime::mime_for` ladder when the
/// hint isn't cached.
fn preview_slot<'a>(state: &'a State, portion: u16) -> Element<'a, Message> {
    let cursor_entry = state.panes.current.entries.get(state.panes.current.cursor);
    let body: Element<'a, Message> = match cursor_entry {
        Some(entry)
            if matches!(
                entry.kind,
                crate::file::state::EntryKind::File | crate::file::state::EntryKind::Symlink
            ) =>
        {
            let path = state.panes.current.cwd.join(&entry.name);
            preview::preview(state, entry, path)
        }
        _ => pane::pane(&state.panes.preview, false),
    };
    container(body)
        .width(Length::FillPortion(portion))
        .height(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roadmap Step 24 pin: 1280 px window → `ThreePane`. The default
    /// `sy file` window size is 1280×800 (see `app::DEFAULT_WIDTH`),
    /// so this test locks the journey-J2 "3-pane render" default.
    #[test]
    fn mode_for_width_three_at_1280() {
        assert_eq!(mode_for_width(1280), LayoutMode::ThreePane);
    }

    /// Roadmap Step 24 pin: 800 px window → `TwoPane`. Mirrors a
    /// niri half-column on a 1600 px monitor — the SPEC §3.2 row 2
    /// "≥720 px = 2-pane" lower bound.
    #[test]
    fn mode_for_width_two_at_800() {
        assert_eq!(mode_for_width(800), LayoutMode::TwoPane);
    }

    /// Roadmap Step 24 pin: 400 px window → `OnePane`. Below the
    /// `TWO_PANE_MIN_WIDTH_PX` threshold; the journey-J7 reflow beat
    /// observes the transition.
    #[test]
    fn mode_for_width_one_at_400() {
        assert_eq!(mode_for_width(400), LayoutMode::OnePane);
    }

    /// Threshold table is inclusive on the lower bound. SPEC §3.2
    /// row 2 says ≥1100 px = 3-pane, ≥720 px = 2-pane — pinning the
    /// exact threshold defends against an off-by-one shift.
    #[test]
    fn mode_thresholds_are_inclusive() {
        assert_eq!(mode_for_width(1100), LayoutMode::ThreePane);
        assert_eq!(mode_for_width(1099), LayoutMode::TwoPane);
        assert_eq!(mode_for_width(720), LayoutMode::TwoPane);
        assert_eq!(mode_for_width(719), LayoutMode::OnePane);
    }

    /// `root_descriptor` matches the SPEC §3.3 row 3 composition
    /// shape — 3 panes when `ThreePane`, 2 panes when `TwoPane`, 1
    /// pane when `OnePane`. Journey-J7 beat reads this to assert
    /// "no entries lost or duplicated across reflow".
    #[test]
    fn root_descriptor_pane_count_matches_mode() {
        fn at(mode: LayoutMode) -> usize {
            let state = State {
                mode,
                ..Default::default()
            };
            root_descriptor(&state).pane_count
        }
        assert_eq!(at(LayoutMode::ThreePane), 3);
        assert_eq!(at(LayoutMode::TwoPane), 2);
        assert_eq!(at(LayoutMode::OnePane), 1);
    }

    /// Step 32 — mounts sidebar is only painted under `ThreePane`.
    /// `TwoPane` / `OnePane` collapse the sidebar; the operator
    /// reaches the same data through `:m` in those modes.
    #[test]
    fn root_descriptor_mounts_shown_only_in_three_pane() {
        fn at(mode: LayoutMode) -> bool {
            let state = State {
                mode,
                ..Default::default()
            };
            root_descriptor(&state).mounts_shown
        }
        assert!(at(LayoutMode::ThreePane));
        assert!(!at(LayoutMode::TwoPane));
        assert!(!at(LayoutMode::OnePane));
    }
}
