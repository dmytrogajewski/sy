//! Integration tests for the `sy file` clap variant landed in Step 13
//! of the [`sy-file-manager` roadmap][roadmap]. Drives the `sy` binary
//! end-to-end via `CARGO_BIN_EXE_sy` (same shape as
//! `tests/sy_plugin_cli.rs`). Step 13 is pure scaffold for `--help` and
//! bare-form dispatch; Step 33 replaces the doctor scaffold with the
//! real [`docs/reference/sy-file-doctor.md`][doc] surface.
//!
//! [roadmap]: ../specs/roadmaps/sy-file-manager/ROADMAP.md
//! [doc]: ../docs/reference/sy-file-doctor.md
use std::process::Command;

/// Stable schema marker emitted by `sy file doctor --json`. Step 33
/// bumped the schema from the scaffold-era
/// `sy.file.doctor.scaffold/v0` marker to the real
/// `sy.file.doctor/v1` documented under `docs/reference/sy-file-doctor.md`.
const SCHEMA_DOCTOR: &str = "sy.file.doctor/v1";

/// `sy file --help` must exit 0 (clap renders help to stdout for
/// `--help`). Anchors journey J1's "shell can dispatch the verb"
/// precondition: if the binary errors on `--help`, niri's `Mod+E`
/// can't dispatch either.
#[test]
fn dispatch_smoke_help_exits_zero() {
    let bin = env!("CARGO_BIN_EXE_sy");
    let out = Command::new(bin)
        .args(["file", "--help"])
        .output()
        .expect("spawn sy file --help");
    assert!(
        out.status.success(),
        "sy file --help must exit 0, got {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("doctor"),
        "sy file --help must list the doctor subcommand:\n{stdout}",
    );
}

/// Bare `sy file` (no subcommand, no path) must print the "scaffold"
/// marker on stdout and exit 0. This is the literal carrier J1's
/// `sy file ~` will ride in Step 34 — failing here means J1 cannot
/// even start.
#[test]
fn dispatch_smoke_bare_prints_scaffold_marker() {
    let bin = env!("CARGO_BIN_EXE_sy");
    let out = Command::new(bin)
        .args(["file"])
        .output()
        .expect("spawn sy file");
    assert!(
        out.status.success(),
        "sy file must exit 0 in Step 13 scaffold, got {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("scaffold"),
        "sy file must print the scaffold marker on stdout:\n{stdout}",
    );
}

/// `sy file <path>` with a positional path must also print the
/// scaffold marker and exit 0. Step 34's niri keybind dispatches
/// `sy file ~` so the positional form is the journey shape.
#[test]
fn dispatch_smoke_with_path_prints_scaffold_marker() {
    let bin = env!("CARGO_BIN_EXE_sy");
    let tmp = tempfile::tempdir().expect("tmp");
    let out = Command::new(bin)
        .args(["file"])
        .arg(tmp.path())
        .output()
        .expect("spawn sy file <path>");
    assert!(
        out.status.success(),
        "sy file <path> must exit 0 in Step 13 scaffold, got {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("scaffold"),
        "sy file <path> must print the scaffold marker:\n{stdout}",
    );
}

/// `sy file doctor` (no flag) must surface the human-rendered probe
/// summary on stdout. Step 33 replaced the scaffold "not-implemented-yet"
/// marker with the SPEC §3.3 item 19 probe runner; the trailing summary
/// line is the wire-stable assertion target.
#[test]
fn dispatch_smoke_doctor_human_surfaces_summary() {
    let bin = env!("CARGO_BIN_EXE_sy");
    let out = Command::new(bin)
        .args(["file", "doctor"])
        .output()
        .expect("spawn sy file doctor");
    // The exit code reflects the host's real state (0 ok, 1 any-fail,
    // 2 warn-only); we don't assert success here because the live host
    // running this test rarely has all six probes green.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("sy file doctor:"),
        "sy file doctor must print the human summary footer:\n{stdout}",
    );
}

/// `sy file doctor --json` must emit valid JSON carrying the
/// `sy.file.doctor/v1` schema marker plus the `status` + `checks`
/// fields documented at `docs/reference/sy-file-doctor.md`.
#[test]
fn dispatch_smoke_doctor_json_emits_v1_schema_envelope() {
    let bin = env!("CARGO_BIN_EXE_sy");
    let out = Command::new(bin)
        .args(["file", "doctor", "--json"])
        .output()
        .expect("spawn sy file doctor --json");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout)
        .expect("sy file doctor --json must emit parseable JSON");
    assert_eq!(
        doc["schema"].as_str(),
        Some(SCHEMA_DOCTOR),
        "doctor --json must pin the Step-33 `sy.file.doctor/v1` schema: {doc:?}",
    );
    let status = doc["status"].as_str().unwrap_or_default();
    assert!(
        matches!(status, "ok" | "warn" | "fail"),
        "doctor --json status must be ok/warn/fail, got {status:?}",
    );
    assert!(
        doc["checks"].is_array(),
        "doctor --json must carry a checks array: {doc:?}",
    );
}
