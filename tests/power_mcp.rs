//! Integration test for `sy power mcp` end-to-end (Step 38 of
//! `specs/roadmaps/sy-power`).
//!
//! Spawn `sy power mcp` with stdin/stdout piped; in parallel run a
//! fake daemon on a tempdir Unix socket so the MCP server can dial it
//! through the same path `sy power status` uses. Drive the documented
//! MCP handshake — `initialize` → `tools/list` → `tools/call
//! power_status` — and assert the response carries the
//! `sy.power.status/v1` schema id.
//!
//! Mirrors `tests/power_status.rs`'s fake daemon shape — the wire
//! format (`u32-BE length || JSON body`) is replicated inline so the
//! test stays decoupled from `sy`'s private modules.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

/// `XDG_RUNTIME_DIR` is per-process state; serialise tests that mutate it.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Spawn the binary, drive the three-step MCP handshake, assert the
/// `power_status` content parses as a `sy.power.status/v1` document
/// carrying the SPEC §4 top-level keys.
#[test]
fn mcp_handshake_returns_power_status_v1() {
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
        let resp = serde_json::json!({
            "schema": "sy.power.status/v1",
            "snapshot_hash": "cafebabe".repeat(8),
            "snapshot": {
                "schema": "sy.power.snapshot/v1",
                "ts": "2026-05-19T12:00:00Z",
                "raw": {
                    "tctl_c": 65.0,
                    "package_power_w": 18.2,
                    "igpu_busy_pct": 0,
                    "npu_workloads": 0,
                    "battery_soc_pct": 92,
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
    let mut child = Command::new(bin)
        .args(["power", "mcp"])
        .env("XDG_RUNTIME_DIR", tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sy power mcp");

    let mut stdin = child.stdin.take().expect("stdin handle");
    let stdout = child.stdout.take().expect("stdout handle");

    // Write all three requests, then close stdin so the server exits.
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .expect("write init");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#
    )
    .expect("write list");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"power_status","arguments":{{}}}}}}"#
    )
    .expect("write call");
    drop(stdin);

    let mut reader = BufReader::new(stdout);
    let mut lines = Vec::new();
    for _ in 0..3 {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read line");
        assert!(n > 0, "premature EOF");
        lines.push(line);
    }
    let status = child.wait().expect("wait child");
    server.join().expect("server thread join");
    assert!(status.success(), "sy power mcp exit={:?}", status.code());

    let init: serde_json::Value = serde_json::from_str(lines[0].trim()).expect("init JSON");
    assert_eq!(
        init["result"]["serverInfo"]["name"].as_str(),
        Some("sy-power")
    );
    assert!(init["result"]["protocolVersion"].as_str().is_some());

    let list: serde_json::Value = serde_json::from_str(lines[1].trim()).expect("list JSON");
    let tools = list["result"]["tools"].as_array().expect("tools is array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"].as_str(), Some("power_status"));
    assert!(
        tools[0]["inputSchema"]["properties"]
            .as_object()
            .expect("properties object")
            .is_empty(),
        "power_status takes no arguments"
    );

    let call: serde_json::Value = serde_json::from_str(lines[2].trim()).expect("call JSON");
    assert_eq!(call["result"]["isError"].as_bool(), Some(false));
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .expect("text payload");
    let parsed: serde_json::Value =
        serde_json::from_str(text).expect("power_status content is JSON");
    assert_eq!(parsed["schema"].as_str(), Some("sy.power.status/v1"));
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
        assert!(
            parsed.get(key).is_some(),
            "v1 schema missing key {key:?}: {parsed}"
        );
    }
    assert!(
        (parsed["sensors"]["package_power_w_5tap"].as_f64().unwrap() - 18.2).abs() < 1e-3,
        "sensors must flow through: {parsed}"
    );
}
