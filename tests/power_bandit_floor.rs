//! Integration test for the Step 22 Conservative LinUCB bandit's
//! audit-log + `sy power status` schema contract.
//!
//! The roadmap text asks for "1000-tick daemon-in-thread with
//! fixed-context fake sensors; chosen-arm distribution stays within α
//! of rules baseline." That assertion lives **inline** under
//! `src/power/daemon.rs::tests::bandit_defers_to_baseline_under_no_signal`
//! because the integration-test crate has no access to `sy::power::*`
//! — the binary has no `lib.rs` (see `tests/power_status.rs`).
//!
//! This file covers the SPEC §4 `sy.power.status/v1` `bandit` block
//! byte-compatibility DoD: a fake daemon returns a wire
//! `StatusResponse` whose `last_audit` slot carries the R3-shape
//! `ranked_actions` + `conservative_alpha` fields; `sy power status
//! --json` then renders the `bandit` block with every key the SPEC
//! §4 schema mandates (`chosen_arm`, `ucb_score`, `top3`,
//! `conservative_alpha`, `baseline_arm`). Cross-crate compile
//! together with `power_status.rs` so the wire format and the
//! renderer stay coupled.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

/// `XDG_RUNTIME_DIR` is per-process state; serialise across neighbour
/// tests so they cannot race on the same socket path.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Step 22 DoD: the `bandit` block in `sy power status --json` is
/// byte-compatible with the SPEC §4 schema and reflects the bandit's
/// real ranked actions instead of the Step-11 stub. Drive a fake
/// daemon that returns a `StatusResponse` whose `last_audit` carries
/// three `(arm, ucb)` tuples + a conservative-α of 0.05; assert every
/// SPEC §4 `bandit.*` field is populated.
#[test]
fn bandit_status_block_schema_matches_spec_v1() {
    const EXPECTED_TOP3: usize = 3;
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
        assert_eq!(req["op"].as_str(), Some("Status"));
        // Wire shape mirrors `StatusResponse { last_audit: AuditEntry }`
        // after Step 22's `ranked_actions` + `conservative_alpha`
        // extension. Three top-3 entries in descending order so the
        // renderer's sortedness check holds.
        let resp = serde_json::json!({
            "schema": "sy.power.status/v1",
            "snapshot_hash": "feedface".repeat(8),
            "snapshot": {
                "schema": "sy.power.snapshot/v1",
                "ts": "2026-05-19T12:00:00Z",
                "raw": {
                    "tctl_c": 65.0,
                    "package_power_w": 18.0,
                    "igpu_busy_pct": 8,
                    "npu_workloads": 0,
                    "battery_soc_pct": 95,
                    "ac_online": true,
                },
            },
            "last_audit": {
                "snapshot": {
                    "schema": "sy.power.snapshot/v1",
                    "ts": "2026-05-19T12:00:00Z",
                    "features": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                    "raw": {
                        "tctl_c": 65.0,
                        "package_power_w": 18.0,
                        "igpu_busy_pct": 8,
                        "npu_workloads": 0,
                        "battery_soc_pct": 95,
                        "ac_online": true,
                    },
                    "snapshot_hash": "0".repeat(64),
                },
                "applied_arm": "browse",
                "shield_state": "COOL_AC",
                "reason_chain": ["bandit:browse (ucb=0.21)", "shield:COOL_AC"],
                "ranked_actions": [["browse", 0.21_f32], ["code", 0.18_f32], ["call", 0.16_f32]],
                "conservative_alpha": 0.05_f32,
            },
        });
        let body = serde_json::to_vec(&resp).expect("encode resp");
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .expect("write len");
        stream.write_all(&body).expect("write body");
        stream.flush().expect("flush");
    });

    // The binary's CLI-side `power status` probe runs
    // `snapshot::collect_tick` and feeds the result into
    // `shield::transition`, so the live host can tip the DFA out of
    // `CoolAc` and flip the SPEC §4 `bandit.baseline_arm` away from
    // `"browse"`: a hot CPU (> ~75 °C) → `Hot`, and — the path that
    // actually bit here — a call / screen-cast / media stream on the
    // operator's desktop sets `call_active` → `Meeting` (baseline
    // `"call"`). Setting `SY_SYSFS_ROOT` to an empty tempdir isolates
    // the *whole* probe: sysfs sensors return `Err(...) → None`, and
    // `probe_intent` skips the live D-Bus intent channels, so the DFA
    // deterministically falls through to `CoolAc`. See BUG-20260601-2030.
    let sysfs = tempfile::tempdir().expect("sysfs tempdir");
    let bin = env!("CARGO_BIN_EXE_sy");
    let out = Command::new(bin)
        .args(["power", "status", "--json"])
        .env("XDG_RUNTIME_DIR", tmp.path())
        .env("SY_SYSFS_ROOT", sysfs.path())
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
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("parseable JSON");
    let bandit = &v["bandit"];
    for key in [
        "chosen_arm",
        "ucb_score",
        "top3",
        "conservative_alpha",
        "baseline_arm",
    ] {
        assert!(
            bandit.get(key).is_some(),
            "bandit.{key} must be present (SPEC §4): {v}",
        );
    }
    assert_eq!(bandit["chosen_arm"].as_str(), Some("browse"));
    let top3 = bandit["top3"].as_array().expect("top3 is an array");
    assert_eq!(
        top3.len(),
        EXPECTED_TOP3,
        "top3 must carry exactly {EXPECTED_TOP3} entries: {v}",
    );
    // Each top3 entry is a `[name, score]` tuple per the audit-entry
    // `ranked_actions` shape. Descending order is the contract.
    let scores: Vec<f64> = top3
        .iter()
        .map(|t| t.as_array().expect("tuple")[1].as_f64().expect("score"))
        .collect();
    for w in scores.windows(2) {
        assert!(w[0] >= w[1], "top3 must be descending by score: {scores:?}",);
    }
    assert!(
        (bandit["conservative_alpha"].as_f64().unwrap() - 0.05).abs() < 1e-3,
        "conservative_alpha must mirror the audit entry: {v}",
    );
    // The baseline arm is the rules-baseline pick for the daemon's
    // current shield state. The fake daemon returns COOL_AC ⇒ the
    // shipped `power.toml` baseline is `browse`.
    assert_eq!(bandit["baseline_arm"].as_str(), Some("browse"));
}
