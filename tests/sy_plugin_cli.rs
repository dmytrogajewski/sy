//! Integration tests for `sy plugin` — Step 8 of the
//! [`sy-file-manager` roadmap][roadmap]. Drives the `sy` binary
//! end-to-end via `CARGO_BIN_EXE_sy` (no `assert_cmd` dep so the
//! workspace `Cargo.toml` stays minimal). Each test points
//! `SY_PLUGIN_DIR` at a hermetic tempdir holding a single
//! `plugin.toml` fixture, so the host's `~/.local/share/sy/plugins/`
//! never leaks into the assertion.
//!
//! Exit codes asserted here follow [plugin SPEC §4.5][spec-cli]:
//! 0 ok, 2 usage / validation, 8 plugin unreachable / unhealthy.
//!
//! [roadmap]: ../specs/roadmaps/sy-file-manager/ROADMAP.md
//! [spec-cli]: ../specs/research/sy-file-manager-plugins/SPEC.md#45-cli--mcp-surface
use std::path::{Path, PathBuf};
use std::process::Command;

/// Exit code emitted when the registry/loader rejects an input
/// (bad TOML, bad glob, malformed manifest path). Mirrors SPEC §4.5.
const EXIT_USAGE: i32 = 2;

/// Exit code emitted when a plugin's manifest references a
/// non-existent binary (SPEC §4.5 "plugin unreachable / unhealthy").
const EXIT_PLUGIN_UNHEALTHY: i32 = 8;

/// Path to the in-tree fake plugin shipped under
/// `tests/fixtures/sy-plugin-fake/bin/sy-plugin-fake`. Resolved
/// against `CARGO_MANIFEST_DIR` so the path is stable regardless of
/// the test runner's cwd.
fn fake_plugin_bin() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir).join("tests/fixtures/sy-plugin-fake/bin/sy-plugin-fake")
}

/// Plant a `plugin.toml` under `<root>/<plugin_dir>/plugin.toml` and
/// write the binary symlink so the manifest's `[plugin.binary] exec`
/// is reachable from the manifest directory. Returns the manifest
/// path so callers can pass it to `sy plugin validate`.
fn install_fake_plugin(root: &Path, plugin_id: &str, bin_path: &Path) -> PathBuf {
    let dir = root.join(plugin_id);
    std::fs::create_dir_all(dir.join("bin")).expect("mkdir plugin bin dir");
    // Use an absolute path for `exec` so the doctor / supervisor can
    // resolve the binary without depending on the manifest dir's
    // location. Production plugins under `~/.local/share/sy/plugins/`
    // ship a relative `./bin/<name>` that the loader joins with the
    // manifest dir — Step 9 lands that resolution; today the CLI
    // accepts both.
    let manifest_body = format!(
        r#"
api = "1"

[plugin]
id = "{plugin_id}"
name = "Fake Plugin"
version = "0.0.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "{exec}"

[[capability]]
kind = "previewer"
mime = "text/markdown"

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
spawn_timeout_ms = 1000
shutdown_timeout_ms = 1000
"#,
        plugin_id = plugin_id,
        exec = bin_path.display(),
    );
    let manifest = dir.join("plugin.toml");
    std::fs::write(&manifest, manifest_body).expect("write plugin.toml");
    manifest
}

/// Build a `Command` invoking the binary under test with
/// `SY_PLUGIN_DIR` pinned to a hermetic tempdir + `XDG_DATA_HOME`
/// scrubbed (defence-in-depth so the host's real plugin lane never
/// leaks into the assertion).
fn sy(plugin_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sy"));
    cmd.env("SY_PLUGIN_DIR", plugin_dir);
    cmd.env_remove("XDG_DATA_HOME");
    cmd.env_remove("SY_PLUGIN_DISABLED_TOML");
    cmd
}

