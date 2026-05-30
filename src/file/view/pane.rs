//! Pane list renderer. Roadmap Step 24.
//!
//! [`pane`] takes one [`Pane`] (the post-Step 14 state slice) and
//! returns the vertical `Column` of row widgets the journey-J2
//! 3-pane render is observing. Each row has three text fragments:
//!
//! 1. **Icon** — a [`crate::file::widgets::icon::icon_for`] Nerd-Font
//!    glyph picked from the entry's mime hint (or `inode/directory`
//!    for `EntryKind::Dir`).
//! 2. **Name** — basename, left-aligned and filling the row middle.
//! 3. **Meta** — `"{size}  {mtime}"`, right-aligned. Locked to a
//!    fixed-width column so a long filename doesn't push the size
//!    off-screen.
//!
//! The cursor row gets a [`Background`] tint pulled from the bar
//! palette's `bg2` slot when `focused == true`; non-focused panes
//! keep the cursor visible (a subtler tint) so the user can see
//! which row the cursor sits on after a focus change.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iced::widget::{column, container, mouse_area, row, text};
use iced::{Background, Border, Element, Length};

use crate::file::app::Message;
use crate::file::state::{Entry, EntryKind, Pane};
use crate::file::widgets::icon::icon_for;

/// Render one pane's entry list. Returns an `Element<Message>` the
/// Step 24 `view::root` composes inside its top-level `row!`. The
/// `focused` flag tints the cursor row brighter — Step 25's keymap
/// will wire `Tab` to flip focus across panes, but the rendering
/// contract has to land today so the journey-J2 e2e can see it.
pub fn pane<'a>(pane: &'a Pane, focused: bool) -> Element<'a, Message> {
    let palette = crate::mon::theme::load_or_ink();
    let rows = pane
        .entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| pane_row(entry, idx == pane.cursor, focused, &palette));
    let body = column(rows).spacing(2).width(Length::Fill);
    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(4)
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            background: Some(Background::Color(palette.bg)),
            text_color: Some(palette.ink),
            border: Border::default(),
            ..Default::default()
        })
        .into()
}

/// One entry row. Pulled out so the palette + focus chrome stays in
/// one place and the parent [`pane`] is "just" `iter().map(pane_row)`.
fn pane_row<'a>(
    entry: &'a Entry,
    is_cursor: bool,
    focused: bool,
    palette: &crate::file::theme::Palette,
) -> Element<'a, Message> {
    let glyph = pick_glyph(entry);
    // Cursor row chrome, yazi-style: the active row in EVERY pane —
    // focused current pane AND the parent / ancestor panes — paints
    // with the accent background so the whole descended chain stays
    // visibly highlighted (the breadcrumb trail). Ink inverts to `bg`
    // for contrast against the accent fill.
    let (bg, ink) = if is_cursor {
        (Some(Background::Color(palette.accent)), palette.bg)
    } else {
        (None, palette.ink)
    };
    // Names never wrap — a long name truncates at the column edge
    // instead of spilling into the size/mtime column (the overlap the
    // narrow parent pane showed before).
    let name = text(entry.name.as_str())
        .width(Length::Fill)
        .wrapping(iced::widget::text::Wrapping::None)
        .color(ink);
    // The size/mtime meta only renders in the focused pane. The parent
    // / ancestor panes are narrow (a thin breadcrumb column) and would
    // collide name-vs-meta; yazi shows names-only there too.
    let row_widget = if focused {
        let meta = format!("{}  {}", format_size(entry.size), format_mtime(entry.mtime));
        row![
            text(glyph.to_string())
                .width(Length::Fixed(20.0))
                .color(ink),
            name,
            text(meta)
                .width(Length::Fixed(150.0))
                .wrapping(iced::widget::text::Wrapping::None)
                .color(ink),
        ]
        .spacing(8)
        .padding(2)
    } else {
        row![
            text(glyph.to_string())
                .width(Length::Fixed(20.0))
                .color(ink),
            name,
        ]
        .spacing(8)
        .padding(2)
    };
    let styled = container(row_widget)
        .width(Length::Fill)
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            background: bg,
            text_color: Some(ink),
            border: Border::default(),
            ..Default::default()
        });
    // Preview follows the *cursor*, not the mouse pointer (yazi-style):
    // single-click positions the cursor (which resolves the preview),
    // double-click activates the entry. We deliberately do NOT wire
    // `on_enter` → hover-preview: previewing every row the pointer
    // drifts over churned the async resolver and fired a plugin probe
    // per directory. The journey-J3 e2e drives `Message::HoverEntry`
    // directly to exercise the plugin path without that churn.
    let entry_id = entry.id;
    mouse_area(styled)
        .on_press(Message::CursorTo(entry_id))
        .on_double_click(Message::ActivateEntry(entry_id))
        .into()
}

