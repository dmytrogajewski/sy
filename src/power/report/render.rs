//! PDF byte-emitter for the `sy power show` report (Step 35 + Step P2-2).
//!
//! The roadmap originally pinned `typst-pdf = "0.13"` for in-process PDF
//! assembly; the typst library API ships ~6 MB of bundled fonts and
//! requires a full `World` glue layer. We fall back to `pdf-writer`
//! (the same crate the typst project itself uses to produce its final
//! bytes) and author the report directly — see `Cargo.toml`'s workspace
//! dep comment for the full rationale.
//!
//! ## Contract
//!
//! [`compile_pdf`] is the public entry point. It takes a
//! [`super::template::ReportTemplate`] (the pure content layer) plus a
//! [`ReportMetrics`] bundle (so the [`Plot`] renderer can produce its
//! SVGs) and returns the raw PDF bytes — no I/O, no clock reads, no
//! temp files, no font assets on disk. The CLI ([`super::super::cli`])
//! is what writes those bytes to
//! `~/.local/state/sy/power/reports/sy-power-<rfc3339>.pdf`.
//!
//! ## Layout
//!
//! 15 pages, A4 portrait ([`PAGE_WIDTH_PT`] × [`PAGE_HEIGHT_PT`]):
//!
//! 1. Header + executive summary + methodology footer (page 1).
//! 2. Bandit text panel (page 2).
//! 3. Forecast text panel (page 3).
//! 4. Shield text panel (page 4).
//! 5. Energy text panel (page 5).
//! 6. Drift text panel (page 6).
//!
//! Pages 7-15: one per Step-34 [`Plot`] variant, in
//! [`super::plots::Plot::ALL`] declaration order, rasterised at
//! [`PLOT_RENDER_DPI`] via `resvg` and embedded as a PDF Image
//! XObject filling [`PLOT_TARGET_WIDTH_PT`] × [`PLOT_TARGET_HEIGHT_PT`]
//! of usable canvas under a small caption.
//!
//! ## Step P2-2 deviation
//!
//! Step 35 shipped the report as text-only because the typst→pdf-writer
//! substitution dropped the SVG embed path. The Step-34 plots remained
//! consumable from `--json` (Step 33 bundles them under `plots.*`) but
//! were absent from the PDF an operator opens. Step P2-2 closes that
//! gap by rasterising each [`Plot`] variant via `resvg` 0.45 (pure-Rust
//! `usvg` + `tiny-skia` already in tree via the plotters dep cluster)
//! into a DeviceRGB Image XObject; the SVG-string output of Step 34
//! stays the canonical source so the JSON path is byte-identical.

use pdf_writer::{Chunk, Content, Filter, Finish, Name, Pdf, Rect, Ref, Str};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{Options, Tree};

use super::plots::{Plot, ReportMetrics};
use super::template::ReportTemplate;

/// A4 portrait width in PDF points (1 pt = 1/72 in). 595 × 842 matches
/// the printpdf / typst defaults; chosen for evince + okular sanity.
const PAGE_WIDTH_PT: f32 = 595.0;
const PAGE_HEIGHT_PT: f32 = 842.0;
/// Left/right text margin in PDF points.
const TEXT_MARGIN_X: f32 = 60.0;
/// Top text margin in PDF points (distance from page top to first line).
const TEXT_MARGIN_TOP: f32 = 60.0;
/// Per-line vertical step in PDF points. 14 pt at 10 pt font is a
/// readable single-spaced layout that still fits ~50 lines per page.
const LINE_STEP_PT: f32 = 14.0;
/// Body font size in PDF points. Mirrors the roadmap's "Inter, 10pt".
const BODY_FONT_PT: f32 = 10.0;
/// Title font size in PDF points. Used once per page on the section
/// heading (e.g. "sy-power report", "Bandit").
const TITLE_FONT_PT: f32 = 16.0;
/// Target render DPI for the embedded plots. 150 DPI lands well above
/// the 72 DPI screen-fidelity floor (so on-screen zoom doesn't
/// pixellate) while keeping the per-image byte budget below the
/// FlateDecode-compressed ~150 KB-per-plot rough target.
const PLOT_RENDER_DPI: f32 = 150.0;
/// SVG canvas DPI plotters renders at. Matches plotters' SVGBackend
/// default; together with [`PLOT_RENDER_DPI`] gives the resvg pixmap
/// scale factor.
const PLOT_SOURCE_DPI: f32 = 96.0;
/// PDF-points width the embedded plot image occupies on its page.
/// A4 portrait minus the [`TEXT_MARGIN_X`] gutter on both sides.
const PLOT_TARGET_WIDTH_PT: f32 = PAGE_WIDTH_PT - 2.0 * TEXT_MARGIN_X;
/// PDF-points height the embedded plot image occupies. Matches the
/// plotters source canvas aspect ratio (600 × 400 = 3:2) so the
/// rasterised pixels arrive un-stretched.
const PLOT_TARGET_HEIGHT_PT: f32 = PLOT_TARGET_WIDTH_PT * 2.0 / 3.0;
/// Bytes per pixel in the embedded image stream. DeviceRGB at 8 bits
/// per component = 3 bytes per pixel.
const RGB_BYTES_PER_PIXEL: usize = 3;

