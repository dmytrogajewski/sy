//! Markdown → PNG rendering pipeline.
//!
//! Stages:
//!
//! 1. Parse markdown via `pulldown_cmark::Parser` into a flat list of
//!    [`Block`]s. Each block carries its kind (heading level, paragraph,
//!    code, list, blockquote, rule, image) and inline spans.
//! 2. Lay each block out into a `cosmic_text::Buffer` with the right
//!    font size + colour for its style.
//! 3. Measure total height, allocate a `tiny_skia::Pixmap` of the
//!    pinned content width × total height, fill with the gruvbox
//!    background, and rasterise each block at its computed y offset.
//! 4. Encode the pixmap as PNG via `Pixmap::encode_png` and return the
//!    bytes.
//!
//! The pipeline is fully synchronous + side-effect-free: same input
//! markdown + same [`RenderOpts`] = same PNG bytes, every time. That's
//! the contract the perceptual-hash tests rely on.

use std::sync::OnceLock;

use cosmic_text::{
    fontdb, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight, Wrap,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use tiny_skia::Pixmap;

/// Gruvbox-dark palette pinned for the canary. Mirrors
/// `crates/sy-plugin-md/style.toml` and `themes/gruvbox-material.toml`.
pub mod palette {
    /// Background fill for the page.
    pub const BG: (u8, u8, u8) = (0x28, 0x28, 0x28);
    /// Primary foreground text.
    pub const FG: (u8, u8, u8) = (0xeb, 0xdb, 0xb2);
    /// Dim foreground (blockquotes, captions).
    pub const FG_DIM: (u8, u8, u8) = (0xa8, 0x99, 0x84);
    /// Accent (headings, separators).
    pub const ACCENT: (u8, u8, u8) = (0x89, 0xb4, 0x82);
    /// Link colour.
    pub const LINK: (u8, u8, u8) = (0x7d, 0xae, 0xa3);
    /// Inline / fenced code foreground.
    pub const CODE_FG: (u8, u8, u8) = (0xa9, 0xb6, 0x65);
    /// Code block background.
    pub const CODE_BG: (u8, u8, u8) = (0x32, 0x30, 0x2f);
    /// Horizontal-rule colour.
    pub const RULE: (u8, u8, u8) = (0x50, 0x49, 0x45);
}

/// Pinned layout constants. Mirrors `style.toml`.
pub const CONTENT_WIDTH_PX: u32 = 800;
pub const MARGIN_PX: u32 = 32;
pub const BODY_FONT_PX: f32 = 16.0;
pub const CODE_FONT_PX: f32 = 14.0;
pub const LINE_HEIGHT_SCALE: f32 = 1.4;
pub const PARAGRAPH_SPACING_PX: f32 = 8.0;
/// One scroll "unit" in pixels — SPEC §4.2.4 `scroll_skip` is in
/// lines, so a unit ≈ body font × line height ≈ 22 px; round to 24
/// to match `style.toml`.
pub const SCROLL_LINE_PX: u32 = 24;

/// Caller-facing options for [`render_to_png`].
#[derive(Debug, Clone)]
pub struct RenderOpts {
    /// Logical scroll skip in "lines" — SPEC §4.2.4. `0` means
    /// "render from the top".
    pub scroll_skip: u32,
    /// Page width budget. The canary always renders at the pinned
    /// 800 px; callers can shrink for fixture-specific tests.
    pub width_px: u32,
    /// Maximum rendered height (after scroll). Defaults to a tall
    /// strip so the entire document is visible on hover.
    pub max_height_px: u32,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            scroll_skip: 0,
            width_px: CONTENT_WIDTH_PX,
            max_height_px: 4096,
        }
    }
}

