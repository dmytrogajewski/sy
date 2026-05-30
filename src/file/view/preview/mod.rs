//! Preview pane dispatcher. Roadmap Step 26 (SPEC §3.3 item 8).
//!
//! Routes by MIME:
//!
//! * `image/*` → [`image::preview`] (iced::widget::image, decoded
//!   off the runtime via [`image::load`]).
//! * `text/*` + `application/json` → [`text::preview`] (syntect-
//!   highlighted text spans, clamped at
//!   [`text::MAX_TEXT_PREVIEW_BYTES`]).
//! * anything else → a fallback container with the literal "no
//!   built-in preview" text. Step 27 will replace this branch with
//!   the plugin-routed dispatch via the `Registry`'s `(previewer,
//!   mime|url)` lookup.
//!
//! The module is `#[cfg(feature = "gui-iced")]`-gated because both
//! the image widget and the syntect text spans require iced's widget
//! tree. `--no-default-features` builds (CLI/MCP only) drop it
//! entirely; the pure-data
//! [`crate::file::state::PreviewState`] still rides on `State` so
//! IPC consumers can see the cursor's preview target without paying
//! for the renderer.
//!
//! ## Anti-chrome contract
//!
//! The SPEC §3.4 anti-goals row "no chrome" is a regression-guard
//! against the failed yazi md-rich experiment that motivated this
//! plane. The journey-J3 e2e in `tests/sy_file_preview_chrome_guard.rs`
//! `pgrep`-snapshots the process tree around a representative preview
//! render to assert the dispatcher never spawns a browser. Every
//! routing arm in [`preview`] below is therefore implemented in pure
//! Rust — no `std::process::Command`, no `xdg-open`, no external
//! renderer call.

pub mod image;
pub mod text;

use std::path::PathBuf;

use ::iced::widget::{container, text as iced_text};
use ::iced::{Element, Length};

use crate::file::app::Message;
use crate::file::fs::mime as fs_mime;
use crate::file::state::{Entry, EntryKind, PreviewPayload, State};

/// Coarse classification the dispatcher walks. `NoBuiltin` is the
/// Step 27 hand-off point — the variant is exposed today so its
/// presence is visible in the public API and the registry can be
/// wired against the same enum next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    /// Routed to [`image::preview`].
    Image,
    /// Routed to [`text::preview`].
    Text,
    /// No built-in preview. Step 27 will dispatch through the plugin
    /// `Registry`'s `(previewer, mime|url)` lookup; today the
    /// dispatcher paints a fallback container.
    NoBuiltin,
}

/// MIME → [`PreviewKind`] pure projection. Read by [`preview`] below
/// and by the unit tests so a future MIME-routing change is forced
/// through one site.
pub fn kind_for(mime: &str) -> PreviewKind {
    if mime.starts_with("image/") {
        PreviewKind::Image
    } else if mime.starts_with("text/") || mime == "application/json" {
        PreviewKind::Text
    } else {
        PreviewKind::NoBuiltin
    }
}

/// Resolve the MIME for an entry under the previewer dispatch
/// contract: prefer the cached `Entry::mime_hint` if Step 19's
/// `fs::walk` already filled it (avoids re-stat-ing the file every
/// frame), otherwise fall back to the full extension-then-sniff
/// ladder via [`fs_mime::mime_for`]. Errors degrade to
/// `application/octet-stream` so the dispatcher never panics on a
/// malformed entry.
pub fn mime_for_entry(entry: &Entry, path: &std::path::Path) -> String {
    if let Some(hint) = entry.mime_hint.as_deref() {
        return hint.to_string();
    }
    fs_mime::mime_for(path).unwrap_or_else(|_| "application/octet-stream".to_string())
}

