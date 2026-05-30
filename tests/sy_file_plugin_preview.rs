//! Roadmap Step 27 — `PluginBridge` dispatch round-trips.
//!
//! Exercises the file plane → `crate::file::plugin_bridge::PluginBridge`
//! → plugin process pipeline end-to-end. Three named tests
//! lock the contracts the journey-J3 hover-preview beat depends on:
//!
//! 1. [`pdf_dispatched_to_pdf_plugin_fixture`] — a freshly-discovered
//!    `sy-plugin-fake-pdf` fixture wins the dispatch for
//!    `application/pdf`. Asserts the bridge returns the PNG the fake
//!    emits byte-for-byte.
//! 2. [`md_uses_sy_plugin_md_end_to_end`] — the real `sy-plugin-md`
//!    canary handles `text/markdown`. Asserts the cold-start path is
//!    under 600 ms and a second hover on the same file is under
//!    100 ms (the `procs: HashMap` cache contract).
//! 3. [`plugin_crash_falls_back_to_built_in_text`] — when the
//!    plugin returns an error (the fake's `preview` arm is replaced
//!    with a `panic`-equivalent emit), the bridge surfaces
//!    [`BridgeError::PluginCrashed`] and the view's calling site
//!    routes to the built-in text path.
//!
//! Each test is hermetic — `$SY_PLUGIN_DIR` is pointed at a tempdir
//! shadow so no other discovered plugin can poison the dispatch.

// Mirror the `#[path]` shim every other `tests/sy_*` integration test
// uses. The `sy` package has no `lib.rs`, so we pull the production
// sources in directly. The set covers every module
// `src/file/plugin_bridge.rs` ultimately reaches for transitively.

#[path = "../src/plugin/capability.rs"]
mod capability;
#[path = "../src/plugin/host_fns.rs"]
mod host_fns;
#[path = "../src/plugin/install.rs"]
mod install;
#[path = "../src/plugin/manifest.rs"]
mod manifest;
#[path = "../src/plugin/proc.rs"]
mod proc_mod;
#[path = "../src/plugin/registry.rs"]
mod registry;
#[path = "../src/plugin/rpc.rs"]
mod rpc;
#[path = "../src/plugin/sandbox.rs"]
mod sandbox;
#[path = "../src/plugin/transport.rs"]
mod transport;

#[path = "../src/file/plugin_bridge.rs"]
#[allow(dead_code)]
mod plugin_bridge;

/// Side-shim so the `#[path]`-imported source files'
/// `use crate::plugin::…` lines resolve under this test binary.
pub(crate) mod plugin {
    pub(crate) use super::capability;
    pub(crate) use super::host_fns;
    pub(crate) use super::install;
    pub(crate) use super::manifest;
    pub(crate) use super::proc_mod as proc;
    pub(crate) use super::registry;
    pub(crate) use super::rpc;
    pub(crate) use super::sandbox;
    pub(crate) use super::transport;
}

// The bridge module references only `crate::plugin::…` (which is
// re-exported above); no `crate::file::…` re-export is required for
// the source to compile under this binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plugin_bridge::{BridgeError, PluginBridge, PreviewResult};

/// `_force_*_used_under_integration_test` — same shim shape as the
/// sibling `sy_plugin_conformance` test. The integration-test
/// binary compiles the production `#[path]`-imported sources alone,
/// without the bin's call sites, so any associated item the bin uses
/// but the test bodies don't gets flagged dead. We touch each one
/// here so clippy's `-D warnings` mode stays green.
#[allow(dead_code)]
fn _force_install_module_used_under_integration_test() {
    let _ = install::install;
    let _ = install::verify_signature;
    let _ = install::strip_signature_block;
    let _ = install::NO_SIGNATURE_ENV;
    let _ = install::InstallOpts::new(PathBuf::from("/tmp/sy-plugins"));
    let _ = install::InstallSource::Path(PathBuf::from("/tmp"));
    let _ = install::InstallSource::Git {
        url: "git+file:///tmp".into(),
        rev: None,
    };
    let _ = install::InstalledPlugin {
        id: String::new(),
        dir: PathBuf::new(),
    };
    let _ = install::InstallError::Io("x".into());
    let _ = install::InstallError::ManifestInvalid("x".into());
    let _ = install::InstallError::SignatureInvalid("x".into());
    let _ = registry::Registry::manifest_dir;
    let _ = registry::Registry::empty;
    let _ = registry::discover_empty;
}

