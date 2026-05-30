//! Text-file previewer. Roadmap Step 26 (SPEC §3.3 item 8).
//!
//! Reads at most [`MAX_TEXT_PREVIEW_BYTES`] from the candidate file,
//! syntect-highlights every line under the language picked from the
//! file extension (or `Plain Text` when the bundled grammar set
//! doesn't know the extension), then composes an iced [`column!`] of
//! coloured [`iced::widget::text`] spans.
//!
//! ## Cold-start hazard
//!
//! `syntect`'s default `SyntaxSet::load_defaults_newlines()` decodes
//! the bundled `.sublime-syntax` files via flate2 + bincode on the
//! first call — ~30 ms on a warm SSD. The roadmap Step 26 Risks block
//! flags this against the cosmic-text shaper hazard
//! [`super::warm_caches`] addresses; this module owns the warmup via
//! [`warm_syntect`], which `super::warm_caches` calls from `app::run`
//! at boot. The journey-J3 perf budget assertion only applies after
//! the warmup completes.
//!
//! ## Oversize clamp
//!
//! Per the Step 26 DoD ("oversize text doesn't OOM"), the previewer
//! refuses to read more than [`MAX_TEXT_PREVIEW_BYTES`]. The clamp
//! ladder is:
//!
//! 1. Open the file with `File::open` (does not allocate the body).
//! 2. `Read::take(MAX_TEXT_PREVIEW_BYTES)` truncates the read at the
//!    boundary regardless of the file's real size.
//! 3. The truncated buffer feeds the syntect highlighter.
//!
//! A 64 MiB file therefore allocates exactly
//! [`MAX_TEXT_PREVIEW_BYTES`] bytes inside the previewer — the
//! `oversize_text_clamps_to_max_height` test pins this invariant.

use std::path::Path;
use std::sync::OnceLock;

use ::iced::widget::{column as iced_column, scrollable, text as iced_text};
use ::iced::{Color, Element, Length};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::file::app::Message;

/// Hard cap on the preview's input window. 64 KiB is large enough to
/// cover the head of any source file the user is likely to actually
/// read in a previewer pane (a "give me a feel for the file" affordance,
/// not a full editor) and small enough that the worst case fits
/// comfortably inside a single allocator slab. Public so the tests
/// (and Step 27's plugin-routed dispatch) can pin the same constant.
pub const MAX_TEXT_PREVIEW_BYTES: usize = 64 * 1024;

/// Bundled syntect syntax set. Populated by [`warm_syntect`] (or by
/// the first highlight call). `OnceLock` so cold start is paid at
/// most once per process — the journey-J3 perf budget rides on this.
static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
/// Bundled syntect theme set. Same `OnceLock` warmup contract as
/// [`SYNTAX_SET`].
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();
/// Theme name we pick out of the bundled set. `base16-ocean.dark` is
/// gruvbox-adjacent (the file plane's chosen palette) and ships with
/// the `default-themes` feature.
const DEFAULT_THEME_NAME: &str = "base16-ocean.dark";

/// Warm the bundled syntect caches. Called from
/// [`super::warm_caches`] at app boot so the journey-J3 first-byte
/// perf budget is measured against a warm process. Idempotent: the
/// second call returns immediately on the `OnceLock` shortcut.
pub fn warm_syntect() {
    let _ = syntax_set();
    let _ = theme_set();
}

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

// `HighlightedLine` / `HighlightedSpan` are pure data and live on the
// state slice ([`crate::file::state::preview`]) so the async preview
// resolver can produce them off the UI thread and the cache can hold
// them without a renderer dependency. Re-exported here for the
// existing test + integration-test call sites.
pub use crate::file::state::{HighlightedLine, HighlightedSpan};

/// Paint pre-resolved highlighted lines into a scrollable preview
/// pane. Pure — no I/O, no syntect. The live window calls this from
/// `view()` with the lines `app::resolve_preview` cached off-thread.
pub fn render_lines<'a>(lines: &[HighlightedLine]) -> Element<'a, Message> {
    let rendered = lines.iter().cloned().map(line_to_element).collect::<Vec<_>>();
    let body = iced_column(rendered).spacing(1).width(Length::Fill);
    let scrolled = scrollable(body).width(Length::Fill).height(Length::Fill);
    ::iced::widget::container(scrolled)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(6)
        .into()
}

