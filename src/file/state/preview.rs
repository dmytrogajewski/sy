//! Preview-pane state slice. Roadmap Step 26 (SPEC §3.3 item 8).
//!
//! The previewer dispatcher in [`crate::file::view::preview`] (under
//! `gui-iced`) writes the path of the entry currently being previewed
//! here so the async image-load `Task` started by
//! `Message::HoverEntry` can be reconciled with the user's cursor when
//! `Message::PreviewLoaded` arrives. Without the keyed slot, a stale
//! preview that just finished decoding would overwrite the freshly
//! hovered one.
//!
//! Pure data, no I/O — so the type is **not** `gui-iced`-gated.
//! `--no-default-features` builds (CLI/MCP only) still see the field
//! on `State`; the dispatch + widget tree live behind the feature.
//!
//! Step 27 extended this slice with a plugin-routed text cache so
//! `Message::PreviewLoadedText` can stash the body it received from a
//! `host.preview.text` round-trip. The image arm continues to lean on
//! iced's GPU-side handle cache (keyed by path inside
//! `Handle::Bytes`'s id) for now; if a future step ships PNG bytes
//! through the bridge's `PreviewResult::Png` arm, the cache will grow
//! a third field at that point.

use std::path::PathBuf;

/// One coloured span inside a [`HighlightedLine`]. `fg` is an
/// `(r, g, b)` triple from the syntect theme; the view layer maps it
/// to an `iced::Color`. Pure data so the async preview resolver can
/// produce it off the UI thread and the cache can hold it without a
/// renderer dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightedSpan {
    pub fg: (u8, u8, u8),
    pub text: String,
}

/// One syntect-highlighted line — a row of coloured spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightedLine {
    pub spans: Vec<HighlightedSpan>,
}

/// Fully-resolved, paint-ready preview content. Produced off the UI
/// thread by the async preview resolver and cached on
/// [`PreviewState::resolved`] so the `view()` callback never touches
/// the filesystem or runs syntect on the render path — navigation
/// stays responsive regardless of file size or storage latency.
#[derive(Debug, Clone)]
pub enum PreviewPayload {
    /// Syntect-highlighted text body (already read + highlighted).
    Text(Vec<HighlightedLine>),
    /// Pre-formatted file-info card body (name / size / mtime / mime).
    Info(String),
    /// Image at the keyed path — iced caches the decoded handle by
    /// path inside its GPU pool, so the view paints `image::preview`
    /// without re-decoding.
    Image,
}

/// Per-frame preview-pane state. Step 26 introduced `current_path`;
/// Step 27 added `text_preview` so plugin-rendered text bodies (the
/// `BridgeError::PluginCrashed` fall-back path renders these via the
/// built-in syntect dispatcher; the plugin-success path stashes them
/// here so the view layer's text dispatcher can read them).
#[derive(Debug, Default, Clone)]
pub struct PreviewState {
    /// Path of the entry the user most recently hovered. Written from
    /// `Message::HoverEntry`; read by `Message::PreviewLoaded` to
    /// decide whether the late-arriving decoded payload still matches
    /// the cursor.
    pub current_path: Option<PathBuf>,
    /// Step 27 — last plugin-rendered text payload, keyed by the path
    /// it was rendered for. Cleared when the cursor moves to a
    /// different entry so a stale plugin text from yesterday's hover
    /// can't paint over today's. `None` when the most recent preview
    /// is not a text payload (or no preview has landed yet).
    pub text_preview: Option<(PathBuf, String)>,
    /// Async-resolved, paint-ready preview content keyed by the path
    /// it was resolved for. The view layer paints purely from this
    /// cache — when it's `None` or the path doesn't match the cursor,
    /// the view shows a cheap "loading…" affordance (no I/O). The
    /// resolver (`app::resolve_preview`) fills it off-thread on every
    /// cursor change.
    pub resolved: Option<(PathBuf, PreviewPayload)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh `PreviewState::default()` has no path so the dispatch
    /// layer can paint the "no built-in preview" fallback widget without
    /// branching on `Option::None` semantics.
    #[test]
    fn fresh_preview_state_has_no_current_path() {
        let s = PreviewState::default();
        assert!(s.current_path.is_none());
    }
}