/// Step 27 perf budget — cold hover spawns the plugin + handshakes +
/// renders. Pinned at 600 ms per the journey-J3 brief.
const COLD_BUDGET_MS: u128 = 600;
/// Step 27 perf budget — warm hover re-uses the cached supervisor.
/// Pinned at 100 ms per the journey-J3 brief.
const WARM_BUDGET_MS: u128 = 100;

/// CI / debug-build slack. The integration-test binary itself runs
/// in debug profile by default; only `sy-plugin-md` is built
/// `--release`. Honours the same `SY_CONFORMANCE_PERF_X2` escape
/// hatch as `tests/sy_plugin_conformance.rs` — `cargo test
/// --release` (or unsetting the env var on a fast runner) sees the
/// unscaled production budgets.
fn perf_x() -> u128 {
    if std::env::var_os("SY_CONFORMANCE_PERF_X2").is_some() || cfg!(debug_assertions) {
        2
    } else {
        1
    }
}

/// Install a manifest + binary under `$root/<plugin_id>/`. Mirrors the
/// pattern `tests/sy_file_journey_e2e.rs::step12_*` uses: the
/// hermetic install lane.
fn install_fixture_plugin(
    root: &Path,
    plugin_id: &str,
    binary_src: &Path,
    manifest_body: &str,
) -> PathBuf {
    let plugin_dir = root.join(plugin_id);
    std::fs::create_dir_all(plugin_dir.join("bin")).expect("mkdir bin");
    let installed_bin = plugin_dir.join("bin").join(plugin_id);
    std::fs::copy(binary_src, &installed_bin).expect("copy fixture bin");
    // Make sure the copied bash script is still executable on
    // filesystems where `copy` preserves perms (most do, but a tmpfs
    // mount with noexec would silently break the supervisor — set
    // explicitly).
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&installed_bin)
        .expect("stat installed bin")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&installed_bin, perms).expect("chmod installed bin");
    let resolved = manifest_body.replace("{exec}", &installed_bin.to_string_lossy());
    std::fs::write(plugin_dir.join("plugin.toml"), resolved).expect("write plugin.toml");
    installed_bin
}

/// Build a [`PluginBridge`] against `$SY_PLUGIN_DIR=$plugin_root`.
/// Holds `registry::env_lock()` only across the synchronous
/// `discover()` snapshot so the bridge's async `preview_for` calls
/// don't trip clippy's `await_holding_lock` lint. The bridge's
/// `HostCtx` carries a preview channel (the receivers are dropped —
/// the tests don't drive them; the canary plugins don't call
/// `host.preview.*` so the receiver staying drained is the expected
/// shape).
fn bridge_from_root(plugin_root: &Path) -> Arc<PluginBridge> {
    let reg = {
        let _lock = registry::env_lock();
        // SAFETY: env lock held above; integration tests in this
        // binary serialise their env mutations through the same
        // mutex.
        unsafe {
            std::env::set_var(registry::PLUGIN_DIR_ENV, plugin_root);
            std::env::remove_var(registry::DISABLED_TOML_ENV);
        }
        Arc::new(registry::discover().expect("discover ok"))
    };
    let (ctx, _notify_rx, _preview_rx) =
        host_fns::ctx_for_with_preview(plugin_root.to_path_buf(), serde_json::Value::Null);
    Arc::new(PluginBridge::new(reg, ctx))
}

/// Manifest body for the `sy-plugin-fake-pdf` fixture installed at
/// `tests/fixtures/sy-plugin-fake-pdf/`. The `{exec}` token is
/// substituted by [`install_fixture_plugin`].
const FAKE_PDF_MANIFEST: &str = r#"
api = "1"

[plugin]
id = "sy-plugin-fake-pdf"
name = "Fake PDF Previewer"
version = "0.0.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "{exec}"

