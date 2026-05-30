//! Roadmap Step 36 — final no-snowflakes step.
//!
//! `sy file` is now the canonical file-manager surface (Steps 13-35),
//! so the yazi productivisation (the `configs/yazi/` rice + the
//! `scripts/yazi-plugins.sh` bootstrap) is gone from the repo.
//!
//! Two structural assertions pin the deletion at the repo-source level
//! so a future regression (a stray git revert, a half-merged branch,
//! someone re-vendoring a yazi plugin) surfaces at `make test` time:
//!
//! 1. `repo_has_no_yazi_path_under_configs` walks `configs/`
//!    recursively and asserts no path component named `yazi` survives.
//! 2. `scripts_yazi_plugins_sh_absent` asserts the bootstrap script
//!    file is gone from `scripts/`.
//!
//! Both probes use `CARGO_MANIFEST_DIR` to anchor at the repo root,
//! mirroring the pattern other `tests/configs_*.rs` files already use.

use std::path::PathBuf;

/// Repo root, resolved from the cargo-injected env var. Mirrors the
/// pattern in `tests/configs_niri_sy_file_binds.rs` so a future
/// workspace move surfaces in one place.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Walk `configs/` and assert no path component literally named
/// `yazi` survives the Step 36 deletion. Uses `walkdir` already in the
/// workspace dev-deps via other tests; if it isn't available a manual
/// recursive walk works equally well — see `_walk_paths` below.
#[test]
fn repo_has_no_yazi_path_under_configs() {
    let configs = repo_root().join("configs");
    assert!(
        configs.is_dir(),
        "configs/ must exist at repo root, got {}",
        configs.display()
    );
    let mut offenders: Vec<PathBuf> = Vec::new();
    walk_collect_yazi(&configs, &mut offenders);
    assert!(
        offenders.is_empty(),
        "Step 36 — `configs/` must not contain any path component named `yazi`; \
         found {} offending path(s): {:?}",
        offenders.len(),
        offenders,
    );
}

/// Scripts dir must not carry `yazi-plugins.sh` after Step 36.
#[test]
fn scripts_yazi_plugins_sh_absent() {
    let script = repo_root().join("scripts").join("yazi-plugins.sh");
    assert!(
        !script.exists(),
        "Step 36 — scripts/yazi-plugins.sh must be deleted, found at {}",
        script.display()
    );
}

/// Recursive walk that pushes any entry whose any path component is
/// literally `yazi` (handles both `configs/yazi/` and any nested
/// `…/yazi/…` regression).
fn walk_collect_yazi(dir: &std::path::Path, offenders: &mut Vec<PathBuf>) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path
            .components()
            .any(|c| c.as_os_str().to_string_lossy() == "yazi")
        {
            offenders.push(path.clone());
        }
        if path.is_dir() {
            walk_collect_yazi(&path, offenders);
        }
    }
}