/// Lazy global FontSystem — `cosmic_text::FontSystem::new` scans the
/// system font catalogue and is slow even in release. We instantiate
/// once with the bundled DejaVu fonts so the renderer is hermetic
/// regardless of the host's font setup.
fn font_system() -> std::sync::MutexGuard<'static, FontSystem> {
    static SYS: OnceLock<std::sync::Mutex<FontSystem>> = OnceLock::new();
    SYS.get_or_init(|| {
        // Pure in-process fonts — same blob every time so the glyph
        // cache stays byte-stable across runs.
        let regular = include_bytes!("../fonts/DejaVuSans.ttf").to_vec();
        let bold = include_bytes!("../fonts/DejaVuSans-Bold.ttf").to_vec();
        let sources = vec![
            fontdb::Source::Binary(std::sync::Arc::new(regular)),
            fontdb::Source::Binary(std::sync::Arc::new(bold)),
        ];
        let mut sys = FontSystem::new_with_fonts(sources);
        sys.db_mut().set_sans_serif_family("DejaVu Sans");
        sys.db_mut().set_serif_family("DejaVu Sans");
        sys.db_mut().set_monospace_family("DejaVu Sans");
        std::sync::Mutex::new(sys)
    })
    .lock()
    .expect("font system mutex poisoned")
}

/// Block-level layout classification.
#[derive(Debug, Clone)]
enum Block {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(Vec<InlineSpan>),
    Code(String),
    ListItem {
        ordinal: Option<usize>,
        text: Vec<InlineSpan>,
    },
    Quote(Vec<InlineSpan>),
    Rule,
    /// Inline image placeholder — we lay out the alt-text as a dim
    /// labelled box rather than decoding the image inline. Markdown
    /// previews this canary handles never need to fetch external
    /// images (manifest's `network = []`); the placeholder is the
    /// honest fallback.
    Image {
        alt: String,
    },
}

/// Inline run within a paragraph / list item / blockquote.
#[derive(Debug, Clone)]
struct InlineSpan {
    text: String,
    style: InlineStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineStyle {
    Plain,
    Bold,
    Code,
    Link,
}

/// Parse a markdown body into a `Vec<Block>` ready for layout.
fn parse_blocks(md: &str) -> Vec<Block> {
    let parser = Parser::new_ext(md, Options::all());
    let mut blocks: Vec<Block> = Vec::new();
    let mut inlines: Vec<InlineSpan> = Vec::new();
    let mut style_stack: Vec<InlineStyle> = vec![InlineStyle::Plain];
    let mut in_code_block = false;
    let mut code_buf = String::new();
    let mut heading_level: Option<u8> = None;
    let mut heading_text = String::new();
    let mut in_list_item = false;
    let mut list_ordinal: Option<usize> = None;
    let mut in_blockquote = false;
    let mut in_image = false;
    let mut image_alt = String::new();

    for ev in parser {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                heading_level = Some(heading_level_to_u8(level));
                heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(lvl) = heading_level.take() {
                    blocks.push(Block::Heading {
                        level: lvl,
                        text: std::mem::take(&mut heading_text),
                    });
                }
            }
            Event::Start(Tag::Paragraph) => {
                inlines.clear();
            }
            Event::End(TagEnd::Paragraph) => {
                if in_blockquote {
                    blocks.push(Block::Quote(std::mem::take(&mut inlines)));
                } else if !in_list_item {
                    blocks.push(Block::Paragraph(std::mem::take(&mut inlines)));
                }
            }
            Event::Start(Tag::CodeBlock(_kind)) => {
                in_code_block = true;
                code_buf.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                let body = code_buf.trim_end_matches('\n').to_string();
                blocks.push(Block::Code(body));
                code_buf.clear();
            }
            Event::Start(Tag::List(start)) => {
                list_ordinal = start.map(|s| s as usize);
            }
            Event::End(TagEnd::List(_)) => {
                list_ordinal = None;
            }
            Event::Start(Tag::Item) => {
                in_list_item = true;
                inlines.clear();
            }
            Event::End(TagEnd::Item) => {
                in_list_item = false;
                let ordinal = list_ordinal.map(|o| {
                    let here = o;
                    list_ordinal = Some(o + 1);
                    here
                });
                blocks.push(Block::ListItem {
                    ordinal,
                    text: std::mem::take(&mut inlines),
                });
            }
            Event::Start(Tag::BlockQuote) => {
                in_blockquote = true;
            }
            Event::End(TagEnd::BlockQuote) => {
                in_blockquote = false;
            }
            Event::Start(Tag::Emphasis) | Event::Start(Tag::Strong) => {
                style_stack.push(InlineStyle::Bold);
            }
            Event::End(TagEnd::Emphasis) | Event::End(TagEnd::Strong) => {
                style_stack.pop();
            }
            Event::Start(Tag::Link { .. }) => style_stack.push(InlineStyle::Link),
            Event::End(TagEnd::Link) => {
                style_stack.pop();
            }
            Event::Start(Tag::Image { .. }) => {
                in_image = true;
                image_alt.clear();
            }
            Event::End(TagEnd::Image) => {
                in_image = false;
                blocks.push(Block::Image {
                    alt: std::mem::take(&mut image_alt),
                });
            }
            Event::Text(t) => {
                if in_image {
                    image_alt.push_str(&t);
                    continue;
                }
                let style = *style_stack.last().unwrap_or(&InlineStyle::Plain);
                if in_code_block {
                    code_buf.push_str(&t);
                } else if heading_level.is_some() {
                    heading_text.push_str(&t);
                } else {
                    inlines.push(InlineSpan {
                        text: t.into_string(),
                        style,
                    });
                }
            }
            Event::Code(c) => {
                if heading_level.is_some() {
                    heading_text.push_str(&c);
                } else if in_image {
                    image_alt.push_str(&c);
                } else {
                    inlines.push(InlineSpan {
                        text: c.into_string(),
                        style: InlineStyle::Code,
                    });
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code_block {
                    code_buf.push('\n');
                } else if heading_level.is_some() {
                    heading_text.push(' ');
                } else if !in_image {
                    let style = *style_stack.last().unwrap_or(&InlineStyle::Plain);
                    inlines.push(InlineSpan {
                        text: " ".to_string(),
                        style,
                    });
                }
            }
            Event::Rule => {
                blocks.push(Block::Rule);
            }
            _ => {}
        }
    }
    blocks
}

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Pixel metrics + paint hints for a block.
struct BlockMetrics {
    metrics: Metrics,
    fg: (u8, u8, u8),
    bg: Option<(u8, u8, u8)>,
    indent_px: f32,
    pad_top: f32,
    pad_bottom: f32,
}