[[capability]]
kind = "previewer"
mime = "application/pdf"
[[capability]]
kind = "previewer"
url = "*.pdf"

[needs]
fs_read = []
fs_write = []
preview = []
knowledge = []
network = []
exec = []

[limits]
memory_mb = 64
cpu_seconds = 10
nofile = 64
spawn_timeout_ms = 2000
shutdown_timeout_ms = 1000
"#;

/// Manifest body for the `sy-plugin-md` canary — same shape as
/// `crates/sy-plugin-md/plugin.toml` but with the `{exec}` placeholder
/// substituted at install time so the supervisor finds the release
/// binary.
const SY_PLUGIN_MD_MANIFEST: &str = r#"
api = "1"

[plugin]
id = "sy-plugin-md"
name = "Markdown Previewer"
version = "0.1.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "{exec}"

[[capability]]
kind = "previewer"
mime = "text/markdown"
[[capability]]
kind = "previewer"
url = "*.md"
[[capability]]
kind = "previewer"
url = "*.markdown"

[needs]
fs_read = ["**/*.md", "**/*.markdown"]
fs_write = []
preview = []
knowledge = []
network = []
exec = []

[limits]
memory_mb = 256
cpu_seconds = 30
nofile = 256
spawn_timeout_ms = 2000
shutdown_timeout_ms = 1000
"#;

/// Build the in-tree `sy-plugin-md` binary if it isn't already
/// available under `target/release/`. Mirrors the `step12_*` warm-up
/// in `tests/sy_file_journey_e2e.rs` so the same binary the journey
/// e2e drives is what the bridge spawns here.
fn ensure_sy_plugin_md_release_binary() -> PathBuf {
    let plugin_bin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("release")
        .join("sy-plugin-md");
    if !plugin_bin.is_file() {
        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("crates")
            .join("sy-plugin-md")
            .join("Cargo.toml");
        let build = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "--release",
                "-p",
                "sy-plugin-md",
                "--bin",
                "sy-plugin-md",
                "--manifest-path",
                manifest_path.to_string_lossy().as_ref(),
            ])
            .output()
            .expect("cargo build sy-plugin-md");
        assert!(
            build.status.success(),
            "sy-plugin-md build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }
    assert!(
        plugin_bin.is_file(),
        "sy-plugin-md missing at {} after build",
        plugin_bin.display()
    );
    plugin_bin
}

/// Step 27 / SPEC §3.3 item 8 — when the registry holds a `previewer`
/// claiming `application/pdf`, the bridge routes a synthetic `.pdf`
/// hover through that plugin. Asserts the bytes returned came from
/// the fake-pdf fixture (its deterministic 1×1 magenta PNG signature)
/// so a future change that silently re-routes PDFs through a
/// different plugin trips this test.
#[tokio::test(flavor = "current_thread")]
async fn pdf_dispatched_to_pdf_plugin_fixture() {
    // `bridge_from_root` acquires `registry::env_lock()` internally
    // and drops it before any `.await`, so the test body doesn't
    // hold the std::sync::Mutex across an await (clippy's
    // `await_holding_lock` lint).
    let tmp_root = tempfile::tempdir().expect("tempdir");
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sy-plugin-fake-pdf")
        .join("bin")
        .join("sy-plugin-fake-pdf");
    install_fixture_plugin(
        tmp_root.path(),
        "sy-plugin-fake-pdf",
        &fixture_bin,
        FAKE_PDF_MANIFEST,
    );
    let bridge = bridge_from_root(tmp_root.path());

    // Synthetic PDF — content doesn't matter (the fake doesn't open
    // it), only the `.pdf` extension drives the dispatch route.
    let pdf_path = tmp_root.path().join("report.pdf");
    std::fs::write(&pdf_path, b"%PDF-1.4\n%STUB").expect("write pdf");

    let result = bridge
        .preview_for("application/pdf", &pdf_path)
        .await
        .expect("pdf must dispatch to sy-plugin-fake-pdf");
    let bytes = match result {
        PreviewResult::Png(b) => b,
        other => panic!("expected Png arm, got {other:?}"),
    };
    // Header byte-for-byte (PNG signature + IHDR width @ offset 16).
    assert_eq!(
        &bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "fake-pdf payload must be a real PNG (got {:?})",
        &bytes[..8.min(bytes.len())],
    );
    // The fake emits a 1×1 PNG — width at bytes 16..20 BE.
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("png width slice"));
    assert_eq!(
        width, 1,
        "fake-pdf must emit its deterministic 1×1 fixture, not a real render"
    );

    bridge.shutdown_all().await;
}

