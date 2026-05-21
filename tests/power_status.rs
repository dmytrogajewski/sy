//! Integration test for `sy power status --json` end-to-end against
//! a fake "daemon" listening on a tempdir socket (Step 11 of
//! `specs/roadmaps/sy-power`).
//!
//! The fake daemon is intentionally minimal: it binds a blocking
//! `UnixListener` on a tempdir, accepts one connection, reads one
//! length-prefixed JSON request frame, and writes back a canned
//! `StatusResponse` frame. The wire format is documented in
//! `src/power/ipc.rs` (`u32-BE length || JSON body`) — we replicate
//! it here so the test stays decoupled from `sy`'s private module
//! shape (the crate has no `lib.rs`, so integration tests cannot
//! reach `sy::power::*` directly).
//!
//! The `sy` binary under test is discovered via `CARGO_BIN_EXE_sy`
//! (set by cargo for integration tests).

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

/// `XDG_RUNTIME_DIR` is per-process state; serialise across any
/// future neighbour tests in this file.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Step 11 DoD bullet 1: `sy power status --json` returns the
/// documented schema with values supplied by the daemon, exit 0.
#[test]
fn status_json_round_trips_against_fake_daemon() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let sock_dir = tmp.path().join("sy");
    std::fs::create_dir_all(&sock_dir).expect("create sock dir");
    let sock = sock_dir.join("powerd.sock");

    let listener = UnixListener::bind(&sock).expect("bind fake daemon");
    // Drive one accept on a background thread so the foreground can
    // exec the CLI under test.
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("rd timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("wr timeout");
        // Read the request frame: u32-BE len || JSON body.
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).expect("read body");
        let req: serde_json::Value = serde_json::from_slice(&body).expect("parse req");
        assert_eq!(req["op"].as_str(), Some("Status"), "wire op pinned");
        // Write a canned StatusResponse. The snapshot carries realistic
        // sensor values so the CLI's renderer has something to format
        // through the SPEC §4 schema.
        let resp = serde_json::json!({
            "schema": "sy.power.status/v1",
            "snapshot_hash": "deadbeef".repeat(8),
            "snapshot": {
                "schema": "sy.power.snapshot/v1",
                "ts": "2026-05-19T12:00:00Z",
                "raw": {
                    "tctl_c": 71.0,
                    "package_power_w": 27.4,
                    "igpu_busy_pct": 4,
                    "npu_workloads": 0,
                    "battery_soc_pct": 100,
                    "ac_online": true,
                },
            },
        });
        let body = serde_json::to_vec(&resp).expect("encode resp");
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .expect("write len");
        stream.write_all(&body).expect("write body");
        stream.flush().expect("flush");
    });

    let bin = env!("CARGO_BIN_EXE_sy");
    let out = Command::new(bin)
        .args(["power", "status", "--json"])
        .env("XDG_RUNTIME_DIR", tmp.path())
        .output()
        .expect("spawn sy power status");
    server.join().expect("server thread join");

    assert!(
        out.status.success(),
        "sy power status --json exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("status --json must emit parseable JSON");
    assert_eq!(v["schema"].as_str(), Some("sy.power.status/v1"));
    // Every SPEC §4 top-level key must be present.
    for key in [
        "schema",
        "ts",
        "onboarding",
        "model",
        "shield_state",
        "activity_label",
        "forecast",
        "bandit",
        "applied_policy",
        "sensors",
        "drift",
    ] {
        assert!(v.get(key).is_some(), "missing top-level key {key}: {v}");
    }
    // Sensor values from the wire snapshot must flow through.
    assert!(
        (v["sensors"]["package_power_w_5tap"].as_f64().unwrap() - 27.4).abs() < 1e-3,
        "sensors must flow through from snapshot.raw: {v}"
    );
    assert_eq!(v["sensors"]["battery_pct"].as_u64(), Some(100));
}