/// Build the full preview-pane Element for the entry under the
/// pane's cursor. The dispatcher consumes the `State` reference to
/// access the cached image handle (under
/// `state.preview` — Step 26's pure-data slice) and the entry to
/// resolve its MIME.
///
/// Step 27 rewrites the `NoBuiltin` branch: if a plugin already
/// rendered a text payload for the current path, paint it directly;
/// otherwise paint a "rendering…" status while the bridge's
/// async task is in flight. The `app::update` reducer is the only
/// site that actually drives the plugin call.
pub fn preview<'a>(state: &'a State, entry: &'a Entry, path: PathBuf) -> Element<'a, Message> {
    // Directories don't get a previewer — they get a child-listing
    // pane (Step 14's `Panes::preview`), which is composed by the
    // Step 24 view::root above this layer. The dispatcher only ever
    // sees files; bail to the fallback if something else slips
    // through.
    if !matches!(entry.kind, EntryKind::File | EntryKind::Symlink) {
        return fallback_panel("no built-in preview");
    }
    // Paint ONLY from the async-resolved cache (`app::resolve_preview`
    // did the file I/O + syntect off the UI thread). When the cache
    // matches the cursor's path, render it; otherwise paint a cheap
    // "loading…" affordance — the render path never blocks navigation
    // on disk I/O.
    if let Some((cached_path, payload)) = state.preview.resolved.as_ref() {
        if cached_path == &path {
            return match payload {
                PreviewPayload::Text(lines) => text::render_lines(lines),
                PreviewPayload::Info(body) => info_card(body),
                PreviewPayload::Image => image::preview(state, &path),
            };
        }
    }
    // Plugin-routed text (Step 27) may have landed in `text_preview`
    // before the generic resolver ran; honour it so a crashed-plugin
    // fallback still paints.
    if let Some((cached_path, _body)) = state.preview.text_preview.as_ref() {
        if cached_path == &path {
            return render_no_builtin(state, &path);
        }
    }
    fallback_panel("loading…")
}

/// Step 27 `NoBuiltin` branch — pull the most recent plugin payload
/// off `state.preview.text_preview`. When present and matching the
/// current path, render it through the same syntect dispatcher the
/// `Text` arm uses (no duplicate text engine). When absent, paint
/// the "rendering…" status so the user sees a stable affordance
/// while the async bridge call is in flight. The Step 27 DoD
/// `plugin_crash_falls_back_to_built_in_text` is honoured here: the
/// `Message::PreviewFailed` reducer clears the text slot, so the
/// next paint walks through this branch and falls back via
/// [`text::preview`] (which reads the raw file body).
fn render_no_builtin<'a>(state: &'a State, path: &std::path::Path) -> Element<'a, Message> {
    if let Some((cached_path, body)) = state.preview.text_preview.as_ref() {
        if cached_path == path {
            // The reducer stashed a plugin-rendered text body for this
            // path. Paint the cached body directly — no re-read, no
            // syntect on the render path. (The plugin already produced
            // the final text; we don't re-highlight it.)
            return info_card(body);
        }
    }
    // No payload — cheap loading affordance (the async resolver will
    // fill `state.preview.resolved` shortly). No I/O on the render
    // path.
    fallback_panel("loading…")
}

/// Render a pre-formatted file-info body into the preview pane. Pure
/// — the formatting (which does the `stat` I/O) happens off the UI
/// thread in [`format_file_info`], called by `app::resolve_preview`.
fn info_card<'a>(body: &str) -> Element<'a, Message> {
    container(iced_text(body.to_string()))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(12)
        .into()
}

/// Format the file-info card body (name / size / mtime / MIME /
/// path). Performs synchronous `stat` + MIME-sniff I/O, so it MUST be
/// called off the UI thread (the async preview resolver wraps it in a
/// `Task`). Returns the pre-formatted string the view's [`info_card`]
/// paints without any further I/O.
pub fn format_file_info(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let (size, mtime) = std::fs::metadata(path)
        .map(|m| {
            let size = m.len();
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (size, mtime)
        })
        .unwrap_or((0, 0));
    let mime = fs_mime::mime_for(path).unwrap_or_else(|_| "application/octet-stream".into());
    format!(
        "{}\n\nsize:  {}\nmtime: {}\nmime:  {}\npath:  {}",
        name,
        humanise_bytes(size),
        format_unix_time(mtime),
        mime,
        path.display(),
    )
}

