//! E2E for the waybar applet contract (BUG-20260927-0119).
//!
//! Black-box: run the real `sy knowledge waybar` against an ephemeral
//! `$XDG_STATE_HOME` and assert what waybar receives on stdout. The tile
//! used to answer `{"class":"hidden","text":""}` for a dead daemon, and
//! `#custom-sy-knowledge.hidden` in `configs/waybar/style.css` zeroes its
//! padding and width — so a plane that had been crash-looping for four days
//! was pixel-identical to a rice that never configured the applet.
//!
//! Contract now:
//!   * no status file at all  -> `hidden`  ("this plane never ran here")
//!   * stale / stopped daemon -> `down`    (visible, glyph kept, names `sy doctor`)
//!   * live daemon            -> `idle` / `indexing` / `paused` / `error`

use std::process::Command;

use serde_json::Value;

/// Minimal `aiplane::status::Status` document: fields without serde
/// defaults are required, everything else is defaulted on parse.
fn status_json(ts_unix: u64, daemon_running: bool) -> String {
    format!(
        r#"{{
            "ts_unix": {ts_unix},
            "daemon_running": {daemon_running},
            "qdrant_ready": true,
            "schedule_secs": 1800,
            "next_run_unix": {ts_unix},
            "sources_explicit": 0,
            "sources_discover": 3,
            "manifests_active": 8,
            "manifests_disabled": 0,
            "points": 112292,
            "indexing": false,
            "embed_backend": "vitisai",
            "last_error": null
        }}"#
    )
}

/// Run `sy knowledge waybar` against `state_home` and parse its stdout line.
fn tile(state_home: &std::path::Path) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_sy"))
        .args(["knowledge", "waybar"])
        .env("XDG_STATE_HOME", state_home)
        .output()
        .expect("spawn sy");
    assert!(
        out.status.success(),
        "sy knowledge waybar failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let line = stdout.lines().next().expect("exactly one JSON line");
    serde_json::from_str(line).unwrap_or_else(|e| panic!("waybar JSON: {e}\n{line}"))
}

fn write_status(state_home: &std::path::Path, body: &str) {
    let dir = state_home.join("sy/aiplane");
    std::fs::create_dir_all(&dir).expect("create state dir");
    std::fs::write(dir.join("status.json"), body).expect("write status");
}

#[test]
fn absent_plane_collapses_the_tile() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let tile = tile(tmp.path());
    assert_eq!(tile["class"], "hidden");
    assert_eq!(tile["text"], "");
}

#[test]
fn stopped_daemon_renders_a_visible_down_tile() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    // Fresh timestamp, `daemon_running: false` — the snapshot a clean
    // shutdown leaves behind.
    write_status(tmp.path(), &status_json(now, false));

    let tile = tile(tmp.path());
    assert_eq!(tile["class"], "down", "got {tile}");
    assert!(
        tile["text"].as_str().is_some_and(|t| !t.is_empty()),
        "a dead plane must not render an empty tile: {tile}"
    );
    assert!(
        tile["text"].as_str().is_some_and(|t| t.contains('!')),
        "down tile carries the flag: {}",
        tile["text"]
    );
    let tip = tile["tooltip"].as_str().expect("tooltip");
    assert!(tip.contains("daemon down"), "{tip}");
    assert!(
        tip.contains("sy doctor"),
        "tooltip must be actionable: {tip}"
    );
}

#[test]
fn stale_snapshot_is_also_reported_as_down() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    // 4 days old and claiming to run — exactly the incident's file.
    write_status(tmp.path(), &status_json(now - 4 * 86_400, true));

    let tile = tile(tmp.path());
    assert_eq!(tile["class"], "down", "got {tile}");
    assert!(
        tile["tooltip"]
            .as_str()
            .is_some_and(|t| t.contains("last status")),
        "{}",
        tile["tooltip"]
    );
}

#[test]
fn live_daemon_keeps_the_count_and_a_non_down_class() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    write_status(tmp.path(), &status_json(now, true));

    let tile = tile(tmp.path());
    assert_eq!(tile["class"], "idle", "got {tile}");
    let text = tile["text"].as_str().expect("text");
    assert!(text.contains('🧠'), "{text}");
    assert!(text.contains("112k"), "point count shown: {text}");
}
