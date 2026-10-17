//! Nerd-Font glyph map for pane row icons. Roadmap Step 24.
//!
//! Pure function `mime -> char`. The caller (Step 24's `view::pane`)
//! draws the returned glyph in the icon column left of the entry name.
//! Kept as a `char` (not an `iced::widget::Image`) because the
//! gruvbox-shaped `JetBrainsMono Nerd Font` already bundles the
//! private-use-area code points the SPEC §3.3 row 3 nominates; routing
//! through a text widget reuses the same font shaping the rest of the
//! pane chrome already pays for.
//!
//! ## Lookup ladder
//!
//! 1. Match the type-/category prefix first — every `image/*` maps to
//!    the same picture glyph regardless of subformat, ditto
//!    `video/*` / `audio/*`. This keeps the table small and the SPEC
//!    §3.3 row 3 "icons by category" contract honest.
//! 2. Then match a small allowlist of canonical full mimes (markdown,
//!    pdf, archive variants). The full mime takes precedence over the
//!    type-prefix fallback below it.
//! 3. Default to the generic file glyph; `inode/directory` is the only
//!    non-`*/*` mime callers reliably hand us so it gets a dedicated
//!    folder glyph.
//!
//! Glyphs are encoded as `\u{...}` escapes so the source file stays
//! grep-able and diff-friendly on terminals without a Nerd Font
//! installed; the runtime cost is zero (`char` literals collapse at
//! compile time).

/// Nerd Font folder glyph (nf-fa-folder, U+F07B). Drawn left of every
/// directory row in the pane.
pub const GLYPH_FOLDER: char = '\u{f07b}';
/// Nerd Font generic file glyph (nf-fa-file, U+F15B). Drawn left of
/// every row whose mime doesn't match a more-specific entry.
pub const GLYPH_FILE: char = '\u{f15b}';
/// Nerd Font picture/image glyph (nf-fa-file_image_o, U+F1C5). All
/// `image/*` mimes route here so journey-J2's `.png` thumbnails render
/// with a recognisable affordance.
pub const GLYPH_PICTURE: char = '\u{f1c5}';
/// Nerd Font markdown glyph (nf-md-language_markdown, U+F0354). Used
/// for `text/markdown` previewer entries (journey-J3 hover target).
pub const GLYPH_MARKDOWN: char = '\u{f0354}';
/// Nerd Font PDF glyph (nf-fa-file_pdf_o, U+F1C1).
pub const GLYPH_PDF: char = '\u{f1c1}';
/// Nerd Font text glyph (nf-fa-file_text_o, U+F15C). `text/plain` and
/// other unspecialised text formats.
pub const GLYPH_TEXT: char = '\u{f15c}';
/// Nerd Font video glyph (nf-fa-file_video_o, U+F1C8). All `video/*`.
pub const GLYPH_VIDEO: char = '\u{f1c8}';
/// Nerd Font audio glyph (nf-fa-file_audio_o, U+F1C7). All `audio/*`.
pub const GLYPH_AUDIO: char = '\u{f1c7}';
/// Nerd Font archive glyph (nf-fa-file_archive_o, U+F1C6).
pub const GLYPH_ARCHIVE: char = '\u{f1c6}';

/// Resolve a mime string to a single Nerd Font glyph. Pure function —
/// no I/O, no font lookups, no panics on invalid input (an empty / odd
/// mime falls through to [`GLYPH_FILE`]). The journey-J2 pane render
/// reaches for this once per row at paint time so the function must
/// stay cheap (one `split('/')` + a handful of string compares).
pub fn icon_for(mime: &str) -> char {
    // 1. Full-mime allowlist (most specific first).
    match mime {
        "inode/directory" => return GLYPH_FOLDER,
        "text/markdown" => return GLYPH_MARKDOWN,
        "application/pdf" => return GLYPH_PDF,
        "application/zip"
        | "application/x-tar"
        | "application/gzip"
        | "application/x-7z-compressed"
        | "application/x-bzip2"
        | "application/x-xz" => return GLYPH_ARCHIVE,
        _ => {}
    }
    // 2. Type-prefix fallback.
    match mime.split('/').next().unwrap_or("") {
        "image" => GLYPH_PICTURE,
        "video" => GLYPH_VIDEO,
        "audio" => GLYPH_AUDIO,
        "text" => GLYPH_TEXT,
        _ => GLYPH_FILE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roadmap Step 24 pin: `image/png` maps to the picture glyph.
    /// Journey-J2 lists `.png` files in the current pane and the icon
    /// column has to carry the right affordance for the user to scan
    /// the list quickly.
    #[test]
    fn png_resolves_to_picture_glyph() {
        assert_eq!(icon_for("image/png"), GLYPH_PICTURE);
    }

    /// Every `image/*` (not just png) hits the picture glyph. Locks the
    /// type-prefix ladder in so a future refactor can't accidentally
    /// regress the subformats `image/jpeg`, `image/webp`, etc.
    #[test]
    fn image_prefix_resolves_to_picture_glyph() {
        for m in ["image/jpeg", "image/webp", "image/gif", "image/svg+xml"] {
            assert_eq!(icon_for(m), GLYPH_PICTURE, "{m} must map to picture");
        }
    }

    /// Markdown wins over the `text/*` prefix fallback so the
    /// journey-J3 preview target gets its dedicated glyph.
    #[test]
    fn markdown_wins_over_text_prefix() {
        assert_eq!(icon_for("text/markdown"), GLYPH_MARKDOWN);
        // text/plain still falls through to the prefix.
        assert_eq!(icon_for("text/plain"), GLYPH_TEXT);
    }

    /// Directories hit the folder glyph; the only `inode/*` mime the
    /// pane builder hands us.
    #[test]
    fn directory_resolves_to_folder_glyph() {
        assert_eq!(icon_for("inode/directory"), GLYPH_FOLDER);
    }

    /// Unknown / empty mimes degrade gracefully to the generic file
    /// glyph instead of panicking — defensive against `Entry::mime_hint
    /// = None` becoming an empty string downstream.
    #[test]
    fn unknown_mime_falls_back_to_generic_file() {
        assert_eq!(icon_for(""), GLYPH_FILE);
        assert_eq!(icon_for("application/x-not-a-real-mime"), GLYPH_FILE);
    }
}
