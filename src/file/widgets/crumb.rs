//! Breadcrumb widget. Roadmap Step 25 (SPEC §3.3 item 4 — statusbar
//! path crumbs).
//!
//! Renders the path components of `cwd` as a row of clickable
//! `button(text(seg))` elements; each press emits a
//! `Message::Navigate(<accumulated-path>)` carrying the prefix up to
//! that segment. The journey-J2 "click 'sources' to jump up" beat
//! rides on the message wiring.
//!
//! The path-to-tokens projection lives in
//! `crate::file::view::statusbar::crumb_tokens` (so the unit test can
//! pin the token shape without driving iced). This widget just turns
//! that token list into iced buttons.

use std::path::{Path, PathBuf};

use iced::widget::{button, row, text};
use iced::Element;

use crate::file::app::Message;

/// Build the breadcrumb row for `cwd`. Each segment becomes a
/// `button` whose press emits [`Message::Navigate`] carrying the
/// accumulated path prefix.
///
/// If `$HOME` is not set the row falls through to the absolute path
/// components — `tokens` still resolves to a usable list, the leading
/// `~` segment just doesn't appear.
pub fn crumb(cwd: &Path) -> Element<'_, Message> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let tokens = crate::file::view::statusbar::crumb_tokens(cwd, &home);
    // Accumulate the on-press payload as we walk the tokens — each
    // segment's button navigates to the prefix path ending at that
    // segment.
    let mut accum = if tokens.first().map(|s| s.as_str()) == Some("~") {
        home.clone()
    } else {
        PathBuf::from("/")
    };
    let mut row_widget = row![].spacing(4);
    for (idx, seg) in tokens.iter().enumerate() {
        // Skip the `~` segment for the path build, but render its
        // button — pressing `~` navigates back to `$HOME`.
        if !(idx == 0 && seg == "~") {
            accum.push(seg);
        }
        row_widget =
            row_widget.push(button(text(seg.clone())).on_press(Message::Navigate(accum.clone())));
    }
    row_widget.into()
}