fn metrics_for_heading(level: u8) -> Metrics {
    let size = match level {
        1 => 28.0,
        2 => 24.0,
        3 => 20.0,
        _ => 18.0,
    };
    Metrics::new(size, size * LINE_HEIGHT_SCALE)
}

fn body_metrics() -> Metrics {
    Metrics::new(BODY_FONT_PX, BODY_FONT_PX * LINE_HEIGHT_SCALE)
}

fn code_metrics() -> Metrics {
    Metrics::new(CODE_FONT_PX, CODE_FONT_PX * LINE_HEIGHT_SCALE)
}

fn block_metrics(b: &Block) -> BlockMetrics {
    match b {
        Block::Heading { level, .. } => BlockMetrics {
            metrics: metrics_for_heading(*level),
            fg: palette::ACCENT,
            bg: None,
            indent_px: 0.0,
            pad_top: 12.0,
            pad_bottom: 4.0,
        },
        Block::Paragraph(_) => BlockMetrics {
            metrics: body_metrics(),
            fg: palette::FG,
            bg: None,
            indent_px: 0.0,
            pad_top: 0.0,
            pad_bottom: PARAGRAPH_SPACING_PX,
        },
        Block::Code(_) => BlockMetrics {
            metrics: code_metrics(),
            fg: palette::CODE_FG,
            bg: Some(palette::CODE_BG),
            indent_px: 8.0,
            pad_top: 6.0,
            pad_bottom: 10.0,
        },
        Block::ListItem { .. } => BlockMetrics {
            metrics: body_metrics(),
            fg: palette::FG,
            bg: None,
            indent_px: 24.0,
            pad_top: 0.0,
            pad_bottom: 2.0,
        },
        Block::Quote(_) => BlockMetrics {
            metrics: body_metrics(),
            fg: palette::FG_DIM,
            bg: None,
            indent_px: 18.0,
            pad_top: 4.0,
            pad_bottom: PARAGRAPH_SPACING_PX,
        },
        Block::Rule => BlockMetrics {
            metrics: body_metrics(),
            fg: palette::RULE,
            bg: None,
            indent_px: 0.0,
            pad_top: 6.0,
            pad_bottom: 6.0,
        },
        Block::Image { .. } => BlockMetrics {
            metrics: body_metrics(),
            fg: palette::FG_DIM,
            bg: Some(palette::CODE_BG),
            indent_px: 8.0,
            pad_top: 4.0,
            pad_bottom: PARAGRAPH_SPACING_PX,
        },
    }
}

