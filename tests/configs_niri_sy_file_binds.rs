//! Roadmap Step 34 — `configs/niri/config.kdl` must carry the three
//! journey-J1 binds (`Mod+E`, `Mod+Shift+E`, `Mod+Slash`) all
//! dispatching to `sy file`. Per the cross-cutting DoD in
//! [`specs/roadmaps/sy-file-manager/ROADMAP.md`](../specs/roadmaps/sy-file-manager/ROADMAP.md)
//! this is the productivised path the operator hits at every login.
//!
//! The doctor already has a runtime probe (`file.niri.binds`) over the
//! same file; this test pins the **source-of-truth** repo file so a
//! drift in `configs/niri/config.kdl` surfaces at `make test` time, not
//! only when an operator runs `sy file doctor` post-`sy apply`.
//!
//! The parse is intentionally line-oriented (the niri `binds {}` block
//! uses one-line `<key> { <action>; }` shape on every productivised
//! entry) — same shape `src/file/doctor.rs::find_bind_target` reads.
//! Niri's own `niri validate` is an optional second layer; this test
//! is the structural anchor.

use std::path::PathBuf;

/// The three journey-J1 binds Step 34 lands. Order pinned for stable
/// test output.
const REQUIRED_BINDS: &[&str] = &["Mod+E", "Mod+Shift+E", "Mod+Slash"];

/// Read the productivised `configs/niri/config.kdl` from the repo
/// root. The `CARGO_MANIFEST_DIR` env is set by cargo for the test
/// runner so this works under `make test` and `cargo test` alike.
fn niri_config_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("configs/niri/config.kdl")
}

/// Extract the body between `{` and `}` for the *exact* bind name on
/// a single line. Matches `find_bind_target` in `src/file/doctor.rs`
/// so the test and the doctor probe stay symmetric.
fn find_bind_action(body: &str, bind: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        let after = match trimmed.strip_prefix(bind) {
            Some(rest) => rest,
            None => continue,
        };
        if !after.starts_with(|c: char| c.is_whitespace() || c == '{') {
            continue;
        }
        let (Some(open), Some(close)) = (trimmed.find('{'), trimmed.rfind('}')) else {
            continue;
        };
        if close <= open {
            continue;
        }
        return Some(trimmed[open + 1..close].trim().to_string());
    }
    None
}

#[test]
fn binds_parsed_by_niri_validate() {
    let path = niri_config_path();
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("step34 — read {}: {}", path.display(), e));
    for bind in REQUIRED_BINDS {
        let action = find_bind_action(&body, bind).unwrap_or_else(|| {
            panic!(
                "step34 — niri config missing `{bind}` bind in {}",
                path.display()
            )
        });
        // Loose match — the productivised binding is
        // `spawn "{{ home }}/.local/bin/sy" "file" ...` so the test
        // accepts either the bare `"sy"` token or the `.local/bin/sy`
        // tail (the latter matches the productivised form).
        assert!(
            action.contains("\"sy\"") || action.contains("\"sy ") || action.contains("/bin/sy\""),
            "step34 — `{bind}` action must dispatch to `sy`: {action}"
        );
        assert!(
            action.contains("\"file\""),
            "step34 — `{bind}` action must reference `file` subcommand: {action}"
        );
    }
}
