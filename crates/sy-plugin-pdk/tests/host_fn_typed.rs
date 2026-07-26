//! `host::fs::read` typed-return integration test — drives the
//! `examples/host_fn_reader.rs` previewer through one
//! `initialize → preview → (plugin emits host.fs.read) → host
//! responds → plugin emits preview result` round-trip and asserts the
//! typed bytes round-trip is byte-identical to what the host wrote
//! into the simulated `host.fs.read` reply.
//!
//! Locks the contract that a PDK user gets `Result<Vec<u8>, RpcError>`
//! out of the host fn surface — no JSON-RPC boilerplate at the
//! call site.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Locate the `host_fn_reader` example binary the test was just
/// compiled against. cargo writes examples to
/// `target/<profile>/examples/<name>`. Falls back to a `cargo build
/// --example` on a `cargo test --test host_fn_typed` standalone run.
fn host_fn_reader_path() -> PathBuf {
    let cur = std::env::current_exe().expect("current_exe");
    let deps = cur.parent().expect("deps dir");
    let profile = deps.parent().expect("profile dir");
    let candidate = profile.join("examples").join("host_fn_reader");
    if candidate.is_file() {
        return candidate;
    }
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "sy-plugin-pdk",
            "--example",
            "host_fn_reader",
        ])
        .status()
        .expect("cargo build --example");
    assert!(status.success(), "cargo build --example failed");
    candidate
}

/// RFC 4648 base64 encoder for the simulated `host.fs.read` reply
/// body. The host's real implementation in
/// `src/plugin/host_fns.rs` and the PDK's decoder in
/// `crates/sy-plugin-pdk/src/runtime.rs` are byte-compatible — both
/// follow the same RFC 4648 standard alphabet.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n: u32 = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn write_frame<W: Write>(w: &mut W, body: &serde_json::Value) {
    let bytes = serde_json::to_vec(body).expect("serialize");
    let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
    w.write_all(header.as_bytes()).expect("write header");
    w.write_all(&bytes).expect("write body");
    w.flush().expect("flush");
}

fn read_frame<R: Read>(r: &mut R) -> serde_json::Value {
    let mut headers = Vec::with_capacity(64);
    let mut last4: [u8; 4] = [0; 4];
    loop {
        let mut b = [0u8; 1];
        let n = r.read(&mut b).expect("read header byte");
        assert!(n > 0, "EOF mid-header");
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

/// Bytes the simulated host returns from `host.fs.read`. Chosen so the
/// previewer's `format!("got {} bytes: {text}", ...)` body is
/// deterministic and easy to assert on.
const FIXTURE_BYTES: &[u8] = b"hello-pdk";

#[test]
fn host_fs_read_returns_typed_vec_u8() {
    let bin = host_fn_reader_path();
    let mut child = Command::new(&bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn host_fn_reader");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");

    // 1. initialize handshake — the macro-generated runtime answers
    //    from compile-time `PluginInfo`; we don't assert on the body
    //    here (echo.rs covers that) — just consume the frame.
    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "host": {"name": "test", "version": "0", "api": ["1"], "host_methods": ["host.fs.read"]}, "plugin": {"workdir": "/tmp"} }
        }),
    );
    let _init = read_frame(&mut stdout);

    // 2. Drive a `preview` request. The plugin's handler calls
    //    `host::fs::read("README.md")` inside; that posts a
    //    `host.fs.read` request onto stdout addressed back to us.
    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "preview",
            "params": { "path": "README.md" }
        }),
    );

    // 3. Read the next frame — it's the plugin's `host.fs.read`
    //    request. Reply with a base64-encoded fixture body.
    let host_req = read_frame(&mut stdout);
    assert_eq!(host_req["method"], "host.fs.read");
    assert_eq!(host_req["params"]["path"], "README.md");
    let host_req_id = host_req["id"].as_i64().expect("host req id");
    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": host_req_id,
            "result": { "bytes_base64": base64_encode(FIXTURE_BYTES) }
        }),
    );

    // 4. The plugin's `preview` handler unblocks and emits its
    //    response. Assert the typed bytes round-trip cleanly through
    //    `host::fs::read`'s `Result<Vec<u8>, RpcError>` return.
    let preview = read_frame(&mut stdout);
    assert_eq!(preview["id"], 2);
    let text = preview["result"]["text"]
        .as_str()
        .expect("preview result text");
    let expected = format!(
        "got {} bytes: {}",
        FIXTURE_BYTES.len(),
        std::str::from_utf8(FIXTURE_BYTES).unwrap()
    );
    assert_eq!(
        text, expected,
        "host.fs.read must return typed Vec<u8>; reader echoed {text:?}"
    );

    // 5. Tear down: close stdin → loop exits.
    drop(stdin);
    let _ = thread::spawn(move || {
        // Drain stdout so the child doesn't block on a stalled writer.
        let mut sink = Vec::with_capacity(64);
        let _ = stdout.read_to_end(&mut sink);
    });
    let _ = child.wait_timeout(Duration::from_secs(3));
}

/// Tiny `wait_timeout` helper so this test never hangs on a runtime
/// regression. `std::process::Child::wait` blocks forever — we want a
/// bounded grace period and a hard kill if the child overruns.
trait ChildWaitTimeout {
    fn wait_timeout(&mut self, dur: Duration) -> std::io::Result<()>;
}

impl ChildWaitTimeout for std::process::Child {
    fn wait_timeout(&mut self, dur: Duration) -> std::io::Result<()> {
        let start = std::time::Instant::now();
        loop {
            if let Some(_status) = self.try_wait()? {
                return Ok(());
            }
            if start.elapsed() >= dur {
                let _ = self.kill();
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