fn humanise_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

fn format_unix_time(secs: u64) -> String {
    // Lightweight: "<secs> seconds ago" is enough for the info card;
    // a full strftime needs an extra dep. Tests pin this to a
    // human-readable shape; the journey doesn't require an exact
    // format.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now > secs {
        let delta = now - secs;
        if delta < 60 {
            format!("{}s ago", delta)
        } else if delta < 3600 {
            format!("{}m ago", delta / 60)
        } else if delta < 86400 {
            format!("{}h ago", delta / 3600)
        } else {
            format!("{}d ago", delta / 86400)
        }
    } else {
        format!("ts={}", secs)
    }
}

/// Step 26 `NoBuiltin` fallback widget. Pure-Rust container with the
/// literal text the SPEC §3.3 item 8 doc-string pins ("Anything else
/// dispatches to a plugin"). Step 27 keeps it as the in-flight /
/// no-bridge affordance.
fn fallback_panel<'a>(label: &'static str) -> Element<'a, Message> {
    container(iced_text(label))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(8)
        .into()
}

/// Warm the syntect + cosmic-text caches at app boot so the
/// journey-J3 "first byte after warm cache" perf budget isn't
/// poisoned by cold-start grammar parsing. Called once from
/// `app::run` before the iced reactor starts (and from the headless
/// harness for symmetry).
///
/// The warmup loads the bundled `SyntaxSet` + `ThemeSet` into the
/// process-global `OnceLock`s the [`text`] module owns. Idempotent:
/// the second call is a no-op (the `OnceLock`s short-circuit).
pub fn warm_caches() {
    text::warm_syntect();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Image MIMEs route to the image arm. The journey-J3 hover
    /// budget is asserted against this branch.
    #[test]
    fn kind_for_image_jpeg_routes_to_image() {
        assert_eq!(kind_for("image/jpeg"), PreviewKind::Image);
        assert_eq!(kind_for("image/png"), PreviewKind::Image);
    }

    /// `text/*` and `application/json` (the SPEC §3.3 item 8 text
    /// previewer scope) route to the text arm.
    #[test]
    fn kind_for_text_md_routes_to_text() {
        assert_eq!(kind_for("text/markdown"), PreviewKind::Text);
        assert_eq!(kind_for("text/plain"), PreviewKind::Text);
        assert_eq!(kind_for("application/json"), PreviewKind::Text);
    }

    /// PDF, binary, video, archive MIMEs land on the Step 27 plugin
    /// hand-off (today: the fallback). Pinning the negative case
    /// prevents a future change from accidentally routing PDFs
    /// through the text previewer.
    #[test]
    fn kind_for_pdf_is_no_builtin() {
        assert_eq!(kind_for("application/pdf"), PreviewKind::NoBuiltin);
        assert_eq!(kind_for("video/mp4"), PreviewKind::NoBuiltin);
        assert_eq!(kind_for("application/octet-stream"), PreviewKind::NoBuiltin);
    }

    /// `mime_for_entry` honours the cached `Entry::mime_hint` (Step
    /// 19) so the dispatcher doesn't re-stat the file every frame.
    #[test]
    fn mime_for_entry_prefers_cached_hint() {
        use crate::file::state::EntryId;
        let entry = Entry {
            id: 7 as EntryId,
            name: "doesnt-matter".into(),
            kind: EntryKind::File,
            size: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            is_symlink: false,
            broken_link: false,
            readable: true,
            mime_hint: Some("image/jpeg".to_string()),
            symlink_target: None,
        };
        // A non-existent path — proves `mime_for_entry` short-
        // circuits on the hint before touching the filesystem.
        let resolved = mime_for_entry(&entry, std::path::Path::new("/this/does/not/exist.bin"));
        assert_eq!(resolved, "image/jpeg");
    }
}
