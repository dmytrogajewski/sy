//! Regenerate the golden PNG fixtures the Step 12 perceptual-diff
//! tests compare against. Run with:
//!
//! ```bash
//! cargo run -p sy-plugin-md --example regen_goldens --release
//! ```
//!
//! Two goldens are produced:
//!
//! * `crates/sy-plugin-md/tests/fixtures/preview-sample.golden.png` —
//!   the canonical preview-sample fixture; locks the crate's own
//!   `render_canonical` / `render_scroll` tests.
//! * `tests/fixtures/sy-plugin-md-readme.golden.png` — this repo's
//!   `README.md`; locks the journey-Step12 E2E pixel contract.
//!
//! Re-run whenever style.toml, the font stack, or the cosmic-text
//! version moves. Commit the resulting PNGs alongside the code change
//! that bumped them.

use std::path::PathBuf;
use sy_plugin_md::render::{render_to_png, RenderOpts};

fn main() -> Result<(), String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| "repo root not found".to_string())?
        .to_path_buf();

    let sample_md = std::fs::read_to_string(manifest.join("tests/fixtures/preview-sample.md"))
        .map_err(|e| format!("read fixture: {e}"))?;
    let png = render_to_png(&sample_md, &RenderOpts::default())?;
    let sample_dst = manifest.join("tests/fixtures/preview-sample.golden.png");
    std::fs::write(&sample_dst, &png).map_err(|e| format!("write {sample_dst:?}: {e}"))?;
    eprintln!("wrote {} ({} bytes)", sample_dst.display(), png.len());

    let readme = std::fs::read_to_string(repo_root.join("README.md"))
        .map_err(|e| format!("read README: {e}"))?;
    let png_readme = render_to_png(&readme, &RenderOpts::default())?;
    let readme_dst = repo_root.join("tests/fixtures/sy-plugin-md-readme.golden.png");
    if let Some(parent) = readme_dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {parent:?}: {e}"))?;
    }
    std::fs::write(&readme_dst, &png_readme).map_err(|e| format!("write {readme_dst:?}: {e}"))?;
    eprintln!(
        "wrote {} ({} bytes)",
        readme_dst.display(),
        png_readme.len()
    );
    Ok(())
}
