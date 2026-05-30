//! Step 35 — cross-link existence test for the `sy file` documentation
//! surface (the two new how-tos and the two pre-existing references).
//!
//! Diátaxis layers refer to each other: the how-to to run `sy file`
//! cites both reference docs, the plugin-author how-to cites the PDK
//! example, and so on. This test extracts every Markdown link
//! `[text](path)` from the docs in scope and asserts the targets
//! resolve on disk (absolute http(s) URLs are skipped — lychee covers
//! those out-of-process per `Makefile::docs-lint`).
//!
//! The roadmap pins the test name as `all_cross_links_resolve`
//! (Step 35 — `tests:` entry).
//!
//! See [`docs/how-to/run-sy-file.md`](../docs/how-to/run-sy-file.md)
//! and [`docs/how-to/write-a-sy-plugin.md`](../docs/how-to/write-a-sy-plugin.md)
//! for the documents under test.

use std::path::{Path, PathBuf};

/// The four `sy file` documentation files Step 35 covers. The two
/// reference files (`sy-file-mcp.md`, `sy-file-doctor.md`) are
/// load-bearing prior art that Step 35 confirms still meets the
/// cross-link bar; the two how-tos (`run-sy-file.md`,
/// `write-a-sy-plugin.md`) ship in this step. Keeping them all in one
/// test surfaces a docs-drift regression (e.g. the canary moves but
/// the doctor reference still cites the old path) in a single rg.
const SY_FILE_DOCS: &[&str] = &[
    "docs/how-to/run-sy-file.md",
    "docs/how-to/write-a-sy-plugin.md",
    "docs/reference/sy-file-mcp.md",
    "docs/reference/sy-file-doctor.md",
];

/// Resolve the workspace root by walking up from `CARGO_MANIFEST_DIR`.
/// `cargo test` sets `CARGO_MANIFEST_DIR` to the crate root, which for
/// the `sy` bin is already the workspace root.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Extract every `[text](target)` Markdown link from `body`. Returns
/// only the `target` half — the text is decoration for human readers.
/// Uses a deliberately simple regex per the roadmap step's spec:
/// `\[([^\]]+)\]\(([^)]+)\)`. Images (`![alt](src)`) match the same
/// shape with a leading `!` and are intentionally included so a broken
/// image path is caught too.
fn extract_links(body: &str) -> Vec<String> {
    let re = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)")
        .expect("link-extraction regex must compile");
    re.captures_iter(body)
        .map(|c| c[2].to_string())
        .collect()
}

/// Strip a Markdown link fragment (`#anchor`) and any leading
/// whitespace introduced by reflowing. The on-disk file the link
/// targets is what matters; the anchor is resolved by the renderer.
fn strip_fragment(target: &str) -> &str {
    target.split_once('#').map(|(p, _)| p).unwrap_or(target).trim()
}

/// Skip rule: absolute URLs (http/https/mailto/file), bare anchors
/// (the link points inside the same doc), and empty strings. These
/// are out of scope for the on-disk existence test; lychee covers the
/// URL set in `make docs-lint`.
fn is_out_of_scope(target: &str) -> bool {
    target.is_empty()
        || target.starts_with('#')
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with("file://")
}

/// Resolve a relative link against the directory of the doc it lives
/// in. The Markdown convention is "relative to the file" — same as
/// every renderer (GitHub, Hugo, mkdocs) consumes.
fn resolve(doc_path: &Path, target: &str) -> PathBuf {
    let dir = doc_path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(target)
}

#[test]
fn all_cross_links_resolve() {
    let root = workspace_root();
    let mut missing: Vec<(String, String)> = Vec::new();
    for rel in SY_FILE_DOCS {
        let doc = root.join(rel);
        let body = std::fs::read_to_string(&doc)
            .unwrap_or_else(|e| panic!("step35 — read {}: {e}", doc.display()));
        for raw in extract_links(&body) {
            let target = strip_fragment(&raw);
            if is_out_of_scope(target) {
                continue;
            }
            let resolved = resolve(&doc, target);
            if !resolved.exists() {
                missing.push((rel.to_string(), target.to_string()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "step35 — broken on-disk cross-links: {missing:#?}"
    );
}
