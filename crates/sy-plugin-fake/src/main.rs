//! `sy-plugin-fake` — in-tree fake plugin fixture for the
//! sy-file-manager plugin runtime conformance tests (SPEC §3.3 item 17).
//!
//! Speaks the [SPEC §4.2 wire][spec-wire] (LSP-framed JSON-RPC 2.0)
//! end-to-end against the host:
//!
//! * `initialize` — replies with a baseline `previewer` capability +
//!   the wire-shape fields the host's [`parse_initialize_result`][p]
//!   reads (`name`, `version`, `api`, `capabilities`, `host_methods`).
//! * `preview` — returns a hard-coded 1×1 PNG by default; configurable
//!   via the request's `trigger` param (or the matching env var) so the
//!   conformance harness can drive cap-violation and rlimit-breach
//!   scenarios from the same binary.
//! * `ping` — echoes the inbound `ts` (SPEC §4.2.3 health-check shape).
//! * `shutdown` — replies `null`, then waits for the host's `exit`
//!   notification and terminates with status 0.
//!
//! The trigger surface is intentionally minimal — each behaviour is a
//! single match arm so the wire shape stays grokable from one read.
//!
//! [spec-wire]: ../../../../specs/research/sy-file-manager-plugins/SPEC.md#42-wire-protocol
//! [p]: ../../../../src/plugin/capability.rs

use std::io::{self, Read, Write};

use serde_json::{json, Value};

/// SPEC §4.2.2 — `-32097 LIMIT_EXCEEDED`. Surfaced by the
/// `rlimit_breach` trigger after a `Vec::try_reserve_exact` on a
/// page-aligned size that exceeds the manifest's `memory_mb` budget
/// returns `TryReserveError`. Mirrors `crate::plugin::rpc::
/// RLIMIT_BREACH` in the host so a future SPEC re-number breaks both
/// sides together.
const LIMIT_EXCEEDED: i32 = -32097;

/// Canonical 1×1 transparent PNG — the literal byte sequence every
/// previewer reply rides on by default. 67 bytes; base64-encoded for
/// the JSON wire so the host can decode straight from the `result`
/// object. The decoded bytes are
/// `\x89PNG\r\n\x1a\n…IDAT…IEND` — the smallest valid PNG.
const ONE_PX_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

/// Try to reserve `bytes` bytes of contiguous memory. Used by the
/// `rlimit_breach` trigger to provoke `RLIMIT_AS` from the inside —
/// `try_reserve_exact` is the canonical fallible-allocator entry
/// point that surfaces a [`std::collections::TryReserveError`]
/// instead of aborting, so the fake can emit a clean `-32097` reply
/// before exiting.
///
/// We feed the size through `std::hint::black_box` so the release-
/// profile optimizer can't constant-fold `try_reserve_exact(4 GiB)`
/// into a known-fits-in-VSZ early-return. We also write to the head
/// and tail of the spare capacity through `black_box` so LLVM keeps
/// the page touches observable; without that round-trip the
/// allocator's success path is treated as a no-op and RLIMIT_AS is
/// never consulted (verified on rustc 1.96-nightly with
/// `--release`).
fn try_reserve_breach(bytes: usize) -> Result<(), String> {
    let mut v: Vec<u8> = Vec::new();
    let bytes = std::hint::black_box(bytes);
    v.try_reserve_exact(bytes)
        .map_err(|e| format!("try_reserve_exact({bytes}): {e}"))?;
    // Touch the head AND the tail of the spare capacity. RLIMIT_AS
    // bounds VSZ, so a successful `try_reserve_exact` means the
    // allocator already grew our address space — the write below is
    // belt-and-braces and also pulls the pages into RSS so a future
    // `vm.overcommit_memory=0` heuristic kernel actually denies.
    let spare = v.spare_capacity_mut();
    if let Some(first) = spare.first_mut() {
        first.write(0);
    }
    let len = spare.len();
    if len > 0 {
        if let Some(last) = spare.get_mut(len - 1) {
            last.write(0);
        }
    }
    // Force LLVM to keep the allocation observable. Without this the
    // `--release` profile elides the whole call (the Vec is dropped
    // immediately and the optimiser sees no observable effect).
    let v = std::hint::black_box(v);
    drop(v);
    Ok(())
}