/// Build the report PDF from a pre-assembled template plus the live
/// [`ReportMetrics`] bundle. Returns the raw bytes ready to write to
/// disk. Pure-fn: deterministic over the same template + metrics (both
/// are themselves deterministic over the audit window).
pub fn compile_pdf(template: &ReportTemplate, metrics: &ReportMetrics<'_>) -> Vec<u8> {
    let mut alloc = Ref::new(1);
    let mut pdf = Pdf::new();
    let catalog_id = alloc.bump();
    let page_tree_id = alloc.bump();
    let font_id = alloc.bump();
    let font_name = Name(b"F1");
    pdf.catalog(catalog_id).pages(page_tree_id);
    // Helvetica is one of the 14 PDF base fonts: no font data needs
    // embedding, every reader (evince / okular / Firefox / Chrome)
    // resolves it locally. The roadmap pins Inter but Inter requires
    // shipping a 200 KB+ TTF asset; the trade-off is documented in
    // the module preamble.
    pdf.type1_font(font_id).base_font(Name(b"Helvetica"));
    let mut page_ids = emit_pages(
        &mut pdf,
        &mut alloc,
        page_tree_id,
        font_id,
        font_name,
        template,
    );
    page_ids.extend(emit_plot_pages(
        &mut pdf,
        &mut alloc,
        page_tree_id,
        font_id,
        font_name,
        metrics,
    ));
    pdf.pages(page_tree_id)
        .kids(page_ids.iter().copied())
        .count(page_ids.len() as i32);
    pdf.finish()
}

/// Emit every page in the report and return their object ids. The
/// allocator carries forward across pages so each call to `alloc.bump`
/// yields a fresh PDF object id.
fn emit_pages(
    pdf: &mut Pdf,
    alloc: &mut Ref,
    page_tree_id: Ref,
    font_id: Ref,
    font_name: Name<'_>,
    template: &ReportTemplate,
) -> Vec<Ref> {
    // Tuple per page: section heading + body lines. Walking the
    // template's panels in declaration order produces the layout
    // documented at module top. Step P2-2 collapsed the dedicated
    // Methodology page into the header so the per-plot pages added
    // below land the total within the roadmap's 11-15 page band.
    let header_lines = header_text_lines(template);
    let pages: Vec<(&str, Vec<String>)> = vec![
        ("sy-power report", header_lines),
        ("Bandit", template.bandit_lines.clone()),
        ("Forecast", template.forecast_lines.clone()),
        ("Shield", template.shield_lines.clone()),
        ("Energy", template.energy_lines.clone()),
        ("Drift", template.drift_lines.clone()),
    ];
    let mut ids = Vec::with_capacity(pages.len());
    for (title, body) in pages {
        ids.push(emit_one_page(
            pdf,
            alloc,
            page_tree_id,
            font_id,
            font_name,
            title,
            &body,
        ));
    }
    ids
}

/// Render header + executive-summary + methodology footer into the
/// body lines of page 1. Step P2-2 collapsed the standalone Methodology
/// page into this single front-matter page so the 9 plot pages added
/// downstream land within the roadmap's 11-15 page band.
fn header_text_lines(template: &ReportTemplate) -> Vec<String> {
    let h = &template.header;
    let mut lines = vec![
        format!("Host: {}", h.host),
        format!("Generated: {}", h.generated_at_rfc3339),
        format!("Window: {:.2} days", h.window_days),
        format!("Model version: {}", h.model_version_sha),
        String::new(),
        "Executive summary".to_string(),
    ];
    for bullet in &template.exec_bullets {
        lines.push(format!("  - {bullet}"));
    }
    lines.push(String::new());
    lines.push("Methodology".to_string());
    for line in &template.methodology_lines {
        lines.push(format!("  {line}"));
    }
    lines
}

