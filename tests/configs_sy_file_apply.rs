//! Roadmap Step 34 — `sy apply` walks `configs/**` and renders every
//! text file through minijinja with the active theme's `{{ … }}`
//! bindings (`{{ home }}` plus the theme's `colors.*` / `ui.*`
//! tables). This test pins the three new `configs/sy/file*.toml`
//! files (`file.toml`, `file-keymap.toml`, `file-theme.toml`) and
//! asserts the same render loop the production `apply()` runs writes
//! them under a tempdir target with `{{ home }}` expanded.
//!
//! We don't shell out to `sy apply` — the apply machinery lives
//! inside the bin and is not exposed as a library function. Instead
//! we mirror the production code's contract: walk `configs/sy/`,
//! render every UTF-8 file through `minijinja::Environment::render_str`
//! against the theme context, write the output under
//! `<tmp>/sy/<rel>`. If any of the three files is missing from the
//! repo, or its content doesn't render cleanly, the test fails.
//!
//! Mirrors the `apply()` function shape in `src/main.rs`.

use std::path::PathBuf;

use minijinja::Environment;

/// Files Step 34 productivises under `configs/sy/`. Stable list so
/// adding a fourth in a follow-on roadmap step surfaces the obligation
/// to extend this test alongside.
const STEP34_FILES: &[&str] = &["file.toml", "file-keymap.toml", "file-theme.toml"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn theme_ctx() -> minijinja::Value {
    // Hand-rolled context mirroring `main::load_theme`'s output for
    // `themes/gruvbox-material.toml`. Hard-coding the values here
    // keeps the test hermetic — a theme rename / repalette of
    // gruvbox-material doesn't silently break Step 34's contract.
    let theme = serde_json::json!({
        "home": "/home/sy-test",
        "colors": {
            "bg": "#282828",
            "bg_soft": "#32302f",
            "bg1": "#3c3836",
            "bg2": "#504945",
            "fg": "#ebdbb2",
            "fg_dim": "#a89984",
            "red": "#ea6962",
            "orange": "#e78a4e",
            "yellow": "#d8a657",
            "green": "#a9b665",
            "aqua": "#89b482",
            "blue": "#7daea3",
            "purple": "#d3869b",
            "gray": "#928374"
        },
        "ui": { "accent": "#89b482" }
    });
    minijinja::Value::from_serialize(&theme)
}

#[test]
fn renders_via_minijinja() {
    let root = repo_root();
    let configs_sy = root.join("configs/sy");
    let tmp = tempfile::tempdir().expect("step34 — tempdir");
    let env = Environment::new();
    let ctx = theme_ctx();
    let mut rendered_count = 0usize;
    for name in STEP34_FILES {
        let src = configs_sy.join(name);
        let body = std::fs::read_to_string(&src)
            .unwrap_or_else(|e| panic!("step34 — read {}: {}", src.display(), e));
        let out = env
            .render_str(&body, &ctx)
            .unwrap_or_else(|e| panic!("step34 — render {}: {}", src.display(), e));
        // `{{ home }}` must have expanded; the literal placeholder must
        // not survive into the rendered output (the production apply
        // walks the same loop).
        assert!(
            !out.contains("{{ home }}"),
            "step34 — `{{{{ home }}}}` placeholder must expand in {name}",
        );
        let dest = tmp.path().join("sy").join(name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).expect("step34 — mkdir tmp parent");
        }
        std::fs::write(&dest, out.as_bytes())
            .unwrap_or_else(|e| panic!("step34 — write {}: {}", dest.display(), e));
        // Parse the rendered TOML — if a placeholder leaked or the
        // syntax drifts, this fails with a parseable diagnostic.
        toml::from_str::<toml::Value>(&out)
            .unwrap_or_else(|e| panic!("step34 — parse rendered {}: {}", dest.display(), e));
        rendered_count += 1;
    }
    assert_eq!(
        rendered_count,
        STEP34_FILES.len(),
        "step34 — every productivised file must render",
    );
}