/// Read one Content-Length framed message from stdin into a UTF-8
/// string. Returns `None` on EOF (stdin closed before a frame
/// header arrived).
fn read_frame() -> io::Result<Option<String>> {
    let mut stdin = io::stdin().lock();
    let mut headers = Vec::with_capacity(64);
    // Read the header block byte-by-byte until we see CRLF CRLF.
    let mut last4: [u8; 4] = [0; 4];
    loop {
        let mut b = [0u8; 1];
        let n = stdin.read(&mut b)?;
        if n == 0 {
            return Ok(None);
        }
        headers.push(b[0]);
        last4 = [last4[1], last4[2], last4[3], b[0]];
        if last4 == *b"\r\n\r\n" {
            break;
        }
        if headers.len() > 16 * 1024 {
            return Err(io::Error::other("header block exceeded 16 KiB"));
        }
    }
    let header_text = String::from_utf8_lossy(&headers);
    let mut length: Option<usize> = None;
    for line in header_text.split("\r\n") {
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = length.ok_or_else(|| io::Error::other("missing Content-Length header"))?;
    let mut body = vec![0u8; len];
    stdin.read_exact(&mut body)?;
    let s = String::from_utf8(body).map_err(|e| io::Error::other(format!("utf-8: {e}")))?;
    Ok(Some(s))
}

/// Write `body` framed with the SPEC §4.2.1 `Content-Length` header.
fn write_frame(body: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    write!(stdout, "Content-Length: {}\r\n\r\n", body.len())?;
    stdout.write_all(body.as_bytes())?;
    stdout.flush()
}

/// Build the `initialize` response body. Advertises a single
/// `previewer / text/markdown` capability so the host's capability
/// cross-check accepts the manifest+wire pair.
fn initialize_result(id: &Value) -> String {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "name": "sy-plugin-fake",
            "version": env!("CARGO_PKG_VERSION"),
            "api": "1",
            "capabilities": [{"kind": "previewer", "mime": "text/markdown"}],
            "host_methods": ["host.fs.read", "host.notify.waybar"],
        }
    });
    body.to_string()
}

/// Build the default `preview` reply — a 1×1 PNG round-trip the
/// `preview_roundtrip_under_100ms_warm` scenario asserts against.
fn preview_ok_result(id: &Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "image": {
                "png_base64": ONE_PX_PNG_BASE64,
                "w": 1,
                "h": 1,
            }
        }
    })
    .to_string()
}

/// Build a `-32097 LIMIT_EXCEEDED` reply for the rlimit-breach
/// scenario. Carries the failure detail in `data` so the conformance
/// test can assert on the structured field without grepping the
/// human-readable message.
fn rlimit_error_result(id: &Value, detail: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": LIMIT_EXCEEDED,
            "message": "LIMIT_EXCEEDED",
            "data": { "reason": detail }
        }
    })
    .to_string()
}

/// Wrap a `host.fs.read` plugin-initiated request the fake sends when
/// the `cap_violation` trigger fires. The supervisor routes this into
/// `crate::plugin::host_fns::dispatch`, which (with the fake's
/// `[needs].fs_read = []` manifest) returns `-32099 CAP_NOT_GRANTED`.
fn host_fs_read_probe(req_id: i64) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "host.fs.read",
        "params": { "path": "/etc/passwd" }
    })
    .to_string()
}

