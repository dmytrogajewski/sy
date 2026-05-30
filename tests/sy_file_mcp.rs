//! Integration tests for `sy file mcp` — the SPEC §4.3 MCP server
//! exposing the eleven `file_*` tools. Each test pins one Step 21
//! acceptance criterion:
//!
//!   * `file_list_round_trip` — happy-path transcoder against a
//!     canned [`FileDaemonClient`] stub.
//!   * `file_copy_then_op_cancel` — two-step lifecycle exercise.
//!   * `file_preview_returns_png_base64` — `sy-plugin-md` fallback
//!     path: stub returns a base64-encoded PNG header; the MCP
//!     handler must preserve the body byte-for-byte.
//!   * `file_search_falls_back_to_filename_when_knowledge_down` —
//!     surfaces `knowledge_status` through the response shape per
//!     SPEC §4.3.
//!
//! The `sy` bin has no `lib.rs`, so we pull the module in via
//! `#[path]` — same pattern `tests/sy_file_journey_e2e.rs` uses for
//! the rest of the file plane.

#[path = "../src/file/mcp.rs"]
#[allow(dead_code)]
mod file_mcp;

/// `crate::file::cli::resolve_sock_path` shim. `mcp.rs` references
/// this from `SyIpcClient::from_env`; the integration-test binary
/// won't call that path (we use a stub [`FileDaemonClient`]), but
/// the compile-time reference still has to resolve. A minimal shim
/// keeps the integration build self-contained.
#[allow(dead_code)]
pub(crate) mod file {
    pub(crate) mod cli {
        use std::path::PathBuf;
        pub fn resolve_sock_path() -> PathBuf {
            PathBuf::from("/tmp/sy-file-mcp-test.sock")
        }
    }
}

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Value};

use file_mcp::{run_with, FileDaemonClient};

/// Canned-response stub. The test seeds `(method, response)` pairs;
/// every `call(method, _)` pops one entry and asserts the request
/// shape via the recorded log so the test body can pin both the
/// outgoing transcode AND the inbound response handling.
struct StubClient {
    responses: RefCell<HashMap<String, Vec<Result<Value, String>>>>,
    calls: RefCell<Vec<(String, Value)>>,
}

impl StubClient {
    fn new() -> Self {
        Self {
            responses: RefCell::new(HashMap::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn expect(&self, method: &str, response: Value) {
        self.responses
            .borrow_mut()
            .entry(method.to_string())
            .or_default()
            .push(Ok(response));
    }

    fn expect_err(&self, method: &str, message: &str) {
        self.responses
            .borrow_mut()
            .entry(method.to_string())
            .or_default()
            .push(Err(message.to_string()));
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.borrow().clone()
    }
}

impl FileDaemonClient for StubClient {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.calls
            .borrow_mut()
            .push((method.to_string(), params.clone()));
        let mut bag = self.responses.borrow_mut();
        let Some(queue) = bag.get_mut(method) else {
            return Err(anyhow::anyhow!(
                "stub: no canned response for method {method}"
            ));
        };
        if queue.is_empty() {
            return Err(anyhow::anyhow!("stub: queue empty for method {method}"));
        }
        match queue.remove(0) {
            Ok(v) => Ok(v),
            Err(msg) => Err(anyhow::anyhow!(msg)),
        }
    }
}

/// Drive the line-delimited stdio loop against an injected stub and
/// return every response frame parsed as JSON. The helper is the
/// same shape `src/power/mcp.rs::tests::stdio_handshake_round_trips`
/// uses — buffer in, buffer out, lines split on `\n`.
fn drive(client: &dyn FileDaemonClient, requests: &[Value]) -> Vec<Value> {
    let mut input = String::new();
    for req in requests {
        input.push_str(&serde_json::to_string(req).expect("serialise req"));
        input.push('\n');
    }
    let mut buf: Vec<u8> = Vec::new();
    run_with(client, input.as_bytes(), &mut buf).expect("mcp loop terminates cleanly");
    std::str::from_utf8(&buf)
        .expect("utf8")
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("response is JSON"))
        .collect()
}

fn tools_call(name: &str, args: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": args }
    })
}

fn structured(resp: &Value) -> Value {
    resp["result"]["structuredContent"].clone()
}