/// SPEC §4.5: `sy plugin list --json` returns the discovered
/// manifests as a JSON array keyed on `id` + `version`. Locks the
/// schema so Step 35's docs don't drift from the binary.
#[test]
fn list_returns_discovered_manifests() {
    let tmp = tempfile::tempdir().expect("tmp");
    install_fake_plugin(tmp.path(), "sample", &fake_plugin_bin());

    let out = sy(tmp.path())
        .args(["plugin", "list", "--json"])
        .output()
        .expect("spawn sy plugin list");
    assert!(
        out.status.success(),
        "exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("list --json must emit parseable JSON");
    let arr = v["plugins"].as_array().expect("plugins is an array");
    assert_eq!(arr.len(), 1, "exactly one fake plugin discovered: {v}");
    assert_eq!(arr[0]["id"].as_str(), Some("sample"));
    assert_eq!(arr[0]["version"].as_str(), Some("0.0.0"));
    // The capability rows must surface in the JSON so an MCP agent
    // can route preview without re-parsing the TOML.
    let caps = arr[0]["capabilities"]
        .as_array()
        .expect("capabilities array");
    assert!(
        caps.iter()
            .any(|c| c["kind"] == "previewer" && c["mime"] == "text/markdown"),
        "previewer/text-markdown row must be present: {caps:?}"
    );
}

/// SPEC §4.5: `sy plugin doctor` exits 0 + reports every check green
/// when the binary the manifest points at is reachable + executable.
#[test]
fn doctor_passes_on_well_formed_fixture() {
    let tmp = tempfile::tempdir().expect("tmp");
    install_fake_plugin(tmp.path(), "sample", &fake_plugin_bin());

    let out = sy(tmp.path())
        .args(["plugin", "doctor", "--json"])
        .output()
        .expect("spawn sy plugin doctor");
    assert!(
        out.status.success(),
        "exit={:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor --json parseable");
    let checks = v["checks"].as_array().expect("checks array");
    assert!(!checks.is_empty(), "at least one check ran: {v}");
    assert!(
        checks.iter().all(|c| c["ok"].as_bool() == Some(true)),
        "every check must be green: {checks:?}"
    );
    // Schema invariants — Step 35 mirrors these in docs.
    assert_eq!(v["schema"].as_str(), Some("sy.plugin.doctor/v1"));
}

/// SPEC §4.5 exit code 8: `sy plugin doctor` reports plugin
/// unreachable when the manifest's binary path is non-existent.
#[test]
fn doctor_fails_on_missing_binary() {
    let tmp = tempfile::tempdir().expect("tmp");
    let bogus = tmp.path().join("does-not-exist");
    install_fake_plugin(tmp.path(), "broken", &bogus);

    let out = sy(tmp.path())
        .args(["plugin", "doctor"])
        .output()
        .expect("spawn sy plugin doctor");
    assert_eq!(
        out.status.code(),
        Some(EXIT_PLUGIN_UNHEALTHY),
        "exit code must be 8 (plugin unreachable) — got {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// SPEC §4.5 exit code 2: `sy plugin validate <path>` rejects a
/// manifest with a malformed glob predicate.
#[test]
fn validate_rejects_bad_glob() {
    let tmp = tempfile::tempdir().expect("tmp");
    let manifest_path = tmp.path().join("plugin.toml");
    // `[` opens a character class; without `]` it's a malformed
    // glob that `globset::Glob::new` rejects.
    let bad = r#"
api = "1"

[plugin]
id = "bad-glob"
name = "Bad"
version = "0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "/bin/true"

[[capability]]
kind = "previewer"
url = "[unterminated"

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
spawn_timeout_ms = 500
shutdown_timeout_ms = 500
"#;
    std::fs::write(&manifest_path, bad).expect("write manifest");

    let out = sy(tmp.path())
        .args(["plugin", "validate"])
        .arg(&manifest_path)
        .output()
        .expect("spawn sy plugin validate");
    assert_eq!(
        out.status.code(),
        Some(EXIT_USAGE),
        "validate must exit 2 on bad glob — got {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("glob") || stderr.contains("url") || stderr.contains("capability"),
        "stderr must name the bad predicate, got: {stderr}"
    );
}

/// SPEC §4.5: `sy plugin exec <id> <method>` spawns the plugin,
/// handshakes, dispatches one request, captures the result, and
/// exits 0 with the result JSON on stdout. The fake plugin echoes
/// `params` back as the result.
#[test]
fn exec_one_shot_request_against_fake_plugin() {
    let tmp = tempfile::tempdir().expect("tmp");
    let runtime = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime).expect("mkdir runtime");
    install_fake_plugin(tmp.path(), "sample", &fake_plugin_bin());

    let out = sy(tmp.path())
        .env("SY_PLUGIN_RUNTIME_DIR", &runtime)
        .args([
            "plugin",
            "exec",
            "sample",
            "ping",
            "--params",
            r#"{"hello":"world"}"#,
        ])
        .output()
        .expect("spawn sy plugin exec");
    assert!(
        out.status.success(),
        "exec exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("exec stdout must be JSON");
    // `ping` returns `{"ts": <number>}`; `hello: world` was in the
    // params but the fake's `ping` handler echoes back the `ts` only.
    // Either way the result must be a JSON object on stdout.
    assert!(v.is_object(), "result must be a JSON object, got: {v}");
}
