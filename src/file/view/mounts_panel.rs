//! Mounts sidebar + `:m` palette overlay. Roadmap Step 32 / SPEC
//! §3.3 item 14.
//!
//! Two surfaces:
//!
//! * [`mounts_panel`] — leftmost column in the 3-pane layout; each
//!   row is a clickable [`iced::widget::mouse_area`] that dispatches
//!   [`crate::file::app::Message::Navigate`] to the mount point on
//!   press.
//! * [`mounts_overlay`] — centred overlay paint for the `:m` palette
//!   mode (2-pane / 1-pane layouts). Reads a filter body off
//!   [`crate::file::state::commandbar::mounts_filter_body`] and
//!   narrows the row list against the mount-point string.

use iced::widget::{column, container, mouse_area, text, Column};
use iced::{Border, Element, Length};

use crate::file::app::Message;
use crate::file::fs::mounts::Mount;

/// Render the mounts sidebar — one clickable row per mount. The
/// `focused` flag tints the surrounding container brighter so a
/// future focus-cycling keymap (`Tab`) can highlight the sidebar
/// without re-rendering the row list. SPEC §3.3 item 14 — the
/// sidebar paints in 3-pane mode only; 2-pane / 1-pane layouts
/// reach the same data through [`mounts_overlay`] below.
pub fn mounts_panel<'a>(mounts: &'a [Mount], focused: bool) -> Element<'a, Message> {
    let palette = crate::mon::theme::load_or_ink();
    let rows: Column<'a, Message> = column(mounts.iter().map(mount_row)).spacing(2);
    let body = container(rows)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(4);
    let bg = if focused {
        iced::Background::Color(palette.bg2)
    } else {
        iced::Background::Color(palette.bg)
    };
    container(body)
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            background: Some(bg),
            text_color: Some(palette.ink),
            border: Border::default(),
            ..Default::default()
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// One mount row. Clicking it dispatches
/// [`Message::Navigate`] to the mount point so the current pane
/// walks to that directory. The removable-media glyph (`*`) paints
/// to the left when `mount.is_removable` so the operator can spot
/// USB sticks at a glance.
fn mount_row(mount: &Mount) -> Element<'_, Message> {
    let glyph = if mount.is_removable { "*" } else { " " };
    let label = format!("{glyph} {}  {}", mount.mount_point.display(), mount.fs_type);
    let target = mount.mount_point.clone();
    mouse_area(text(label).width(Length::Fill))
        .on_press(Message::Navigate(target))
        .into()
}

/// Render the `:m` palette overlay. Filters the mount list against
/// `filter` (the body of the palette query past the verb) and paints
/// a centred container with one row per matched mount. Pure view
/// helper — no I/O.
pub fn mounts_overlay<'a>(mounts: &'a [Mount], filter: &'a str) -> Element<'a, Message> {
    let palette = crate::mon::theme::load_or_ink();
    let filter_lc = filter.to_ascii_lowercase();
    let filtered: Vec<&'a Mount> = if filter.is_empty() {
        mounts.iter().collect()
    } else {
        mounts
            .iter()
            .filter(|m| {
                m.mount_point
                    .display()
                    .to_string()
                    .to_ascii_lowercase()
                    .contains(&filter_lc)
            })
            .collect()
    };
    let rows: Column<'a, Message> = column(filtered.into_iter().map(mount_row)).spacing(2);
    container(rows)
        .padding(8)
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(palette.bg2)),
            text_color: Some(palette.ink),
            border: Border::default(),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mount(point: &str, fs: &str) -> Mount {
        Mount {
            mount_point: PathBuf::from(point),
            source: format!("/dev/{fs}"),
            fs_type: fs.into(),
            options: vec![],
            is_removable: false,
        }
    }

    /// `mounts_panel` paints without panicking for the empty and
    /// non-empty cases. Pure smoke — iced 0.14 has no introspection
    /// for the descendant tree.
    #[test]
    fn mounts_panel_renders_without_panic() {
        let mounts = vec![mount("/", "ext4"), mount("/home", "btrfs")];
        let _ = mounts_panel(&mounts, false);
        let _ = mounts_panel(&[], true);
    }

    /// `mounts_overlay` filter narrows by case-insensitive substring
    /// on the mount-point. Empty filter shows every row.
    #[test]
    fn mounts_overlay_renders_without_panic() {
        let mounts = vec![mount("/", "ext4"), mount("/home", "btrfs")];
        let _ = mounts_overlay(&mounts, "");
        let _ = mounts_overlay(&mounts, "home");
        let _ = mounts_overlay(&mounts, "HOME");
    }
}
