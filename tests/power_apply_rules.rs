//! Integration test for `sy power profile <name>` end-to-end against
//! a fake daemon listening on a tempdir socket. Companion to the
//! Step 19 daemon-in-thread unit tests in
//! `src/power/daemon.rs::tests`.
//!
//! The crate has no `lib.rs`, so integration tests cannot drive the
//! daemon's `one_tick` function directly — that path is covered by
//! `src/power/daemon.rs::tests::{rules_baseline_applies_browse_when_cool_ac,
//! hot_baseline_applies_idle, manual_pin_overrides_baseline,
//! pin_cleared_by_auto, exit_writes_vendor_defaults}`. Here we
//! exercise the WIRE side: the `sy power profile <name>` and
//! `sy power profile --auto` CLI handlers dial the real `sy-powerd`
//! IPC socket, send the `ProfileSet`/`ProfileClear` op, and decode
//! the `ProfileAck` reply. The fake daemon validates the op shape
//! and responds with the expected ack — drift in either direction
//! is caught here without needing a full daemon process.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

/// `XDG_RUNTIME_DIR` is per-process state; serialise across any
/// future neighbour tests in this file.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Step 19: `sy power profile build` sends `ProfileSet { name: "build" }`
/// over IPC and exits 0 on a positive ack. The fake daemon asserts
/// the wire shape and responds with the canonical `ProfileAck` form.
#[test]
fn profile_set_round_trips_to_fake_daemon() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let sock_dir = tmp.path().join("sy");
    std::fs::create_dir_all(&sock_dir).expect("create sock dir");
    let sock = sock_dir.join("powerd.sock");

    let listener = UnixListener::bind(&sock).expect("bind fake daemon");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("rd timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("wr timeout");
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).expect("read body");
        let req: serde_json::Value = serde_json::from_slice(&body).expect("parse req");
        assert_eq!(req["op"].as_str(), Some("ProfileSet"), "op tag: {req}");
        assert_eq!(req["name"].as_str(), Some("build"), "name: {req}");
        let resp = serde_json::json!({
            "schema": "sy.power.status/v1",
            "ok": true,
            "pinned": "build",
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
        .args(["power", "profile", "build"])
        .env("XDG_RUNTIME_DIR", tmp.path())
        .output()
        .expect("spawn sy power profile");
    server.join().expect("server thread join");

    assert!(
        out.status.success(),
        "sy power profile build exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.contains("pinned arm=build"),
        "expected pin confirmation in stdout: {stdout}",
    );
}

/// Step 19: `sy power profile --auto` sends `ProfileClear` and the
/// daemon's ack carries `pinned: null`. The CLI surfaces "rules
/// baseline active" on stdout.
#[test]
fn profile_auto_clears_pin() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let sock_dir = tmp.path().join("sy");
    std::fs::create_dir_all(&sock_dir).expect("create sock dir");
    let sock = sock_dir.join("powerd.sock");

    let listener = UnixListener::bind(&sock).expect("bind fake daemon");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("rd timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("wr timeout");
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).expect("read body");
        let req: serde_json::Value = serde_json::from_slice(&body).expect("parse req");
        assert_eq!(req["op"].as_str(), Some("ProfileClear"), "op tag: {req}");
        let resp = serde_json::json!({
            "schema": "sy.power.status/v1",
            "ok": true,
            "pinned": serde_json::Value::Null,
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
        .args(["power", "profile", "--auto"])
        .env("XDG_RUNTIME_DIR", tmp.path())
        .output()
        .expect("spawn sy power profile --auto");
    server.join().expect("server thread join");

    assert!(
        out.status.success(),
        "sy power profile --auto exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.contains("cleared"),
        "expected clear confirmation in stdout: {stdout}",
    );
}