/// `file_list` happy path. Stub returns a real `file.list` response
/// (the daemon's primary path); the MCP transcoder must surface the
/// `entries` array on the structured-content side without mutating
/// it.
#[test]
fn file_list_round_trip() {
    let stub = StubClient::new();
    stub.expect(
        "file.list",
        json!({
            "entries": [
                { "name": "Cargo.toml", "mime": "text/x-toml", "size": 1024, "mtime": 1700000000 },
                { "name": "README.md",  "mime": "text/markdown", "size": 512,  "mtime": 1700000001 }
            ]
        }),
    );

    let responses = drive(&stub, &[tools_call("file_list", json!({ "path": "/tmp" }))]);
    assert_eq!(responses.len(), 1, "one response per request");
    let payload = structured(&responses[0]);
    let entries = payload["entries"]
        .as_array()
        .expect("file_list must surface an `entries` array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["name"].as_str(), Some("Cargo.toml"));
    assert_eq!(entries[1]["name"].as_str(), Some("README.md"));
    assert_eq!(
        responses[0]["result"]["isError"].as_bool(),
        Some(false),
        "happy path must not set isError"
    );

    // The first dial must hit `file.list` with the literal path
    // argument — the SPEC §4.3 wire mapping the doc pins.
    let calls = stub.calls();
    assert_eq!(calls[0].0, "file.list", "first IPC call must be file.list");
    assert_eq!(calls[0].1["path"].as_str(), Some("/tmp"));
}

/// `file_copy` returns an `op_id`; the agent can immediately
/// `file_op_cancel` against that id and observe the daemon's `ok:
/// true` ack. Both round-trips travel through the MCP envelope.
#[test]
fn file_copy_then_op_cancel() {
    let stub = StubClient::new();
    stub.expect("file.copy", json!({ "op_id": 42 }));
    stub.expect("file.op_cancel", json!({ "ok": true }));

    let copy_resp = drive(
        &stub,
        &[tools_call(
            "file_copy",
            json!({
                "sources": ["/tmp/src/a"],
                "dest": "/tmp/dst",
                "conflict": "skip"
            }),
        )],
    );
    let copy_payload = structured(&copy_resp[0]);
    let op_id = copy_payload["op_id"]
        .as_u64()
        .expect("file_copy must return an op_id");
    assert_eq!(op_id, 42, "stub returned op_id=42");

    let cancel_resp = drive(
        &stub,
        &[tools_call("file_op_cancel", json!({ "op_id": op_id }))],
    );
    let cancel_payload = structured(&cancel_resp[0]);
    assert_eq!(
        cancel_payload["ok"].as_bool(),
        Some(true),
        "file_op_cancel must surface ok=true on the structured side"
    );

    // Verify both calls hit the right wire methods with the right
    // params. Drift here would silently dead-route an agent call.
    let calls = stub.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "file.copy");
    assert_eq!(calls[0].1["sources"].as_array().map(|a| a.len()), Some(1));
    assert_eq!(calls[0].1["dest"].as_str(), Some("/tmp/dst"));
    assert_eq!(calls[0].1["conflict"].as_str(), Some("skip"));
    assert_eq!(calls[1].0, "file.op_cancel");
    assert_eq!(calls[1].1["op_id"].as_u64(), Some(42));
}

/// `file_preview` returns `{ mime, png_base64 }`. We use the
/// **documented fallback**: a stub that returns a base64 string
/// encoding the 8-byte PNG signature. The MCP transcoder must
/// preserve the body byte-for-byte; we verify by decoding the
/// returned string and asserting it begins with `\x89PNG\r\n\x1a\n`.
///
/// Rationale (per roadmap): the full chain (real daemon + spawned
/// `sy-plugin-md`) is brittle inside one unit test and the daemon's
/// `file.preview` op deliberately leaves `png_base64 = ""` until
/// Step 27 wires the plugin dispatcher. Stubbing the daemon-side
/// proves the **MCP transcoder honours the SPEC §4.3 shape** —
/// which is the contract this test owns.
#[test]
fn file_preview_returns_png_base64() {
    // PNG signature + a tiny IHDR-like payload. We don't need a
    // real PNG; we just need the first 8 bytes to match the magic
    // header so the assertion is meaningful.
    let png_bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
    let encoded = base64_encode_for_test(&png_bytes);

    let stub = StubClient::new();
    stub.expect(
        "file.preview",
        json!({
            "mime": "image/png",
            "png_base64": encoded.clone(),
        }),
    );

    let resp = drive(
        &stub,
        &[tools_call(
            "file_preview",
            json!({ "path": "/tmp/README.md", "max_width": 800, "max_height": 600 }),
        )],
    );
    let payload = structured(&resp[0]);
    let mime = payload["mime"]
        .as_str()
        .expect("file_preview must return a mime string");
    assert_eq!(mime, "image/png", "stub returned image/png");
    let b64 = payload["png_base64"]
        .as_str()
        .expect("file_preview must carry png_base64");
    assert!(!b64.is_empty(), "png_base64 must be non-empty");
    assert_eq!(
        b64, encoded,
        "MCP transcoder must preserve the body byte-for-byte"
    );

    let decoded = base64_decode_for_test(b64);
    assert!(
        decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
        "png_base64 must decode to a valid PNG header (got first 8 bytes: {:?})",
        &decoded[..8.min(decoded.len())]
    );

    // Wire mapping pin: `max_width` / `max_height` must propagate
    // verbatim so the daemon-side previewer can read them.
    let calls = stub.calls();
    assert_eq!(calls[0].0, "file.preview");
    assert_eq!(calls[0].1["path"].as_str(), Some("/tmp/README.md"));
    assert_eq!(calls[0].1["max_width"].as_u64(), Some(800));
    assert_eq!(calls[0].1["max_height"].as_u64(), Some(600));
}

