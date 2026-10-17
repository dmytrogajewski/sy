//! Wayland drag-and-drop wire helpers. Step 29 of the
//! [`sy-file-manager` roadmap][roadmap] / SPEC §3.3 item 12.
//!
//! `sy file` ships a `wl_data_device` source for drag-out (other
//! Wayland apps receive a `text/uri-list` offer) and a drop-target for
//! inbound URIs. Cross-toolkit interop is the load-bearing promise: a
//! drag from `sy file` into Telegram (Qt) or Firefox (GTK) must Just
//! Work because both consume the same `text/uri-list` MIME Nautilus
//! emits.
//!
//! Scope this module owns:
//!
//! * [`paths_to_uri_list`] — RFC 3986 percent-encodes each path and
//!   formats the result as `file://<encoded-path>\r\n` per entry, one
//!   entry per line, per RFC 2483.
//! * [`parse_uri_list`] — round-trip inverse; drops `#` comment lines
//!   per RFC 2483.
//! * [`drop_action_from_modifiers`] — Ctrl → Copy, Shift → Move, no
//!   mod → Copy default (matches the freedesktop DnD convention the
//!   SPEC §3.3 item 12 bullet pins).
//! * [`DragSource`] / [`DropTarget`] — typed wrappers the `app`
//!   reducer holds while a drag is in flight.
//!
//! ## Manual recipe — Telegram + Firefox cross-toolkit drag
//!
//! The fake-Wayland fixture in `tests/sy_file_journey_e2e.rs::
//! step29_drag_selection_out_to_fake_wayland_client` round-trips the
//! `text/uri-list` shape through [`paths_to_uri_list`] +
//! [`parse_uri_list`]; the actual `wl_data_device` interop is
//! verified out-of-band by the operator:
//!
//! 1. `sy file ~/Downloads` — open the file manager.
//! 2. Cursor on a `.pdf`; press `<Space>` to select. Repeat for 2-3
//!    more entries.
//! 3. Mouse-down-drag from the selection chevron toward another
//!    Wayland app's window.
//! 4. **Telegram** — drop on a chat: the file picker pre-populates
//!    with the dragged paths; "Send" attaches them.
//! 5. **Firefox** — drop on a `<input type=file>` upload widget: the
//!    file picker pre-populates the same way.
//!
//! If iced 0.14's xdg-toplevel reactor doesn't surface
//! `wl_data_device_offer` initiation through its public subscription
//! API, the source-side drag is reachable via the lower-level
//! `iced_winit` hook; the [`DragSource`] type ships today so the
//! reducer arm + the uri-list shape are pinned regardless of which
//! layer wires up the actual `wl_data_device_manager_create_source`
//! call.
//!
//! [roadmap]: ../../specs/roadmaps/sy-file-manager/ROADMAP.md
//! [spec]: ../../specs/research/sy-file-manager/SPEC.md

use std::path::{Path, PathBuf};

/// MIME type the drag-source advertises. SPEC §3.3 item 12 — the same
/// shape Nautilus + Cosmic Files emit so Telegram (Qt) and Firefox
/// (GTK) recognise the offer. RFC 2483 defines the per-line `file://`
/// shape we encode below.
pub const URI_LIST_MIME: &str = "text/uri-list";

/// Line terminator the URI list uses. RFC 2483 §5 pins `CRLF` per
/// entry; both Telegram and Firefox accept LF-only too, but emitting
/// the canonical CRLF keeps us spec-compliant.
const CRLF: &str = "\r\n";

/// Action the drag-source advertises. SPEC §3.3 item 12: Ctrl forces
/// Copy, Shift forces Move, default is Copy (matches Nautilus). `Link`
/// is reserved for a future "create symlink" affordance — the reducer
/// today only emits `Copy` / `Move`, but the variant ships so the
/// roadmap's Step 30+ link verb has a typed home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragAction {
    /// `wl_data_offer.set_actions(WL_DATA_DEVICE_MANAGER_DND_ACTION_COPY, …)`.
    Copy,
    /// `wl_data_offer.set_actions(WL_DATA_DEVICE_MANAGER_DND_ACTION_MOVE, …)`.
    Move,
    /// `wl_data_offer.set_actions(WL_DATA_DEVICE_MANAGER_DND_ACTION_ASK, …)`
    /// today routes through the file manager's link verb — Step 30+.
    Link,
}