/// Async producer: read + highlight the file off the UI thread. The
/// reducer wraps this in a `Task::perform` so navigation never blocks
/// on file I/O or syntect. Returns the highlighted lines (empty on
/// read error — the caller paints the file-info fallback then).
pub fn highlight_path(path: &Path) -> Vec<HighlightedLine> {
    highlight_lines(path)
}

/// Highlight a path's first [`MAX_TEXT_PREVIEW_BYTES`] of body.
/// Production [`preview`] and the test / integration-test surfaces
/// both route through this single entry point so the highlighter
/// logic has one site to evolve under future grammar additions.
fn highlight_lines(path: &Path) -> Vec<HighlightedLine> {
    let body = match read_clamped(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let ss = syntax_set();
    let ts = theme_set();
    let theme = match ts.themes.get(DEFAULT_THEME_NAME) {
        Some(t) => t,
        None => return plain_lines(&body),
    };
    let syntax = pick_syntax_for(ss, path);
    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(&body) {
        let regions: Vec<(Style, &str)> = match h.highlight_line(line, ss) {
            Ok(r) => r,
            Err(_) => return plain_lines(&body),
        };
        out.push(HighlightedLine {
            spans: regions
                .into_iter()
                .map(|(style, text)| HighlightedSpan {
                    fg: (style.foreground.r, style.foreground.g, style.foreground.b),
                    text: text.to_string(),
                })
                .collect(),
        });
    }
    out
}


/// Read at most [`MAX_TEXT_PREVIEW_BYTES`] from `path` and decode as
/// UTF-8 (lossy on non-UTF-8 input so binary-leaning files still
/// preview legibly). Returns the truncated string, or `Err` only on
/// open / read I/O errors.
fn read_clamped(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(MAX_TEXT_PREVIEW_BYTES);
    f.by_ref()
        .take(MAX_TEXT_PREVIEW_BYTES as u64)
        .read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Pick the syntect syntax for `path`. Extension match first, then
/// the plaintext fallback so non-matching files still render in the
/// "default" foreground colour (rather than panicking on
/// `find_syntax_by_extension(None)`).
fn pick_syntax_for<'a>(ss: &'a SyntaxSet, path: &Path) -> &'a syntect::parsing::SyntaxReference {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    ss.find_syntax_by_extension(ext)
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

/// Fallback when the theme lookup fails — emit one span per line
/// painted in the theme's default ink. Keeps the previewer from
/// flat-failing on a corrupt theme bundle.
fn plain_lines(body: &str) -> Vec<HighlightedLine> {
    LinesWithEndings::from(body)
        .map(|l| HighlightedLine {
            spans: vec![HighlightedSpan {
                fg: PLAIN_INK,
                text: l.to_string(),
            }],
        })
        .collect()
}

/// Default ink for the [`plain_lines`] fallback. Matches the
/// gruvbox-dark `ink` slot (235, 219, 178); used only when the
/// bundled `base16-ocean.dark` theme can't be loaded.
const PLAIN_INK: (u8, u8, u8) = (235, 219, 178);

/// Render one highlighted line into a row of coloured `text` widgets.
fn line_to_element<'a>(line: HighlightedLine) -> Element<'a, Message> {
    let spans = line.spans.into_iter().map(|s| {
        iced_text(s.text)
            .color(Color {
                r: s.fg.0 as f32 / 255.0,
                g: s.fg.1 as f32 / 255.0,
                b: s.fg.2 as f32 / 255.0,
                a: 1.0,
            })
            .into()
    });
    ::iced::widget::row(spans).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// `MAX_TEXT_PREVIEW_BYTES` is the public constant the journey
    /// J3 perf budget + the e2e clamp test both read. Pinning it
    /// here prevents an accidental shrink that would silently break
    /// the previewer's "first KiB is enough" affordance.
    #[test]
    fn max_text_preview_bytes_is_64_kib() {
        assert_eq!(MAX_TEXT_PREVIEW_BYTES, 64 * 1024);
    }

    /// Roadmap pin: `text_md_uses_syntect_not_plain`.
    ///
    /// Writes a tiny markdown fixture, then calls
    /// [`highlight_lines_for_test`] (the documented escape hatch
    /// surface — iced 0.14 has no public span introspection on
    /// `Element`). Asserts the syntect highlighter produces at
    /// least one span whose foreground colour is non-default — i.e.
    /// the markdown heading is painted in a different colour to the
    /// surrounding text, which is the literal "syntect is wired,
    /// not a plain-text fallback" signal.
    #[test]
    fn text_md_uses_syntect_not_plain() {
        warm_syntect();
        let tmp = tempfile::tempdir().expect("tempdir");
        let md = tmp.path().join("sample.md");
        std::fs::write(&md, "# Heading\n**bold** _italic_\n").expect("write md");

        let lines = highlight_path(&md);
        assert!(
            !lines.is_empty(),
            "syntect must emit at least one highlighted line for the fixture"
        );
        // Gather distinct foreground colours across all spans. A
        // pure plain-text fallback would emit exactly one colour
        // (the theme's default ink); syntect-coloured markdown
        // emits at least two (heading vs. body, bold-emphasis vs.
        // surrounding text).
        let mut distinct = std::collections::BTreeSet::new();
        for line in &lines {
            for span in &line.spans {
                distinct.insert(span.fg);
            }
        }
        assert!(
            distinct.len() >= 2,
            "syntect must emit at least two distinct foreground colours \
             across the heading + bold/italic spans; got {distinct:?}"
        );
    }

    /// Roadmap pin: `oversize_text_clamps_to_max_height`.
    ///
    /// Writes a 64 MiB text file, then calls
    /// [`highlight_lines_for_test`]. Asserts:
    ///
    /// 1. The call returns without OOM.
    /// 2. The total span text under all lines is at most
    ///    [`MAX_TEXT_PREVIEW_BYTES`] — the clamp landed.
    /// 3. The call returns in less than one second on the dev box
    ///    (a 64 MiB file would take >> 1 s if the clamp leaked).
    ///
    /// The 64 MiB size matches the roadmap brief verbatim
    /// (`"x".repeat(64 * 1024 * 1024)`). 1 MiB would be enough to
    /// prove the truncation but wouldn't catch a regression that
    /// changed the read shape to "read whole, then truncate" — the
    /// 64 MiB body is the load-bearing assertion.
    #[test]
    fn oversize_text_clamps_to_max_height() {
        const OVERSIZE_BYTES: usize = 64 * 1024 * 1024;
        const CLAMP_WALLCLOCK_BUDGET: Duration = Duration::from_secs(1);

        let tmp = tempfile::tempdir().expect("tempdir");
        let huge = tmp.path().join("oversize.txt");
        // Pre-allocate then write; `set_len` is cheaper than
        // streaming 64 MiB through stdlib's BufWriter, and the file
        // body content doesn't matter — only its size does.
        let f = std::fs::File::create(&huge).expect("create oversize");
        f.set_len(OVERSIZE_BYTES as u64).expect("set_len 64 MiB");
        drop(f);

        warm_syntect();
        let start = Instant::now();
        let lines = highlight_path(&huge);
        let elapsed = start.elapsed();

        assert!(
            elapsed < CLAMP_WALLCLOCK_BUDGET,
            "oversize clamp must complete inside {CLAMP_WALLCLOCK_BUDGET:?}; took {elapsed:?}. \
             A leaking clamp would take seconds to read 64 MiB."
        );
        let total_bytes: usize = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.text.len())
            .sum();
        assert!(
            total_bytes <= MAX_TEXT_PREVIEW_BYTES,
            "previewer must clamp at MAX_TEXT_PREVIEW_BYTES ({MAX_TEXT_PREVIEW_BYTES}); \
             read {total_bytes} bytes"
        );
    }
}