/// Step 27 / SPEC §3.3 item 8 — the canonical `sy-plugin-md` canary
/// renders Markdown end-to-end through the bridge. Cold-path ≤ 600 ms
/// (the J3 brief's cold-start budget), warm-path ≤ 100 ms (the
/// `procs: HashMap` cache contract).
#[tokio::test(flavor = "current_thread")]
async fn md_uses_sy_plugin_md_end_to_end() {
    // `bridge_from_root` handles env serialisation; see the sibling
    // `pdf_dispatched_to_pdf_plugin_fixture` for the rationale.
    let plugin_bin = ensure_sy_plugin_md_release_binary();
    let tmp_root = tempfile::tempdir().expect("tempdir");
    install_fixture_plugin(
        tmp_root.path(),
        "sy-plugin-md",
        &plugin_bin,
        SY_PLUGIN_MD_MANIFEST,
    );
    let bridge = bridge_from_root(tmp_root.path());

    // The plugin needs `fs_read` access to `**/*.md`, which the
    // manifest above grants. Plant a small markdown fixture inside
    // the tempdir so the read is hermetic.
    let md_path = tmp_root.path().join("preview-sample.md");
    std::fs::write(
        &md_path,
        b"# Preview Sample\n\nA short canary body for the J3 hover-preview perf budget.\n",
    )
    .expect("write md fixture");

    let cold_start = Instant::now();
    let cold_result = bridge
        .preview_for("text/markdown", &md_path)
        .await
        .expect("cold preview must succeed");
    let cold_elapsed = cold_start.elapsed();
    let cold_bytes = match cold_result {
        PreviewResult::Png(b) => b,
        other => panic!("expected Png arm, got {other:?}"),
    };
    assert_eq!(
        &cold_bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "sy-plugin-md must emit a PNG"
    );
    let cold_budget = COLD_BUDGET_MS * perf_x();
    assert!(
        cold_elapsed.as_millis() <= cold_budget,
        "step27 J3 cold path took {cold_elapsed:?}, must be ≤ {cold_budget} ms",
    );

    // Warm path — the supervisor is already cached on `bridge.procs`.
    let warm_start = Instant::now();
    let warm_result = bridge
        .preview_for("text/markdown", &md_path)
        .await
        .expect("warm preview must succeed");
    let warm_elapsed = warm_start.elapsed();
    let warm_bytes = match warm_result {
        PreviewResult::Png(b) => b,
        other => panic!("expected Png arm on warm hover, got {other:?}"),
    };
    assert_eq!(
        &warm_bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "warm preview must still be a PNG"
    );
    let warm_budget = WARM_BUDGET_MS * perf_x();
    assert!(
        warm_elapsed.as_millis() <= warm_budget,
        "step27 J3 warm path took {warm_elapsed:?}, must be ≤ {warm_budget} ms",
    );

    bridge.shutdown_all().await;
}