/// Emit one page with a title + body lines and return its id. The
/// content stream is a single text object that walks line by line from
/// the top margin down.
fn emit_one_page(
    pdf: &mut Pdf,
    alloc: &mut Ref,
    page_tree_id: Ref,
    font_id: Ref,
    font_name: Name<'_>,
    title: &str,
    body: &[String],
) -> Ref {
    let page_id = alloc.bump();
    let content_id = alloc.bump();
    let mut page = pdf.page(page_id);
    page.media_box(Rect::new(0.0, 0.0, PAGE_WIDTH_PT, PAGE_HEIGHT_PT));
    page.parent(page_tree_id);
    page.contents(content_id);
    page.resources().fonts().pair(font_name, font_id);
    page.finish();
    let mut content = Content::new();
    content.begin_text();
    content.set_font(font_name, TITLE_FONT_PT);
    content.next_line(TEXT_MARGIN_X, PAGE_HEIGHT_PT - TEXT_MARGIN_TOP);
    content.show(Str(sanitise(title).as_bytes()));
    content.set_font(font_name, BODY_FONT_PT);
    // Move down by one title-line step before the body.
    content.next_line(0.0, -LINE_STEP_PT * 1.5);
    for line in body {
        content.show(Str(sanitise(line).as_bytes()));
        content.next_line(0.0, -LINE_STEP_PT);
    }
    content.end_text();
    pdf.stream(content_id, &content.finish());
    page_id
}

/// One full-page panel per [`Plot`] variant. Each page carries a small
/// caption (the variant's debug name) plus the rasterised plot image
/// filling the gutter-bounded canvas under the caption. Order matches
/// [`Plot::ALL`] so a future variant addition lands at the end of the
/// PDF without re-shuffling existing pages.
fn emit_plot_pages(
    pdf: &mut Pdf,
    alloc: &mut Ref,
    page_tree_id: Ref,
    font_id: Ref,
    font_name: Name<'_>,
    metrics: &ReportMetrics<'_>,
) -> Vec<Ref> {
    let mut ids = Vec::with_capacity(Plot::ALL.len());
    for plot in Plot::ALL {
        let svg = plot.render(metrics);
        let rendered = rasterise_svg(&svg);
        ids.push(emit_plot_page(
            pdf,
            alloc,
            page_tree_id,
            font_id,
            font_name,
            plot,
            rendered.as_ref(),
        ));
    }
    ids
}

/// Rasterised plot image: DeviceRGB pixel buffer + pixmap dimensions.
/// Returned by [`rasterise_svg`] for embedding via [`emit_plot_page`].
struct RasterPlot {
    width_px: u32,
    height_px: u32,
    /// DeviceRGB stream payload (width × height × 3 bytes per pixel).
    /// FlateDecode-compressed when [`Self::flate_compressed`] is `true`;
    /// raw bytes otherwise. The two-mode design keeps the renderer
    /// no-panic on the (vanishingly unlikely) `flate2` OOM path while
    /// always producing a valid PDF image stream.
    rgb_stream: Vec<u8>,
    flate_compressed: bool,
}

