//! Roadmap Step 22 — `configs/systemd/user/sy-file.{service,socket}`
//! productivised under the same `sy apply` machinery the rest of the
//! sy.target group rides on.
//!
//! Two tests:
//!
//! 1. [`unit_renders`] — `sync_units(dry_run=true)` walks the canonical
//!    `configs/systemd/user/` source tree and surfaces both new unit
//!    files (`sy-file.service` + `sy-file.socket`) in the planned-ops
//!    diff. This is the literal `sy apply --dry-run` path a fresh
//!    Fedora 43 install will hit, asserting the unit lands without any
//!    special supervisor wiring (the walker auto-picks up new files).
//! 2. [`activation_via_socket_connect`] — spawns the daemon with
//!    `--systemd-notify` against a fake `$NOTIFY_SOCKET` datagram
//!    listener and asserts the `READY=1` lifecycle byte arrives after
//!    bind + chmod. This proves the SPEC §4.5 `Type=notify` contract
//!    the `.service` unit relies on: systemd flips `activating` →
//!    `active` only after the daemon emits READY, and a missing emit
//!    would leave the unit stuck in `activating` forever.
//!
//! End-to-end socket-activation against a live `systemctl --user`
//! lives in `tests/sy_file_journey_e2e.rs::step22_…` (the journey
//! beat); this file covers the unit-level wiring around it.

#[path = "../src/supervision/apply.rs"]
#[allow(dead_code)]
mod apply;

use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;

/// Repo-relative source dir the apply walker scans. Same constant the
/// production `sy apply` CLI passes into `sync_units`.
const SOURCE_DIR: &str = "configs/systemd/user";

/// Step 22 DoD bullet 1 — `sy apply --dry-run` must surface both new
/// unit files in the planned-ops diff so an operator running the
/// canonical install flow sees the supervisor pick them up.
#[test]
fn unit_renders() {
    let td = TempDir::new().expect("tempdir for target_dir");
    let src = PathBuf::from(SOURCE_DIR)
        .canonicalize()
        .expect("canonicalize configs/systemd/user");
    let tgt = td.path().join("systemd-user");
    std::fs::create_dir_all(&tgt).expect("mkdir target_dir");

    let opts = apply::ApplyOpts {
        source_dir: src,
        target_dir: tgt.clone(),
        legacy_system_path: td.path().join("nonexistent-legacy"),
        dry_run: true,
        yes: false,
        daemon_reload: false,
    };
    let diff = apply::sync_units(&opts).expect("sync_units must succeed");

    // The target dir is empty, so both new units land in `created`.
    let basenames: Vec<String> = diff
        .created
        .iter()
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
        .collect();
    assert!(
        basenames.iter().any(|n| n == "sy-file.service"),
        "sync_units must surface sy-file.service in `created`: {basenames:?}"
    );
    assert!(
        basenames.iter().any(|n| n == "sy-file.socket"),
        "sync_units must surface sy-file.socket in `created`: {basenames:?}"
    );
}

/// Step 22 DoD — the daemon must emit `READY=1` to `$NOTIFY_SOCKET`
/// after bind + chmod when invoked with `--systemd-notify`. Without
/// that the `Type=notify` service would hang in `activating` forever.
///
/// We bind a `UnixDatagram` listener at a tempdir path, set
/// `NOTIFY_SOCKET` to it, spawn the daemon binary with
/// `ipc serve --sock <path> --systemd-notify`, and assert the
/// notification body arrives inside 5 s.
#[test]
fn activation_via_socket_connect() {
    use std::os::unix::net::UnixDatagram;
    use std::process::{Command, Stdio};

    let td = TempDir::new().expect("tempdir");
    let notify_path = td.path().join("notify.sock");
    let sock_path = td.path().join("sy-file.sock");

    let listener = UnixDatagram::bind(&notify_path).expect("bind fake NOTIFY_SOCKET");
    listener
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout on notify listener");

    let bin = env!("CARGO_BIN_EXE_sy");
    let mut child = Command::new(bin)
        .args([
            "file",
            "ipc",
            "serve",
            "--sock",
            sock_path.to_str().expect("sock path utf8"),
            "--systemd-notify",
        ])
        .env("NOTIFY_SOCKET", &notify_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sy file ipc serve");

    let mut buf = [0u8; 256];
    let recv_res = listener.recv(&mut buf);
    // Tear the daemon down regardless of the recv outcome so a
    // failure path doesn't leak a child past the test.
    let _ = child.kill();
    let _ = child.wait();

    let n = recv_res.expect("READY notification must arrive within 5 s");
    let body = std::str::from_utf8(&buf[..n]).expect("notify body is utf-8");
    assert!(
        body.contains("READY=1"),
        "notify body must carry READY=1, got: {body:?}"
    );
}
