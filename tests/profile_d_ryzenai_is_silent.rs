//! Asset-content test for `configs/profile.d/ryzenai.sh`.
//!
//! BUG-20260524-2235: the `ryzenai-1.7.1-1.fc43` rpm ships a noisy +
//! non-idempotent `/etc/profile.d/ryzenai.sh` that prints a four-line
//! XRT banner in every interactive shell and re-prepends `$PATH` /
//! `$LD_LIBRARY_PATH` / `$PYTHONPATH` on every nest. The user's
//! pre-upgrade silent shape (preserved as `.rpmsave`) is the
//! canonical fix; this repo now owns it under `configs/profile.d/` and
//! `scripts/install-system-npu.sh` deploys it.
//!
//! This test pins the silent + idempotent shape so a future edit
//! cannot quietly regress to the noisy upstream shape. It does NOT
//! source the script (that would need `/opt/xilinx/xrt/setup.sh`,
//! which is host state); it asserts the file contents directly.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn snippet() -> String {
    let path = repo_root().join("configs/profile.d/ryzenai.sh");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn ryzenai_profile_d_sources_xrt_silently_and_dedupes_path() {
    let body = snippet();

    assert!(
        body.contains(". /opt/xilinx/xrt/setup.sh >/dev/null"),
        "must source xrt/setup.sh with `>/dev/null` redirect so the \
         banner stays out of interactive shells; got:\n{body}"
    );

    assert!(
        body.contains("[ -z \"$XILINX_XRT\" ]"),
        "must guard the source on $XILINX_XRT being empty so nested \
         interactive shells don't re-source xrt/setup.sh; got:\n{body}"
    );

    assert!(
        body.contains("case \":$PATH:\"")
            && body.contains("RYZEN_AI_INSTALLATION_PATH"),
        "must dedupe the venv $PATH prepend with a `case` guard so \
         $PATH doesn't bloat by one venv entry per nested shell; \
         got:\n{body}"
    );

    for noisy in [
        // Exact upstream-rpm offender. Any of these without the
        // `>/dev/null` redirect would reintroduce the banner.
        "    . /opt/xilinx/xrt/setup.sh\n",
        "\t. /opt/xilinx/xrt/setup.sh\n",
        "source /opt/xilinx/xrt/setup.sh\n",
    ] {
        assert!(
            !body.contains(noisy),
            "must not contain unredirected source line {noisy:?}; got:\n{body}"
        );
    }
}