/// `file_search` with `knowledge: true` against a daemon whose
/// knowledge plane is down. The daemon returns the filename-match
/// result set alongside a `knowledge_status: "down"` field; the MCP
/// transcoder must surface both so the agent can branch on the
/// status without losing the results.
#[test]
fn file_search_falls_back_to_filename_when_knowledge_down() {
    let stub = StubClient::new();
    stub.expect(
        "file.search",
        json!({
            "results": ["/tmp/notes/OOM-tuning.md", "/tmp/notes/oom.txt"],
            "knowledge_status": "down"
        }),
    );

    let resp = drive(
        &stub,
        &[tools_call(
            "file_search",
            json!({
                "query": "OOM",
                "root": "/tmp/notes",
                "knowledge": true
            }),
        )],
    );
    let payload = structured(&resp[0]);
    let results = payload["results"]
        .as_array()
        .expect("file_search must return a results array");
    assert_eq!(
        results.len(),
        2,
        "filename fallback must still return both matches"
    );
    assert_eq!(
        payload["knowledge_status"].as_str(),
        Some("down"),
        "knowledge_status must propagate to the MCP envelope"
    );

    let calls = stub.calls();
    assert_eq!(calls[0].0, "file.search");
    assert_eq!(calls[0].1["query"].as_str(), Some("OOM"));
    assert_eq!(calls[0].1["root"].as_str(), Some("/tmp/notes"));
    assert_eq!(calls[0].1["knowledge"].as_bool(), Some(true));
}

/// Bonus negative-path coverage: a daemon-side error surfaces as the
/// MCP `isError: true` envelope rather than a JSON-RPC `error` frame.
/// Locked in so agents see one consistent shape across tool failures.
#[test]
fn daemon_error_surfaces_via_is_error_envelope() {
    let stub = StubClient::new();
    stub.expect_err("file.open", "sy-file daemon unreachable at /tmp/x.sock");

    let resp = drive(&stub, &[tools_call("file_open", json!({ "path": "/x" }))]);
    let frame = &resp[0];
    assert!(
        frame["error"].is_null(),
        "transport-level error frame must be null on tool failures: {frame}"
    );
    assert_eq!(
        frame["result"]["isError"].as_bool(),
        Some(true),
        "tool failure must set isError=true"
    );
    let content = frame["result"]["content"]
        .as_array()
        .expect("isError envelope must carry a content array");
    let text = content[0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    assert!(
        text.contains("sy-file daemon unreachable"),
        "error text must surface the daemon message: {text}"
    );
}

/// Tiny base64 encoder/decoder pair, mirroring the
/// `host_fns::base64_encode` alphabet so the test asserts a wire
/// contract independent of any external crate. Same rationale
/// `tests/sy_file_journey_e2e.rs::base64_decode_for_test` calls out.
fn base64_encode_for_test(input: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let (b0, b1, b2) = match c.len() {
            3 => (c[0], c[1], c[2]),
            2 => (c[0], c[1], 0),
            1 => (c[0], 0, 0),
            _ => unreachable!(),
        };
        let n: u32 = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        match c.len() {
            3 => {
                out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
                out.push(ALPHA[(n & 0x3f) as usize] as char);
            }
            2 => {
                out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
                out.push('=');
            }
            1 => {
                out.push('=');
                out.push('=');
            }
            _ => unreachable!(),
        }
    }
    out
}

fn base64_decode_for_test(s: &str) -> Vec<u8> {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (i, b) in ALPHA.iter().enumerate() {
        table[*b as usize] = i as u8;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for c in bytes.chunks(4) {
        if c.len() != 4 {
            break;
        }
        let mut chunk = [0u8; 4];
        let mut pad = 0;
        for (i, b) in c.iter().enumerate() {
            if *b == b'=' {
                pad += 1;
                chunk[i] = 0;
            } else {
                chunk[i] = table[*b as usize];
            }
        }
        let n: u32 = (u32::from(chunk[0]) << 18)
            | (u32::from(chunk[1]) << 12)
            | (u32::from(chunk[2]) << 6)
            | u32::from(chunk[3]);
        out.push(((n >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((n & 0xff) as u8);
        }
    }
    out
}
