//! Step 12 scroll-seek test (DoD bullet 2).
//!
//! Renders the canonical sample at `scroll_skip = 0` and at
//! `scroll_skip = 10`, then asserts the two PNGs differ both in
//! byte-length (the scrolled image has fewer rows than the unscrolled
//! one) and in perceptual hash (different content visible). Together
//! that proves `preview/seek` is wired through the renderer rather
//! than ignored — the SPEC §4.2.4 `scroll_skip` field has a real
//! effect.

use std::path::PathBuf;
use sy_plugin_md::ahash::{hamming, hash_png, HAMMING_BUDGET};
use sy_plugin_md::render::{render_to_png, RenderOpts};

fn fixture_md() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/preview-sample.md");
    std::fs::read_to_string(p).expect("read preview-sample.md")
}

#[test]
fn render_with_scroll_differs_from_top() {
    let md = fixture_md();
    let top = render_to_png(
        &md,
        &RenderOpts {
            scroll_skip: 0,
            ..RenderOpts::default()
        },
    )
    .expect("render top");
    let scrolled = render_to_png(
        &md,
        &RenderOpts {
            scroll_skip: 10,
            ..RenderOpts::default()
        },
    )
    .expect("render scrolled");
    assert_ne!(
        top.len(),
        scrolled.len(),
        "scrolled PNG must differ in size from top render"
    );
    let h_top = hash_png(&top).expect("hash top");
    let h_scrolled = hash_png(&scrolled).expect("hash scrolled");
    let d = hamming(h_top, h_scrolled);
    assert!(
        d > HAMMING_BUDGET,
        "scroll=10 PNG should be perceptually distinct from scroll=0; \
         hamming={d}, budget={HAMMING_BUDGET}"
    );
}