/// Build cosmic-text rich-text spans for a block.
fn block_text<'a>(
    b: &'a Block,
    family_sans: &'a Family<'a>,
    family_mono: &'a Family<'a>,
) -> Vec<(&'a str, Attrs<'a>)> {
    fn attrs_for_style<'a>(
        style: InlineStyle,
        base_fg: (u8, u8, u8),
        family_sans: &'a Family<'a>,
        family_mono: &'a Family<'a>,
    ) -> Attrs<'a> {
        match style {
            InlineStyle::Plain => Attrs::new()
                .family(*family_sans)
                .color(rgb_to_color(base_fg)),
            InlineStyle::Bold => Attrs::new()
                .family(*family_sans)
                .weight(Weight::BOLD)
                .color(rgb_to_color(base_fg)),
            InlineStyle::Code => Attrs::new()
                .family(*family_mono)
                .color(rgb_to_color(palette::CODE_FG)),
            InlineStyle::Link => Attrs::new()
                .family(*family_sans)
                .color(rgb_to_color(palette::LINK)),
        }
    }
    match b {
        Block::Heading { text, .. } => vec![(
            text.as_str(),
            Attrs::new()
                .family(*family_sans)
                .weight(Weight::BOLD)
                .color(rgb_to_color(palette::ACCENT)),
        )],
        Block::Paragraph(spans) | Block::Quote(spans) => spans
            .iter()
            .map(|s| {
                (
                    s.text.as_str(),
                    attrs_for_style(s.style, palette::FG, family_sans, family_mono),
                )
            })
            .collect(),
        Block::ListItem { text, .. } => text
            .iter()
            .map(|s| {
                (
                    s.text.as_str(),
                    attrs_for_style(s.style, palette::FG, family_sans, family_mono),
                )
            })
            .collect(),
        Block::Code(body) => vec![(
            body.as_str(),
            Attrs::new()
                .family(*family_mono)
                .color(rgb_to_color(palette::CODE_FG)),
        )],
        Block::Rule => vec![(
            " ",
            Attrs::new()
                .family(*family_sans)
                .color(rgb_to_color(palette::RULE)),
        )],
        Block::Image { alt } => vec![(
            alt.as_str(),
            Attrs::new()
                .family(*family_sans)
                .color(rgb_to_color(palette::FG_DIM)),
        )],
    }
}

fn rgb_to_color((r, g, b): (u8, u8, u8)) -> Color {
    Color::rgb(r, g, b)
}

/// Visible height of a laid-out buffer.
fn buffer_height(buf: &Buffer) -> f32 {
    let mut max_y = 0.0_f32;
    for run in buf.layout_runs() {
        let y_end = run.line_top + run.line_height;
        if y_end > max_y {
            max_y = y_end;
        }
    }
    max_y
}

/// Render markdown to a PNG byte stream.
pub fn render_to_png(md: &str, opts: &RenderOpts) -> Result<Vec<u8>, String> {
    let pixmap = render_to_pixmap(md, opts)?;
    pixmap
        .encode_png()
        .map_err(|e| format!("render::encode_png: {e}"))
}