/// Pick the Nerd-Font glyph for an entry — directories get the
/// folder glyph unconditionally; files defer to their `mime_hint`
/// (or the generic-file fallback when absent).
fn pick_glyph(entry: &Entry) -> char {
    match entry.kind {
        EntryKind::Dir => icon_for("inode/directory"),
        _ => icon_for(entry.mime_hint.as_deref().unwrap_or("")),
    }
}

/// Human-readable size. Bytes for < 1 KiB; KiB / MiB / GiB after.
/// Pulled out so the row column width can be sized to the longest
/// expected label (`"999.9 GiB"` = 9 chars) without measuring every
/// frame. Used by the Step 25 statusbar too.
pub fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    }
}

/// Human-readable mtime. Emits `"YYYY-MM-DD"` for any time more than
/// 24 h ago and `"hh:mm"` otherwise — matches yazi's display
/// convention so the journey-J2 user doesn't have to relearn the
/// format. Returns `"-"` for `UNIX_EPOCH` (the Step 14 test fixtures'
/// default) so synthetic entries don't display as `"1970-01-01"`.
pub fn format_mtime(t: SystemTime) -> String {
    if t == UNIX_EPOCH {
        return "-".to_string();
    }
    let now = SystemTime::now();
    let secs_since = now.duration_since(t).unwrap_or(Duration::ZERO).as_secs();
    let dt = chrono::DateTime::<chrono::Local>::from(t);
    if secs_since < 86_400 {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

/// Pure descriptor of one pane's render shape — the Step 24 e2e
/// reads this instead of round-tripping through iced's runtime. The
/// `entries` count + cursor row are the journey-J2 / J7 invariants
/// the test asserts ("3 sub-panes, no entries lost"); `focused`
/// rides through so Step 25 can extend the descriptor without
/// breaking the wire shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneDescriptor {
    /// Row count rendered into this pane.
    pub entry_count: usize,
    /// Cursor index inside `entries`. Always `< entry_count` (or 0
    /// when the pane is empty).
    pub cursor: usize,
    /// Whether the pane is the focused one (only one is at a time).
    pub focused: bool,
}

/// Build the descriptor for one pane — sister fn to [`pane`] so the
/// e2e can assert the responsive ladder's shape without driving
/// iced.
pub fn descriptor_for(pane: &Pane, focused: bool) -> PaneDescriptor {
    PaneDescriptor {
        entry_count: pane.entries.len(),
        cursor: pane.cursor,
        focused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    fn sample_dir(id: u64, name: &str) -> Entry {
        Entry {
            id,
            name: name.to_owned(),
            kind: EntryKind::Dir,
            size: 0,
            mtime: UNIX_EPOCH,
            is_symlink: false,
            broken_link: false,
            readable: true,
            mime_hint: None,
            symlink_target: None,
        }
    }

    /// `descriptor_for` mirrors the pane's row count + cursor — the
    /// journey-J2 e2e reads this to assert "3 sub-panes rendered".
    #[test]
    fn descriptor_mirrors_pane_shape() {
        let mut p = Pane::new(PathBuf::from("/tmp"));
        p.entries.push(sample_dir(0, "a"));
        p.entries.push(sample_dir(1, "b"));
        p.cursor = 1;
        let d = descriptor_for(&p, true);
        assert_eq!(d.entry_count, 2);
        assert_eq!(d.cursor, 1);
        assert!(d.focused);
    }

    /// Sizes ladder up through the expected unit changes.
    #[test]
    fn format_size_ladder() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(2 * 1024 * 1024), "2.0 MiB");
    }

    /// `UNIX_EPOCH` collapses to `"-"` so synthetic Step 14 fixtures
    /// don't paint a misleading 1970 timestamp.
    #[test]
    fn format_mtime_epoch_dashes() {
        assert_eq!(format_mtime(UNIX_EPOCH), "-");
    }

    /// `pick_glyph` honours the dir > mime_hint > fallback order.
    #[test]
    fn pick_glyph_dir_wins_over_mime_hint() {
        let mut e = sample_dir(0, "x");
        e.mime_hint = Some("text/markdown".to_string());
        // Dir kind beats the markdown mime hint — folder glyph wins.
        assert_eq!(pick_glyph(&e), super::icon_for("inode/directory"));
    }
}