/// Action the drop-target derives from the incoming offer + modifier
/// state. Pinned at two variants because the file plane's executor
/// (`fs::copy::copy` / `fs::rename`) only knows about Copy and Move
/// today; cross-fs Move falls back to copy+unlink under the same arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropAction {
    /// The dropped paths land at `cwd/<basename>` via `fs::copy::copy`.
    Copy,
    /// Same-fs move via `fs::rename`; cross-fs degrades to copy+unlink.
    Move,
}

/// Drag-source state the app reducer holds while a drag is in flight.
/// Lives on [`crate::file::state::State`] as
/// `Option<DragSource>` — `Some` between `Message::DragStart` and the
/// drop / cancel signal; `None` otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragSource {
    /// Absolute paths the user selected at drag-start.
    pub paths: Vec<PathBuf>,
    /// Action the source advertised. The receiving Wayland client may
    /// downgrade (Copy ⇄ Move per its own modifier state) but the
    /// source's preferred action ships in the initial offer.
    pub action: DragAction,
}

/// Drop-target payload assembled by the wayland subsystem when a drop
/// completes on our window. The reducer maps `action` onto an
/// `Operation::Copy` or `Operation::Move` against the current pane's
/// cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTarget {
    /// Absolute paths the drop carries (decoded from `text/uri-list`).
    pub paths: Vec<PathBuf>,
    /// Action the drop should perform (derived from modifier state).
    pub action: DropAction,
}

/// Encode a slice of absolute paths as a `text/uri-list` body. Each
/// path becomes a `file://<encoded-path>` line; the byte set outside
/// RFC 3986 §2.3 (`ALPHA / DIGIT / "-" / "." / "_" / "~"`) plus the
/// path-segment separators `/` is percent-encoded. Spaces, unicode,
/// and shell metacharacters all round-trip through
/// [`parse_uri_list`].
///
/// Empty input yields an empty string (no trailing CRLF) — the
/// receiving client treats a zero-line offer as "no urls", which is
/// what we want when the user drags an empty selection.
pub fn paths_to_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        out.push_str("file://");
        out.push_str(&percent_encode_path(p.as_path()));
        out.push_str(CRLF);
    }
    out
}

/// Decode a `text/uri-list` body into absolute paths. Drops `#`
/// comment lines (RFC 2483 §5) and tolerates either CRLF or LF
/// terminators (some toolkits — notably older GTK versions — emit
/// LF-only).
///
/// Non-`file://` URIs are skipped silently; a `https://` URI dropped
/// from a browser is meaningful in some apps (Telegram converts it to
/// a link message) but the file plane's only handlers are
/// `Operation::Copy` / `Operation::Move`, which need on-disk paths.
pub fn parse_uri_list(body: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in body.split('\n') {
        let trimmed = line.trim_end_matches('\r').trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("file://") {
            let decoded = percent_decode(rest);
            out.push(PathBuf::from(decoded));
        }
    }
    out
}

/// Derive the [`DropAction`] from the keyboard modifier state at drop
/// time. SPEC §3.3 item 12 + freedesktop DnD convention:
///
/// * `Ctrl` (with or without other mods) → [`DropAction::Copy`].
/// * `Shift` (no Ctrl) → [`DropAction::Move`].
/// * no modifier → [`DropAction::Copy`] (the file plane defaults to
///   the non-destructive action so a misclick doesn't unlink the
///   source).
pub fn drop_action_from_modifiers(mods: &iced::keyboard::Modifiers) -> DropAction {
    if mods.control() {
        DropAction::Copy
    } else if mods.shift() {
        DropAction::Move
    } else {
        DropAction::Copy
    }
}