/// Same as [`render_to_png`] but yields the in-memory pixmap.
pub fn render_to_pixmap(md: &str, opts: &RenderOpts) -> Result<Pixmap, String> {
    let blocks = parse_blocks(md);
    let mut sys = font_system();
    let mut cache = SwashCache::new();
    let content_width = opts.width_px.saturating_sub(MARGIN_PX * 2).max(64);
    let family_sans = Family::Name("DejaVu Sans");
    let family_mono = Family::Name("DejaVu Sans");

    // Pass 1: lay each block out and remember its measured height so
    // we can size the pixmap exactly. Each Buffer keeps its shaped
    // lines so we reuse it for the draw pass.
    let mut laid: Vec<(Block, BlockMetrics, Buffer, f32)> = Vec::new();
    let mut total_height: f32 = MARGIN_PX as f32;
    for b in blocks {
        let bm = block_metrics(&b);
        let mut buf = Buffer::new(&mut sys, bm.metrics);
        buf.set_wrap(&mut sys, Wrap::WordOrGlyph);
        buf.set_size(
            &mut sys,
            Some((content_width as f32 - bm.indent_px).max(32.0)),
            None,
        );
        let spans = block_text(&b, &family_sans, &family_mono);
        let default_attrs = Attrs::new().family(family_sans).color(rgb_to_color(bm.fg));
        buf.set_rich_text(
            &mut sys,
            spans.iter().map(|(t, a)| (*t, a.clone())),
            &default_attrs,
            Shaping::Advanced,
            None,
        );
        buf.shape_until_scroll(&mut sys, false);
        let h = buffer_height(&buf).max(bm.metrics.line_height);
        total_height += bm.pad_top + h + bm.pad_bottom;
        laid.push((b, bm, buf, h));
    }
    total_height += MARGIN_PX as f32;
    let total_height = total_height.max((MARGIN_PX * 2 + BODY_FONT_PX as u32 * 2) as f32) as u32;

    // Apply scroll. The pixmap is the visible window.
    let scroll_y = opts.scroll_skip.saturating_mul(SCROLL_LINE_PX);
    let visible_height = total_height
        .saturating_sub(scroll_y)
        .min(opts.max_height_px)
        .max(BODY_FONT_PX as u32 * 2);

    let mut pix = Pixmap::new(opts.width_px, visible_height).ok_or_else(|| {
        format!(
            "render::Pixmap::new({}, {}) failed",
            opts.width_px, visible_height
        )
    })?;
    pix.fill(tiny_skia::Color::from_rgba8(
        palette::BG.0,
        palette::BG.1,
        palette::BG.2,
        255,
    ));

    // Pass 2: draw each block at its computed y, minus the scroll.
    let mut y_cursor: f32 = MARGIN_PX as f32;
    for (block, bm, buf, _h) in &laid {
        y_cursor += bm.pad_top;
        let block_top = y_cursor - scroll_y as f32;
        let block_h = buffer_height(buf);

        // Block background fill (code / image placeholder).
        if let Some(bg) = bm.bg {
            fill_rect(
                &mut pix,
                MARGIN_PX as f32 + bm.indent_px - 4.0,
                block_top - 4.0,
                content_width as f32 - bm.indent_px + 8.0,
                block_h + 8.0,
                bg,
            );
        }
        // True horizontal rule — stroke a crisp line instead of
        // shaping the "─" Unicode glyph (which would antialias and
        // drift across font versions).
        if matches!(block, Block::Rule) {
            fill_rect(
                &mut pix,
                MARGIN_PX as f32,
                block_top + bm.metrics.line_height * 0.5,
                content_width as f32,
                1.5,
                palette::RULE,
            );
        }
        // List bullet (• or ordinal). Hanging indent style.
        if let Block::ListItem { ordinal, .. } = block {
            let marker = ordinal
                .map(|o| format!("{o}."))
                .unwrap_or_else(|| "•".to_string());
            draw_marker(
                &mut pix,
                &mut sys,
                &mut cache,
                MarkerSpec {
                    text: &marker,
                    x: MARGIN_PX as f32 + 4.0,
                    y: block_top,
                    metrics: bm.metrics,
                    fg: bm.fg,
                },
            );
        }
        let indent_x = MARGIN_PX as f32 + bm.indent_px;
        let block_top_i = block_top as i32;
        buf.draw(
            &mut sys,
            &mut cache,
            rgb_to_color(bm.fg),
            |gx, gy, gw, gh, color| {
                blit_glyph(
                    &mut pix,
                    gx + indent_x as i32,
                    gy + block_top_i,
                    gw,
                    gh,
                    color,
                );
            },
        );
        y_cursor += block_h + bm.pad_bottom;
    }
    Ok(pix)
}

/// Fill an axis-aligned rectangle of `colour` into `pix`. Direct
/// per-pixel writes (premultiplied RGBA, opaque) skip the
/// `Paint`/`PathBuilder` allocations a tiny-skia fill_rect would do.
fn fill_rect(pix: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c: (u8, u8, u8)) {
    let px = pix.width() as i32;
    let py = pix.height() as i32;
    let x0 = x.max(0.0) as i32;
    let y0 = y.max(0.0) as i32;
    let x1 = (x + w).min(px as f32) as i32;
    let y1 = (y + h).min(py as f32) as i32;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let stride = (pix.width() * 4) as usize;
    let data = pix.data_mut();
    for yy in y0..y1 {
        for xx in x0..x1 {
            let i = (yy as usize) * stride + (xx as usize) * 4;
            data[i] = c.0;
            data[i + 1] = c.1;
            data[i + 2] = c.2;
            data[i + 3] = 255;
        }
    }
}

