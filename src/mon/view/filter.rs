//! `/` filter overlay — textbox + compiled-regex helpers.
//!
//! Roadmap: `specs/roadmaps/sy-mon/ROADMAP.md` Step 18. The popup
//! opens this overlay when the user presses `/`; subsequent character
//! keypresses extend the pattern, Backspace pops a char, `Esc` clears
//! the filter, and pressing `/` again is a no-op (the overlay is
//! already open).
//!
//! The overlay paints a single-row textbox along the top of the panel
//! area showing the current pattern. Per SPEC §3 SCOPE §4 the overlay
//! is read-only chrome — the actual filter state lives on
//! [`crate::mon::state::State::filter`] as the compiled regex, so the
//! per-panel projections (currently [`crate::mon::view::aiplane`])
//! can read one source of truth.
//!
//! ## Pure helpers
//!
//! - [`open`] — set the filter to `Some(empty regex)`. Pure; called
//!   from the keypress handler.
//! - [`apply_char`] — append a char to the current pattern and
//!   recompile. Invalid intermediate patterns (e.g. `^sy_npu_(`)
//!   collapse to "match everything" until the user either backspaces
//!   or types a closing paren — fail open so the overlay never
//!   freezes mid-keystroke.
//! - [`apply_backspace`] — pop the last char.
//! - [`close`] — clear the filter (`Esc`).
//! - [`pattern`] — read the current pattern back out for the overlay's
//!   textbox label.

use iced::widget::{container, text};
use iced::{Background, Border, Element, Length, Theme};
use regex::Regex;

use super::super::app::Message;
use super::super::state::State;
use super::super::theme::Palette;

/// Open the filter overlay with an empty pattern. Empty regex matches
/// every metric label, so opening the overlay alone hides nothing —
/// the user must type a pattern for `metric_matches` to start
/// dropping rows. Spec test `slash_opens_filter_overlay` asserts on
/// this shape (`state.filter = Some(<empty>)`).
pub fn open(state: &mut State) {
    state.filter = Some(Regex::new("").expect("empty regex always compiles"));
}

/// Append a character to the active filter pattern and recompile.
/// No-op if the overlay isn't open. Recompile failure is non-fatal:
/// we keep the previous compiled regex so the panel set keeps
/// rendering the last valid filter while the user finishes typing.
pub fn apply_char(state: &mut State, c: char) {
    if let Some(re) = state.filter.as_ref() {
        let mut s = re.as_str().to_string();
        s.push(c);
        match Regex::new(&s) {
            Ok(new) => state.filter = Some(new),
            Err(_) => {
                // Mid-typing invalid pattern (e.g. unbalanced `(`).
                // Keep the last-good compile so the panel doesn't
                // flicker; the next keystroke retries.
            }
        }
    }
}

/// Pop the last character from the filter pattern. No-op if the
/// overlay isn't open or the pattern is already empty.
pub fn apply_backspace(state: &mut State) {
    if let Some(re) = state.filter.as_ref() {
        let mut s = re.as_str().to_string();
        if s.pop().is_none() {
            return;
        }
        // Truncated pattern can't be invalid (regex grammar is a
        // prefix grammar for the well-formed subset we care about),
        // but compile defensively just in case.
        if let Ok(new) = Regex::new(&s) {
            state.filter = Some(new);
        }
    }
}

/// Close the filter overlay (`Esc`). Clears the active filter so all
/// panel rows reappear.
pub fn close(state: &mut State) {
    state.filter = None;
}

/// Return the current filter pattern as a borrowed `&str` (for the
/// overlay's textbox label). `None` when the overlay is closed.
pub fn pattern(state: &State) -> Option<&str> {
    state.filter.as_ref().map(|re| re.as_str())
}

/// Build the textbox element painted along the top of the panel area
/// when the filter overlay is open. Returns `None` when the overlay
/// is closed so the caller can `.push(filter)` unconditionally.
pub fn overlay<'a>(state: &State, palette: &Palette) -> Option<Element<'a, Message>> {
    let pat = pattern(state)?;
    let display = if pat.is_empty() {
        "/(type a regex; Esc to close)".to_string()
    } else {
        format!("/{pat}")
    };
    let accent = palette.accent;
    let ink = palette.ink;
    Some(
        container(text(display).size(12).color(ink))
            .padding(6)
            .width(Length::Fill)
            .style(move |_t: &Theme| container::Style {
                background: Some(Background::Color(accent)),
                border: Border::default(),
                ..Default::default()
            })
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sy_core::mon::ring::Ring;

    fn fresh_state() -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        std::mem::forget(dir);
        let ring = Ring::open_or_rebuild(&path, 600, 16).expect("ring");
        State::new(ring)
    }

    #[test]
    fn open_sets_empty_regex() {
        let mut state = fresh_state();
        open(&mut state);
        assert_eq!(state.filter.as_ref().map(|re| re.as_str()), Some(""));
    }

    #[test]
    fn apply_char_extends_pattern() {
        let mut state = fresh_state();
        open(&mut state);
        for c in "^sy_npu_".chars() {
            apply_char(&mut state, c);
        }
        assert_eq!(
            state.filter.as_ref().map(|re| re.as_str()),
            Some("^sy_npu_")
        );
    }

    #[test]
    fn apply_char_invalid_pattern_keeps_last_good() {
        let mut state = fresh_state();
        open(&mut state);
        apply_char(&mut state, '^');
        // Unbalanced paren — `^( ` is not a valid pattern; the helper
        // must keep `^` as the last-good compile.
        apply_char(&mut state, '(');
        assert_eq!(state.filter.as_ref().map(|re| re.as_str()), Some("^"));
    }

    #[test]
    fn apply_backspace_pops() {
        let mut state = fresh_state();
        open(&mut state);
        apply_char(&mut state, 'a');
        apply_char(&mut state, 'b');
        apply_backspace(&mut state);
        assert_eq!(state.filter.as_ref().map(|re| re.as_str()), Some("a"));
    }

    #[test]
    fn close_clears_filter() {
        let mut state = fresh_state();
        open(&mut state);
        apply_char(&mut state, 'x');
        close(&mut state);
        assert!(state.filter.is_none());
    }

    #[test]
    fn overlay_is_none_when_closed() {
        let state = fresh_state();
        let palette = Palette::ink_fallback();
        assert!(overlay(&state, &palette).is_none());
    }
}