/// Rasterise one Step-34 SVG document via `resvg` and zlib-compress the
/// resulting DeviceRGB pixel stream. Returns `None` (the caller falls
/// back to a caption-only page) when usvg refuses the SVG payload — the
/// `Plot::render` surface guarantees a well-formed SVG so this branch
/// only fires on a future regression.
fn rasterise_svg(svg: &str) -> Option<RasterPlot> {
    let tree = Tree::from_str(svg, &Options::default()).ok()?;
    let scale = PLOT_RENDER_DPI / PLOT_SOURCE_DPI;
    let src_size = tree.size().to_int_size();
    let target_w = ((src_size.width() as f32) * scale).round() as u32;
    let target_h = ((src_size.height() as f32) * scale).round() as u32;
    let mut pixmap = Pixmap::new(target_w.max(1), target_h.max(1))?;
    resvg::render(
        &tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let rgb = rgba_to_rgb_on_white(pixmap.data(), pixmap.width(), pixmap.height());
    let (rgb_stream, flate_compressed) = match zlib_compress(&rgb) {
        Some(z) => (z, true),
        None => (rgb, false),
    };
    Some(RasterPlot {
        width_px: pixmap.width(),
        height_px: pixmap.height(),
        rgb_stream,
        flate_compressed,
    })
}

/// Convert a tiny-skia RGBA byte stream to a DeviceRGB pixel buffer
/// composited against white. tiny-skia stores pixels straight-alpha so
/// we drop the alpha channel by alpha-compositing against white — the
/// report background — keeping the visible appearance identical to the
/// SVG renderer's white-canvas convention (`area.fill(&WHITE)` in
/// `plots::render_into_string`).
fn rgba_to_rgb_on_white(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let pixel_count = (width as usize) * (height as usize);
    let mut rgb = Vec::with_capacity(pixel_count * RGB_BYTES_PER_PIXEL);
    for px in rgba.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        if a == 0xff {
            rgb.extend_from_slice(&[r, g, b]);
        } else {
            // Composite against white. tiny-skia returns straight
            // alpha, so `out = (a*c + (255-a)*255) / 255`.
            let inv = 255u16 - a as u16;
            let blend = |c: u8| -> u8 { ((c as u16 * a as u16 + inv * 255) / 255) as u8 };
            rgb.extend_from_slice(&[blend(r), blend(g), blend(b)]);
        }
    }
    rgb
}

/// Zlib-compress the DeviceRGB pixel stream. PDF's `FlateDecode` filter
/// expects raw zlib (RFC 1950) wrapping a DEFLATE bitstream (RFC 1951);
/// `tiny-skia` already pulls `flate2` as a transitive dependency (via
/// `png`'s decoder chain) so the workspace-level `flate2` dep we
/// declare in `Cargo.toml` adds no compile cost. Falls back to the
/// uncompressed bytes on allocator OOM so the renderer stays no-panic
/// (the caller then drops the `/FlateDecode` filter pair — see the
/// degradation note in [`write_image_xobject`]).
fn zlib_compress(rgb: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(rgb).ok()?;
    encoder.finish().ok()
}

/// Emit one plot page: caption text at the top, full-width plot image
/// below. Returns the page object id so the page-tree walker can
/// register it. When `rendered` is `None` the page degrades to the
/// caption-only layout so a future SVG regression doesn't sink the
/// whole report.
fn emit_plot_page(
    pdf: &mut Pdf,
    alloc: &mut Ref,
    page_tree_id: Ref,
    font_id: Ref,
    font_name: Name<'_>,
    plot: &Plot,
    rendered: Option<&RasterPlot>,
) -> Ref {
    let page_id = alloc.bump();
    let content_id = alloc.bump();
    let image_id = alloc.bump();
    let image_name = Name(b"Im0");
    {
        let mut page = pdf.page(page_id);
        page.media_box(Rect::new(0.0, 0.0, PAGE_WIDTH_PT, PAGE_HEIGHT_PT));
        page.parent(page_tree_id);
        page.contents(content_id);
        let mut resources = page.resources();
        resources.fonts().pair(font_name, font_id);
        resources.x_objects().pair(image_name, image_id);
        resources.finish();
        page.finish();
    }
    let mut content = Content::new();
    let caption = format!("{plot:?}");
    content.begin_text();
    content.set_font(font_name, TITLE_FONT_PT);
    content.next_line(TEXT_MARGIN_X, PAGE_HEIGHT_PT - TEXT_MARGIN_TOP);
    content.show(Str(sanitise(&caption).as_bytes()));
    content.end_text();
    if rendered.is_some() {
        let image_y = PAGE_HEIGHT_PT - TEXT_MARGIN_TOP - LINE_STEP_PT * 2.0 - PLOT_TARGET_HEIGHT_PT;
        content.save_state();
        content.transform([
            PLOT_TARGET_WIDTH_PT,
            0.0,
            0.0,
            PLOT_TARGET_HEIGHT_PT,
            TEXT_MARGIN_X,
            image_y,
        ]);
        content.x_object(image_name);
        content.restore_state();
    }
    pdf.stream(content_id, &content.finish());
    // The Image XObject must exist even on the degraded path so the
    // page resources reference resolves; a 1×1 white pixel keeps the
    // PDF valid without polluting the visible page.
    write_image_xobject(pdf, image_id, rendered);
    page_id
}