/// Step 27 DoD `plugin_crash_falls_back_to_built_in_text` — when the
/// plugin's `preview` handler returns an error, the bridge surfaces
/// [`BridgeError::PluginCrashed`] (the bridge evicts the supervisor
/// from the cache so the next hover would re-spawn). The view's
/// calling site reads this discriminant and routes to the syntect
/// built-in text path — that fallback is *covered by the view-layer
/// reducer test* (see `crate::file::view::preview::tests`); this
/// test pins the bridge-side contract that triggers it.
#[tokio::test(flavor = "current_thread")]
async fn plugin_crash_falls_back_to_built_in_text() {
    // `bridge_from_root` handles env serialisation; see the sibling
    // `pdf_dispatched_to_pdf_plugin_fixture` for the rationale.
    let tmp_root = tempfile::tempdir().expect("tempdir");
    // Write a tiny bash plugin that handshakes, then on the first
    // `preview` request emits a JSON-RPC error reply and exits. The
    // bridge's `request_preview` sees the supervisor's error, evicts
    // the supervisor from the cache, and surfaces PluginCrashed.
    let crash_bin = tmp_root.path().join("crash-plugin.sh");
    std::fs::write(
        &crash_bin,
        r#"#!/bin/bash
emit() {
  local body="$1"
  printf 'Content-Length: %d\r\n\r\n%s' "${#body}" "$body"
}
FRAME=""
read_frame() {
  local len=0 line
  while IFS= read -r line; do
    line="${line%$'\r'}"
    [ -z "$line" ] && break
    case "$line" in
      Content-Length:*)
        len="${line#Content-Length: }"
        len="${len// /}"
        ;;
    esac
  done || { FRAME=""; return 1; }
  if [ "$len" -gt 0 ]; then
    FRAME=$(dd bs=1 count="$len" 2>/dev/null)
  else
    FRAME=""
  fi
  return 0
}
read_frame
emit '{"jsonrpc":"2.0","id":1,"result":{"name":"crash","version":"0","api":"1","capabilities":[{"kind":"previewer","mime":"text/markdown"}],"host_methods":[]}}'
read_frame
case "$FRAME" in
  *'"method":"preview"'*)
    id=$(printf '%s' "$FRAME" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    emit "{\"jsonrpc\":\"2.0\",\"id\":${id},\"error\":{\"code\":-32603,\"message\":\"plugin panic\",\"data\":null}}"
    ;;
esac
exit 1
"#,
    )
    .expect("write crash plugin");
    use std::os::unix::fs::PermissionsExt;
    let mut p = std::fs::metadata(&crash_bin).expect("meta").permissions();
    p.set_mode(0o755);
    std::fs::set_permissions(&crash_bin, p).expect("chmod");

    install_fixture_plugin(
        tmp_root.path(),
        "sy-plugin-crash",
        &crash_bin,
        // Sub the same manifest body the fake-pdf fixture uses, but
        // claim text/markdown so the dispatch routes hovers on .md to
        // this crash-bot.
        &FAKE_PDF_MANIFEST
            .replace("sy-plugin-fake-pdf", "sy-plugin-crash")
            .replace("Fake PDF Previewer", "Crash Bot")
            .replace("application/pdf", "text/markdown")
            .replace("*.pdf", "*.md"),
    );
    let bridge = bridge_from_root(tmp_root.path());

    let md_path = tmp_root.path().join("doc.md");
    std::fs::write(&md_path, b"# crash\n").expect("write md");
    let err = bridge
        .preview_for("text/markdown", &md_path)
        .await
        .expect_err("plugin crash must surface as BridgeError");
    match err {
        BridgeError::PluginCrashed(_) => {}
        other => panic!("expected PluginCrashed, got {other:?}"),
    }
    bridge.shutdown_all().await;
}

/// Compile-time anchor that pins the bridge's perf-budget constants
/// (they're public to the file plane and to this test binary; the
/// numbers are the SLO the journey-J3 brief commits to).
#[test]
fn perf_budgets_match_journey_j3_brief() {
    // The bridge's per-request timeout is generous enough to cover a
    // cold spawn + render but tight enough that a stalled plugin
    // surfaces as a Timeout before the user sees the spinner.
    assert!(
        plugin_bridge::PREVIEW_REQUEST_TIMEOUT_MS >= COLD_BUDGET_MS as u64,
        "bridge request timeout must be ≥ cold budget"
    );
    // Sanity-check warm < cold; expressed via `const_assert!`-shape
    // so clippy doesn't complain about a constant-value `assert!`.
    const _: () = assert!(WARM_BUDGET_MS < COLD_BUDGET_MS);
    let _ = Duration::from_millis(plugin_bridge::PREVIEW_REQUEST_TIMEOUT_MS);
}