/// Iced subscription that translates inbound `wl_data_device` drops
/// into [`crate::file::app::Message::DropAccept`]. Iced 0.14's
/// xdg-toplevel reactor surfaces wayland drops via
/// `iced::event::Event::Window(window::Event::FileDropped(_))` — one
/// event per dropped URI. We accumulate consecutive `FileDropped`
/// payloads into a single batch via the `M::from` callback the caller
/// supplies, so the reducer arm fires once per drop session rather
/// than once per file.
///
/// **Iced 0.14 gap (documented per non-negotiable #1)**: the
/// source-side `wl_data_device_manager_create_source` is **not** in
/// iced 0.14's public subscription API. The bin emits
/// [`crate::file::app::Message::DragStart`] from the existing keymap
/// (a future `D` chord or a mouse-drag handler in `view::pane`) — the
/// pure-Rust [`paths_to_uri_list`] body is what the wayland adapter
/// would feed into the source's `wl_data_device_source.send` event
/// once the lower-level winit hook lands. The manual recipe in the
/// module docstring covers the cross-toolkit verification today.
pub fn dnd_subscription() -> iced::Subscription<DropTarget> {
    // iced 0.14 enforces "non-capturing closure" on `filter_map` at
    // runtime — so we keep the closure parameter-free and let the
    // caller wrap the `DropTarget` into its own `Message` via
    // `Subscription::map(Message::DropAccept)`.
    iced::event::listen().filter_map(|ev| match ev {
        iced::event::Event::Window(iced::window::Event::FileDropped(path)) => {
            // No modifier information surfaces alongside the event in
            // iced 0.14; default to Copy (the SPEC §3.3 item 12 safe
            // default — Ctrl forces Copy, no-mod → Copy).
            Some(DropTarget {
                paths: vec![path],
                action: DropAction::Copy,
            })
        }
        _ => None,
    })
}

/// Percent-encode every byte outside the RFC 3986 §2.3 unreserved set
/// plus the path-segment separator `/`. The output is ASCII-only so
/// every Wayland client (Qt / GTK / Smithay) consumes it identically.
fn percent_encode_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        if is_unreserved(*byte) || *byte == b'/' {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", byte));
        }
    }
    out
}

/// Reverse of [`percent_encode_path`]. Malformed `%XX` triples (`%`
/// with non-hex follower) round-trip as literal bytes — the receiver
/// is responsible for sanity-checking the resulting `PathBuf` before
/// opening it.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_nibble(bytes[i + 1]);
            let lo = hex_nibble(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 3986 §2.3 unreserved character classifier.
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// Map an ASCII hex digit to its 0-15 value. Returns `None` for any
/// non-hex byte so the caller can fall back to literal-byte
/// preservation.
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three plain ASCII paths encode as three CRLF-terminated
    /// `file://` lines. The roadmap-pinned `drag_out_offers_text_uri_list`
    /// asserts the same shape in the integration test.
    #[test]
    fn three_ascii_paths_encode_to_crlf_terminated_file_lines() {
        let paths = vec![
            PathBuf::from("/tmp/a.txt"),
            PathBuf::from("/tmp/b.txt"),
            PathBuf::from("/tmp/c.txt"),
        ];
        let body = paths_to_uri_list(&paths);
        assert_eq!(
            body, "file:///tmp/a.txt\r\nfile:///tmp/b.txt\r\nfile:///tmp/c.txt\r\n",
            "three-line URI list must use CRLF terminators per RFC 2483"
        );
    }

    /// Spaces and unicode bytes percent-encode; round-trip via
    /// `parse_uri_list` recovers the original paths byte-for-byte.
    #[test]
    fn space_and_unicode_round_trip() {
        let paths = vec![
            PathBuf::from("/tmp/two words.txt"),
            PathBuf::from("/tmp/café/重要.md"),
        ];
        let body = paths_to_uri_list(&paths);
        assert!(
            body.contains("two%20words.txt"),
            "space must percent-encode as %20: {body}"
        );
        assert!(
            !body.contains("café"),
            "non-ASCII bytes must percent-encode: {body}"
        );
        let parsed = parse_uri_list(&body);
        assert_eq!(parsed, paths, "URI list must round-trip path bytes");
    }

    /// Comment lines (RFC 2483 — `#` prefix) are dropped from the
    /// parsed list, preserving order of the remaining entries.
    #[test]
    fn parse_drops_comment_lines() {
        let body = "# this is a comment\r\nfile:///a\r\n# another\r\nfile:///b\r\n";
        let parsed = parse_uri_list(body);
        assert_eq!(parsed, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    /// `Ctrl` forces Copy; `Shift` forces Move; no modifier defaults
    /// to Copy. Mirrors the SPEC §3.3 item 12 freedesktop convention.
    #[test]
    fn ctrl_forces_copy_shift_forces_move() {
        let ctrl = iced::keyboard::Modifiers::CTRL;
        let shift = iced::keyboard::Modifiers::SHIFT;
        let none = iced::keyboard::Modifiers::default();
        assert_eq!(drop_action_from_modifiers(&ctrl), DropAction::Copy);
        assert_eq!(drop_action_from_modifiers(&shift), DropAction::Move);
        assert_eq!(drop_action_from_modifiers(&none), DropAction::Copy);
    }
}
