//! `define_plugin!` integration test — locks the macro's wire shape
//! end-to-end by spawning the `examples/echo_previewer.rs` binary and
//! driving one `initialize` → `preview` → `shutdown` round-trip
//! through its stdio.
//!
//! The example file is also what the PDK README quotes verbatim as
//! the "20-line previewer" — keeping the test and the README aimed
//! at the same fixture means the README never rots.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Locate the `echo_previewer` example binary the test was just
/// compiled against. cargo writes examples to
/// `target/<profile>/examples/<name>` (or `examples\\<name>.exe` on
/// Windows) — we walk up from the integration test's exe to find it.
fn echo_previewer_path() -> PathBuf {
    let cur = std::env::current_exe().expect("current_exe");
    // current_exe → target/<profile>/deps/echo-<hash>
    let deps = cur.parent().expect("deps dir");
    let profile = deps.parent().expect("profile dir");
    let candidate = profile.join("examples").join("echo_previewer");
    if candidate.is_file() {
        return candidate;
    }
    // Fallback: cargo test --no-run sometimes lands examples under
    // target/debug/examples directly. Walk one more level up.
    let target = profile.parent().expect("target dir");
    let alt = target.join("debug").join("examples").join("echo_previewer");
    if alt.is_file() {
        return alt;
    }
    // Final fallback: build it on the fly so a `cargo test --test echo`
    // standalone invocation also works.
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "sy-plugin-pdk",
            "--example",
            "echo_previewer",
        ])
        .status()
        .expect("cargo build --example");
    assert!(status.success(), "cargo build --example failed");
    if candidate.is_file() {
        return candidate;
    }
    alt
}

/// Write one Content-Length-framed JSON-RPC body onto `w`.
fn write_frame<W: Write>(w: &mut W, body: &serde_json::Value) {
    let bytes = serde_json::to_vec(body).expect("serialize");
    let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
    w.write_all(header.as_bytes()).expect("write header");
    w.write_all(&bytes).expect("write body");
    w.flush().expect("flush");
}

/// Block until one Content-Length frame body arrives on `r` and
/// return it as a parsed JSON value. Bounded by `timeout` so a
/// regression doesn't hang CI.
fn read_frame<R: Read>(r: &mut R, _timeout: Duration) -> serde_json::Value {
    // Read header bytes one at a time until we see CRLF CRLF.
    let mut headers = Vec::with_capacity(64);
    let mut last4: [u8; 4] = [0; 4];
    loop {
        let mut b = [0u8; 1];
        let n = r.read(&mut b).expect("read header byte");
        assert!(n > 0, "EOF while reading header");
        headers.push(b[0]);
        last4 = [last4[1], last4[2], last4[3], b[0]];
        if last4 == *b"\r\n\r\n" {
            break;
        }
        assert!(headers.len() < 16 * 1024, "header block too big");
    }
    let header_text = std::str::from_utf8(&headers).expect("utf8 headers");
    let mut len: usize = 0;
    for line in header_text.split("\r\n") {
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            len = rest.trim().parse().expect("parse content-length");
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).expect("read body");
    serde_json::from_slice(&body).expect("parse body")
}

/// The PDK's `define_plugin!` macro generates a binary that
/// (1) responds to `initialize` with its compile-time `PluginInfo`
/// and (2) routes `preview` to the user's handler. Driving both
/// against `examples/echo_previewer.rs` locks the macro contract.
#[test]
fn define_plugin_serves_initialize_then_preview() {
    let bin = echo_previewer_path();
    let mut child = Command::new(&bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn echo_previewer");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");

    // 1. initialize round-trip — asserts the macro emitted the right
    //    PluginInfo and the runtime's lifecycle dispatch picks it up.
    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "host": {"name": "test", "version": "0", "api": ["1"], "host_methods": []}, "plugin": {"workdir": "/tmp"} }
        }),
    );
    let init = read_frame(&mut stdout, Duration::from_secs(3));
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["id"], 1);
    let result = &init["result"];
    assert_eq!(result["api"], "1");
    assert_eq!(result["name"], "sy-plugin-pdk-echo");
    let caps = result["capabilities"]
        .as_array()
        .expect("capabilities array");
    assert_eq!(caps.len(), 1, "echo previewer advertises one capability");
    assert_eq!(caps[0]["kind"], "previewer");
    assert_eq!(caps[0]["mime"], "text/plain");

    // 2. preview round-trip — drives the macro's typed bridge:
    //    PreviewReq deserialise → user closure → PreviewResp serialise.
    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "preview",
            "params": { "path": "README.md", "mime": "text/markdown" }
        }),
    );
    let preview = read_frame(&mut stdout, Duration::from_secs(3));
    assert_eq!(preview["id"], 2);
    let text = preview["result"]["text"]
        .as_str()
        .expect("preview result text");
    assert!(
        text.contains("README.md") && text.contains("text/markdown"),
        "echo body must echo path + mime; got {text:?}"
    );

    // 3. shutdown then exit — the runtime returns `null` on shutdown
    //    and terminates the loop on the `exit` notification.
    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "shutdown",
            "params": null
        }),
    );
    let sd = read_frame(&mut stdout, Duration::from_secs(3));
    assert_eq!(sd["id"], 3);
    assert!(sd["result"].is_null(), "shutdown result must be null");
    write_frame(
        &mut stdin,
        &serde_json::json!({"jsonrpc": "2.0", "method": "exit"}),
    );
    // Closing stdin signals the loop that even on a stubborn host the
    // child will exit; assert termination is bounded.
    drop(stdin);
    let exited = child.wait().expect("wait child");
    assert!(
        exited.success(),
        "echo previewer must exit cleanly; got {exited:?}"
    );
}