/// Anchor + metrics for a [`draw_marker`] call. Bundled to keep the
/// function under clippy's `too_many_arguments` ceiling (8 is over,
/// 7 max).
struct MarkerSpec<'a> {
    text: &'a str,
    x: f32,
    y: f32,
    metrics: Metrics,
    fg: (u8, u8, u8),
}

/// Draw a single short text marker (bullet / ordinal) at the given
/// anchor. Used for list bullets so the body buffer doesn't carry
/// them (which would otherwise wrap with the body and lose the
/// hanging-indent alignment).
fn draw_marker(
    pix: &mut Pixmap,
    sys: &mut FontSystem,
    cache: &mut SwashCache,
    spec: MarkerSpec<'_>,
) {
    let mut buf = Buffer::new(sys, spec.metrics);
    buf.set_size(sys, Some(48.0), Some(spec.metrics.line_height));
    buf.set_text(
        sys,
        spec.text,
        &Attrs::new()
            .family(Family::Name("DejaVu Sans"))
            .color(rgb_to_color(spec.fg)),
        Shaping::Advanced,
        None,
    );
    buf.shape_until_scroll(sys, false);
    buf.draw(
        sys,
        cache,
        rgb_to_color(spec.fg),
        |gx, gy, gw, gh, color| {
            blit_glyph(pix, gx + spec.x as i32, gy + spec.y as i32, gw, gh, color);
        },
    );
}

/// Blit a single cosmic-text glyph tile into the pixmap with alpha
/// compositing. cosmic-text 0.15 passes (x, y, w, h, color) where
/// `color` already encodes the post-alpha glyph colour; treat its
/// alpha channel as coverage and src-over composite.
fn blit_glyph(pix: &mut Pixmap, x: i32, y: i32, w: u32, h: u32, color: Color) {
    let pw = pix.width() as i32;
    let ph = pix.height() as i32;
    if x >= pw || y >= ph || (x + w as i32) <= 0 || (y + h as i32) <= 0 {
        return;
    }
    let (r, g, b, a) = (color.r(), color.g(), color.b(), color.a());
    if a == 0 {
        return;
    }
    let stride = (pix.width() * 4) as usize;
    let data = pix.data_mut();
    for ty in 0..(h as i32) {
        let dy = y + ty;
        if dy < 0 || dy >= ph {
            continue;
        }
        for tx in 0..(w as i32) {
            let dx = x + tx;
            if dx < 0 || dx >= pw {
                continue;
            }
            let i = (dy as usize) * stride + (dx as usize) * 4;
            let af = a as u32;
            let inv = 255 - af;
            data[i] = ((r as u32 * af + data[i] as u32 * inv) / 255) as u8;
            data[i + 1] = ((g as u32 * af + data[i + 1] as u32 * inv) / 255) as u8;
            data[i + 2] = ((b as u32 * af + data[i + 2] as u32 * inv) / 255) as u8;
            data[i + 3] = (data[i + 3] as u32 + af).min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest markdown body renders to a non-empty PNG with valid
    /// magic bytes.
    #[test]
    fn renders_hello_world_to_valid_png() {
        let png = render_to_png("# Hello\n\nworld.", &RenderOpts::default()).expect("render");
        assert!(png.len() > 100, "PNG body too small: {}", png.len());
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    /// Scrolling a 1-line document by 100 units does not panic.
    #[test]
    fn over_scroll_does_not_panic() {
        let opts = RenderOpts {
            scroll_skip: 100,
            ..RenderOpts::default()
        };
        let png = render_to_png("only one line", &opts).expect("render");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    /// Two identical renders must hash equal — proves the glyph cache
    /// stays reproducible across calls (no time / random in the loop).
    #[test]
    fn deterministic_across_two_calls() {
        let body = "# Same\n\nbody.";
        let p1 = render_to_pixmap(body, &RenderOpts::default()).unwrap();
        let p2 = render_to_pixmap(body, &RenderOpts::default()).unwrap();
        let h1 = crate::ahash::hash_pixmap(&p1);
        let h2 = crate::ahash::hash_pixmap(&p2);
        assert_eq!(crate::ahash::hamming(h1, h2), 0);
    }
}
