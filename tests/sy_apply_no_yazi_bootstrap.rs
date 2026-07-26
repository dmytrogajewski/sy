//! Roadmap Step 36 — `sy apply --dry-run` no longer mentions yazi.
//!
//! Before Step 36, `apply()` printed `"yazi:"` followed by `"  ~ bash
//! <repo>/scripts/yazi-plugins.sh"` (or in non-dry mode, ran the
//! script). After Step 36 the `ensure_yazi(root, dry)?;` call site +
//! the `mod yazi_install;` declaration are deleted from `src/main.rs`,
//! so no token containing `yazi-plugins.sh` or `yazi_install` can
//! survive in the dry-run transcript.
//!
//! The test drives the real `sy` bin via `CARGO_BIN_EXE_sy`,
//! re-pointing `--target` at a fresh tempdir so the operator's actual
//! `~/.config/` is not touched (a CLAUDE.md "no snowflakes" invariant
//! the test itself must uphold). `--root` is left default so apply
//! walks the in-repo `configs/` tree the same way `make lint`'s post-
//! apply sanity does.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn dry_run_doesnt_invoke_yazi_plugins_sh() {
    let bin = env!("CARGO_BIN_EXE_sy");
    let tmp = tempfile::tempdir().expect("step36 tempdir");
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new(bin)
        .args(["apply", "--dry-run"])
        .arg("--root")
        .arg(&repo_root)
        .arg("--target")
        .arg(tmp.path())
        // Synthetic HOME so any `~`-expansion inside `apply` lands
        // entirely in the tempdir, not the operator's real home.
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .output()
        .expect("spawn sy apply --dry-run");

    // The dry-run path must succeed: pre-Step-36 this assert FAILS
    // because `ensure_yazi` errors out when `scripts/yazi-plugins.sh`
    // is missing. Post-Step-36 the call site is gone, so the dry-run
    // returns 0.
    assert!(
        out.status.success(),
        "Step 36 — `sy apply --dry-run` must exit 0 after the yazi bootstrap is removed; \
         exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    for needle in ["yazi-plugins.sh", "yazi_install"] {
        assert!(
            !stdout.contains(needle),
            "Step 36 — `sy apply --dry-run` stdout must not mention `{needle}`; got:\n{stdout}",
        );
        assert!(
            !stderr.contains(needle),
            "Step 36 — `sy apply --dry-run` stderr must not mention `{needle}`; got:\n{stderr}",
        );
    }
}