/// Write the per-plot Image XObject. Either embeds the rasterised plot
/// or falls back to a 1×1 white pixel so the resource reference always
/// resolves to a well-formed image stream. The `flate_compressed` flag
/// drives whether the `/Filter /FlateDecode` pair appears in the stream
/// header — without that flag the bytes are written raw.
fn write_image_xobject(pdf: &mut Chunk, image_id: Ref, rendered: Option<&RasterPlot>) {
    const WHITE_PIXEL: [u8; RGB_BYTES_PER_PIXEL] = [0xff, 0xff, 0xff];
    let fallback_pixel;
    let (width, height, stream, flate) = match rendered {
        Some(r) => (
            r.width_px,
            r.height_px,
            r.rgb_stream.as_slice(),
            r.flate_compressed,
        ),
        None => {
            fallback_pixel = WHITE_PIXEL.to_vec();
            (1, 1, fallback_pixel.as_slice(), false)
        }
    };
    let mut image = pdf.image_xobject(image_id, stream);
    image.width(width as i32);
    image.height(height as i32);
    image.color_space().device_rgb();
    image.bits_per_component(8);
    if flate {
        image.filter(Filter::FlateDecode);
    }
    image.finish();
}

/// Strip control characters + non-ASCII so the Helvetica base font's
/// WinAnsi encoding renders cleanly. The PDF spec lets us embed UTF-8
/// behind a custom encoding, but the report's content is
/// fixed-vocabulary ASCII (numbers, panel names, the dot interpunct
/// `\u{00b7}` we sub for ` · `) so an ASCII coercion is enough.
fn sanitise(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else if c == '\u{00b7}' {
                '*'
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::report::metrics::{
        ActivityMetrics, BanditMetrics, DriftMetrics, EnergyMetrics, ForecastMetrics, ShieldMetrics,
    };
    use crate::power::report::plots::ReportMetrics;
    use crate::power::report::template::ReportHeader;
    use std::collections::HashMap;

    /// PDF magic bytes every conformant reader looks for at byte 0.
    /// Scoped to the test module because production code does not need
    /// to introspect the bytes — the operator's PDF viewer does.
    const PDF_MAGIC: &[u8] = b"%PDF-";

    /// Lower bound on a non-trivial PDF body. 1.5 KB easily clears the
    /// catalog + page tree + 7 pages of text overhead while staying
    /// honest about an empty-metrics edge case.
    const MIN_PDF_BYTES: usize = 1_500;
    /// Expected page count after Step P2-2's plot embed: header(+exec
    /// summary +methodology footer) + bandit + forecast + shield +
    /// energy + drift = 6 text pages + 9 plot pages = 15. Mirrors the
    /// SPEC §RV.2 panel set with the plots Step 34 produced, now lifted
    /// from `--json`-only into the PDF itself.
    const EXPECTED_PAGES: usize = 15;
    /// Lower bound on page count any "PDF embeds Step-34 plots" build
    /// must satisfy. Loose enough that a future text-panel re-layout
    /// (e.g. merging two short panels onto one page) doesn't trip the
    /// test, tight enough that "we forgot to add the plot pages"
    /// regressions trip immediately. Mirrors the roadmap's
    /// "11-15 pages" target band lower bound.
    const PLOT_EMBED_MIN_PAGES: usize = 11;
    /// Number of plot variants embedded. Pinned to `Plot::ALL.len()`
    /// rather than a magic 9 so a future plot addition only updates
    /// the enum.
    const EXPECTED_PLOT_PAGES: usize = crate::power::report::plots::Plot::ALL.len();

    fn empty_metrics_bundle() -> (
        BanditMetrics,
        ForecastMetrics,
        ShieldMetrics,
        EnergyMetrics,
        DriftMetrics,
        ActivityMetrics,
    ) {
        (
            BanditMetrics::default(),
            ForecastMetrics {
                accuracy_per_class: HashMap::new(),
                ..Default::default()
            },
            ShieldMetrics::default(),
            EnergyMetrics::default(),
            DriftMetrics::default(),
            ActivityMetrics::default(),
        )
    }

    /// Owned bundle: each metric struct lives here so a borrowed
    /// [`ReportMetrics`] can be re-assembled from `&FixtureBundle`
    /// without copy-pasting the empty-defaults dance into each test.
    struct FixtureBundle {
        bandit: BanditMetrics,
        forecast: ForecastMetrics,
        shield: ShieldMetrics,
        energy: EnergyMetrics,
        drift: DriftMetrics,
        activity: ActivityMetrics,
    }

    impl FixtureBundle {
        fn empty() -> Self {
            let (b, f, s, e, d, a) = empty_metrics_bundle();
            Self {
                bandit: b,
                forecast: f,
                shield: s,
                energy: e,
                drift: d,
                activity: a,
            }
        }

        fn metrics(&self) -> ReportMetrics<'_> {
            ReportMetrics {
                bandit: &self.bandit,
                forecast: &self.forecast,
                shield: &self.shield,
                energy: &self.energy,
                drift: &self.drift,
                activity: &self.activity,
                entries: &[],
            }
        }
    }

    fn fixture_template_and_metrics() -> (ReportTemplate, FixtureBundle) {
        let bundle = FixtureBundle::empty();
        let template = ReportTemplate::build(
            &bundle.metrics(),
            ReportHeader {
                host: "test-host".to_string(),
                generated_at_rfc3339: "2026-05-20T12:00:00Z".to_string(),
                window_days: 7.0,
                model_version_sha: "rules-baseline".to_string(),
            },
        );
        (template, bundle)
    }

    /// Roadmap test: golden fixture metrics → PDF bytes start with the
    /// `%PDF-` magic + are above the documented size floor.
    #[test]
    fn compiles_to_non_empty_pdf() {
        let (template, bundle) = fixture_template_and_metrics();
        let bytes = compile_pdf(&template, &bundle.metrics());
        assert!(
            bytes.starts_with(PDF_MAGIC),
            "PDF must start with magic bytes, got {:?}",
            &bytes[..bytes.len().min(8)],
        );
        assert!(
            bytes.len() > MIN_PDF_BYTES,
            "PDF must be > {MIN_PDF_BYTES} bytes, got {}",
            bytes.len(),
        );
    }

    /// Roadmap test: compiled PDF contains the expected page count
    /// (15 pages — 6 text panels + 9 plot panels per Step P2-2). We
    /// probe the page count via the `Count` entry in the page tree —
    /// pdf-writer emits it verbatim as `/Count 15`.
    #[test]
    fn pdf_round_trips_to_pages() {
        let (template, bundle) = fixture_template_and_metrics();
        let bytes = compile_pdf(&template, &bundle.metrics());
        let needle = format!("/Count {EXPECTED_PAGES}");
        let haystack = String::from_utf8_lossy(&bytes);
        assert!(
            haystack.contains(&needle),
            "PDF must encode {needle:?}; full PDF length: {}",
            bytes.len(),
        );
    }

    /// Roadmap Step P2-2 test: the compiled PDF must carry at least
    /// one PDF Image XObject for every Step-34 [`Plot`] variant. The
    /// `/Subtype /Image` pair is the canonical marker pdf-writer emits
    /// inside each `image_xobject` stream header; scanning the byte
    /// stream for that token gives a backend-agnostic count.
    #[test]
    fn pdf_contains_image_streams() {
        let (template, bundle) = fixture_template_and_metrics();
        let bytes = compile_pdf(&template, &bundle.metrics());
        let haystack = String::from_utf8_lossy(&bytes);
        let hits = haystack.matches("/Subtype /Image").count();
        assert!(
            hits >= EXPECTED_PLOT_PAGES,
            "PDF must embed >= {EXPECTED_PLOT_PAGES} image XObjects (one per Plot variant), \
             got {hits}; PDF length: {}",
            bytes.len(),
        );
    }

    /// Roadmap Step P2-2 test: the PDF grew from the 7-page Step-35
    /// text-only baseline to the 11-15-page banner with embedded plots.
    /// Asserts the lower bound so an accidental "plots were dropped"
    /// regression trips immediately.
    #[test]
    fn pdf_page_count_grew_to_match_panel_count() {
        let (template, bundle) = fixture_template_and_metrics();
        let bytes = compile_pdf(&template, &bundle.metrics());
        let haystack = String::from_utf8_lossy(&bytes);
        let count_idx = haystack.find("/Count ").unwrap_or_else(|| {
            panic!(
                "PDF must encode a `/Count N` page-tree entry; \
                 PDF length: {}",
                bytes.len()
            )
        });
        let tail = &haystack[count_idx + "/Count ".len()..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        let count: usize = digits
            .parse()
            .unwrap_or_else(|_| panic!("page tree `/Count {digits}` not numeric"));
        assert!(
            count >= PLOT_EMBED_MIN_PAGES,
            "PDF page count must be >= {PLOT_EMBED_MIN_PAGES} after plot embed, got {count}",
        );
    }
}