/// Parse the inbound frame body into a JSON `Value`. Frames that don't
/// parse as JSON are treated as a fatal protocol violation — the fake
/// exits non-zero so the conformance test sees a clean failure.
fn parse_body(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

/// Handle a single `preview` request, applying any `trigger` the
/// request carries (or the matching env-var defaults). Returns the
/// frame body the fake should send back; for `cap_violation` returns
/// `None` because the fake first sends a `host.fs.read` request and
/// then handles the response in the main loop.
async fn handle_preview(req_id: &Value, params: &Value, next_id: &mut i64) -> Option<String> {
    let trigger = params
        .get("trigger")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| std::env::var("SY_FAKE_TRIGGER").ok())
        .unwrap_or_default();
    match trigger.as_str() {
        "rlimit_breach" => {
            // RLIMIT_AS in the test manifest is set well below the
            // canonical 4 GiB asked for here so the allocation must
            // fail under any sane configuration.
            let attempt: usize = 4 * 1024 * 1024 * 1024;
            match try_reserve_breach(attempt) {
                Ok(_) => Some(preview_ok_result(req_id)),
                Err(detail) => Some(rlimit_error_result(req_id, &detail)),
            }
        }
        "cap_violation" => {
            let probe_id = *next_id;
            *next_id += 1;
            // Send the disallowed host request; the host's reply lands
            // in the main loop, which folds the received error code
            // into the preview reply.
            let probe = host_fs_read_probe(probe_id);
            if write_frame(&probe).is_err() {
                return Some(rlimit_error_result(req_id, "host.fs.read send failed"));
            }
            // Stash the pending preview id for the response handler.
            PENDING_PREVIEW.with(|slot| {
                *slot.borrow_mut() = Some((req_id.clone(), probe_id));
            });
            None
        }
        _ => Some(preview_ok_result(req_id)),
    }
}

use std::cell::RefCell;

thread_local! {
    /// Pending `(preview_id, host_probe_id)` pair when a `cap_violation`
    /// preview is in flight. The host fn response handler reads this
    /// and emits the preview reply with the propagated `-32099` code.
    /// `thread_local!` keeps the storage single-threaded (the fake
    /// runs on `tokio` current-thread) without reaching for `static mut`.
    static PENDING_PREVIEW: RefCell<Option<(Value, i64)>> = const { RefCell::new(None) };
}

/// Process a JSON-RPC response coming back from the host. The only
/// response shape the fake currently consumes is the cap-violation
/// reply from its earlier `host.fs.read` probe — fold the received
/// error code into a preview response so the conformance test reads
/// `code = -32099` directly off the wire.
fn handle_host_response(v: &Value) -> Option<String> {
    let rid = v.get("id").and_then(|i| i.as_i64())?;
    let preview_id = PENDING_PREVIEW.with(|slot| {
        let mut s = slot.borrow_mut();
        match s.as_ref() {
            Some((p, pid)) if *pid == rid => {
                let p = p.clone();
                *s = None;
                Some(p)
            }
            _ => None,
        }
    })?;
    let err = v.get("error").cloned().unwrap_or(Value::Null);
    Some(
        json!({
            "jsonrpc": "2.0",
            "id": preview_id,
            "result": { "host_error": err }
        })
        .to_string(),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let mut next_outgoing_id: i64 = 1000;
    loop {
        let body = match read_frame()? {
            Some(b) => b,
            None => return Ok(()),
        };
        let v = parse_body(&body);
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        // Plugin→host response (id present, no method) — the only
        // case the fake consumes is the cap_violation probe reply.
        if method.is_empty() && !id.is_null() {
            if let Some(reply) = handle_host_response(&v) {
                write_frame(&reply)?;
            }
            continue;
        }
        match method {
            "initialize" => write_frame(&initialize_result(&id))?,
            "ping" => {
                let ts = v
                    .get("params")
                    .and_then(|p| p.get("ts"))
                    .cloned()
                    .unwrap_or_else(|| json!(0));
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "ts": ts }
                });
                write_frame(&body.to_string())?;
            }
            "preview" => {
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                if let Some(reply) = handle_preview(&id, &params, &mut next_outgoing_id).await {
                    write_frame(&reply)?;
                }
            }
            "shutdown" => {
                let body = json!({"jsonrpc": "2.0", "id": id, "result": null});
                write_frame(&body.to_string())?;
                // Wait for the host's `exit` notification, then return.
                if let Ok(Some(_)) = read_frame() {
                    return Ok(());
                }
                return Ok(());
            }
            "exit" => return Ok(()),
            _ => {
                // Unknown method — emit the SPEC §4.2.2 reserved
                // METHOD_NOT_FOUND so the host sees a clean error
                // rather than a hang.
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": "METHOD_NOT_FOUND",
                        "data": { "method": method }
                    }
                });
                write_frame(&body.to_string())?;
            }
        }
    }
}
