//! Step 12 canonical render test (DoD bullet 1).
//!
//! Renders the in-tree `preview-sample.md` fixture and asserts the
//! resulting PNG perceptually matches the committed golden — Hamming
//! distance ≤ [`sy_plugin_md::ahash::HAMMING_BUDGET`] on the 64-bit
//! aHash. Regenerate with:
//!
//! ```bash
//! cargo run -p sy-plugin-md --example regen_goldens --release
//! ```

use std::path::PathBuf;
use sy_plugin_md::ahash::{hamming, hash_png, HAMMING_BUDGET};
use sy_plugin_md::render::{render_to_png, RenderOpts};

/// Fixture root next to this test file. `CARGO_MANIFEST_DIR` resolves
/// to `crates/sy-plugin-md`, and we walk into `tests/fixtures/`.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn render_canonical_matches_golden() {
    let dir = fixtures_dir();
    let md = std::fs::read_to_string(dir.join("preview-sample.md"))
        .expect("read preview-sample.md fixture");
    let png = render_to_png(&md, &RenderOpts::default()).expect("render fixture");
    // PNG magic bytes — the DoD's "valid PNG" probe.
    assert_eq!(
        &png[..8],
        b"\x89PNG\r\n\x1a\n",
        "rendered output is not a PNG"
    );
    let golden = std::fs::read(dir.join("preview-sample.golden.png")).expect(
        "read golden PNG; run `cargo run -p sy-plugin-md --example regen_goldens --release`",
    );
    let h_now = hash_png(&png).expect("hash candidate");
    let h_golden = hash_png(&golden).expect("hash golden");
    let d = hamming(h_now, h_golden);
    assert!(
        d <= HAMMING_BUDGET,
        "perceptual hash drifted: distance={d} budget={HAMMING_BUDGET} \
         (golden={h_golden:#018x} now={h_now:#018x})"
    );
}
