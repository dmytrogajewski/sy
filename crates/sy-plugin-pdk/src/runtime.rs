//! Framed-stdio runtime for the PDK.
//!
//! This is the loop the [`crate::define_plugin!`] macro expands into.
//! It owns:
//!
//! 1. The LSP `Content-Length:` framing on stdin and stdout — same
//!    wire shape as the host's
//!    [`src/plugin/transport.rs`][host-transport]. The
//!    `MAX_PAYLOAD_BYTES` constant mirrors the host file's verbatim;
//!    a drift here breaks every plugin.
//! 2. The SPEC §4.2.3 lifecycle handshake (`initialize`, `ping`,
//!    `shutdown`, `exit`) so plugin authors never have to spell those
//!    methods themselves.
//! 3. Routing of plugin-initiated host requests (`host.fs.read`, …)
//!    by id so [`HostHandle::call`] can `await` a typed reply.
//!
//! No `unwrap` / `expect` on the request path: malformed input is
//! converted to a JSON-RPC error object on stdout and the loop
//! continues; only EOF or stdio I/O failure terminates the runtime.
//!
//! [host-transport]: ../../../src/plugin/transport.rs

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex};

use crate::types::{codes, Capability, HandlerError, RpcError};

/// Maximum frame payload in bytes (16 MiB). Mirrors
/// `src/plugin/transport.rs::MAX_PAYLOAD_BYTES`.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// `Content-Length:` literal (LSP framing).
const CONTENT_LENGTH_HEADER: &str = "Content-Length:";

/// Static plugin metadata baked at compile time. The macro emits one
/// of these and hands it to [`run`].
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// `[plugin] id` — kebab-case, unique. Mirrors the manifest.
    pub id: &'static str,
    /// Plugin version string (usually `env!("CARGO_PKG_VERSION")`).
    pub version: &'static str,
    /// `api = "1"` — plugin contract version the runtime advertises.
    pub api: &'static str,
    /// Capabilities the plugin offers. Mirrors `[[capability]]` rows.
    pub capabilities: Vec<Capability>,
}

/// Type-erased async handler signature. Each capability method
/// (`preview`, `open`, …) is one of these, mapped by method name.
pub type HandlerFn = Arc<
    dyn Fn(
            Value,
            Arc<HostHandle>,
        ) -> Pin<Box<dyn Future<Output = Result<Value, anyhow::Error>> + Send>>
        + Send
        + Sync,
>;

/// Routing table the macro emits. Maps a capability method name
/// (`"preview"`, …) to its async handler.
pub type HandlerTable = HashMap<&'static str, HandlerFn>;

/// Internal trait object the [`HostHandle`] writes outbound frames
/// through. Bridges `tokio::io::stdout()` and the in-process
/// `DuplexStream` half tests drive over the same shape.
trait AsyncWriteDyn: tokio::io::AsyncWrite + Unpin + Send {}
impl<T> AsyncWriteDyn for T where T: tokio::io::AsyncWrite + Unpin + Send {}

/// Handle plugin authors use to call host methods (`host.fs.read`, …).
///
/// Shared via `Arc<HostHandle>` so the macro can hand a handler a
/// cheap reference while the runtime owns the long-lived outbound
/// writer + pending-response table.
pub struct HostHandle {
    /// Outbound writer guarded by a mutex so concurrent host-fn calls
    /// from the same handler don't interleave frames.
    out: Mutex<Box<dyn AsyncWriteDyn>>,
    /// Map of pending plugin→host requests by id.
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>,
    /// Monotonic counter for plugin-initiated request ids. Starts at
    /// 1000 so it never collides with the host's request ids (which
    /// start at 1 and stay small).
    next_id: Mutex<i64>,
}

impl HostHandle {
    /// Build a `HostHandle` over the given outbound writer.
    fn new<W: tokio::io::AsyncWrite + Unpin + Send + 'static>(out: W) -> Self {
        Self {
            out: Mutex::new(Box::new(out)),
            pending: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1000),
        }
    }

    /// Post a plugin-initiated request to the host and await the
    /// matching response. The runtime's main loop routes the reply
    /// back via the `pending` table.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = {
            let mut g = self.next_id.lock().await;
            let v = *g;
            *g += 1;
            v
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(e) = write_frame(&mut *self.out.lock().await, &body).await {
            // Drop the pending slot on send failure so a future poll
            // doesn't leak.
            self.pending.lock().await.remove(&id);
            return Err(RpcError::Transport(e.to_string()));
        }
        rx.await
            .map_err(|e| RpcError::Transport(format!("oneshot dropped: {e}")))?
    }

    /// Resolve a pending plugin→host request by id with the host's
    /// response payload (or error). Called by the runtime's main loop
    /// when a frame with `id` but no `method` arrives.
    async fn resolve(&self, id: i64, payload: Result<Value, RpcError>) {
        if let Some(tx) = self.pending.lock().await.remove(&id) {
            // Receiver may have been dropped if the caller timed out;
            // ignore the send result.
            let _ = tx.send(payload);
        }
    }
}

/// Run the PDK runtime against `tokio::io::{stdin, stdout}`. Returns
/// when stdin is closed or the host sends `exit`.
///
/// The macro expands into a `#[tokio::main(flavor = "current_thread")]`
/// `main` that calls this. Authors can also call it directly if they
/// want to build the [`HandlerTable`] dynamically.
pub async fn run(info: PluginInfo, handlers: HandlerTable) -> std::io::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    run_with_io(info, handlers, stdin, stdout).await
}

/// Entry point factored out for in-process integration tests that
/// drive the runtime against a `tokio::io::DuplexStream` instead of
/// the real stdio pair. Public so the PDK's own `tests/` can pin the
/// wire shape end-to-end without spawning a child binary.
///
/// The read loop dispatches each frame in a `tokio::spawn` so a
/// handler that awaits a plugin→host call (`host.fs.read`) doesn't
/// block the read loop from picking up the host's reply on stdin —
/// without this, the runtime deadlocks the moment a handler calls
/// any host fn. Mirrors `src/plugin/proc.rs::route_incoming_frame`.
pub async fn run_with_io<R, W>(
    info: PluginInfo,
    handlers: HandlerTable,
    mut stdin: R,
    stdout: W,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let host = Arc::new(HostHandle::new(stdout));
    let info = Arc::new(info);
    let handlers = Arc::new(handlers);
    loop {
        match read_frame(&mut stdin).await? {
            None => return Ok(()),
            Some(body) => {
                let Ok(v) = serde_json::from_slice::<Value>(&body) else {
                    // Malformed frame — emit a JSON-RPC parse error
                    // notification (no id known) and keep going.
                    let _ =
                        write_frame(&mut *host.out.lock().await, &parse_error_notification()).await;
                    continue;
                };
                let info_c = info.clone();
                let handlers_c = handlers.clone();
                let host_c = host.clone();
                // Classify the frame inline: exit notifications must
                // terminate the read loop; everything else is
                // dispatched concurrently so a long-running handler
                // can `.await` host fns without blocking us from
                // reading the host's reply.
                if is_exit_notification(&v) {
                    return Ok(());
                }
                tokio::spawn(async move {
                    dispatch_frame(&info_c, &handlers_c, host_c, v).await;
                });
            }
        }
    }
}

/// `true` when the inbound frame is the SPEC §4.2.3 `exit`
/// notification (`{"method":"exit"}` with no `id` or `id = null`).
fn is_exit_notification(v: &Value) -> bool {
    let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let has_id = v.get("id").map(|x| !x.is_null()).unwrap_or(false);
    method == "exit" && !has_id
}

/// Single-frame dispatch. Classifies the inbound JSON value as
/// host→plugin request, plugin→host response, or notification, then
/// routes accordingly. Returns `false` when the loop should
/// terminate (the host sent the `exit` notification).
async fn dispatch_frame(
    info: &PluginInfo,
    handlers: &HandlerTable,
    host: Arc<HostHandle>,
    v: Value,
) -> bool {
    let method = v.get("method").and_then(|m| m.as_str()).map(String::from);
    let id = v.get("id").cloned();
    let has_id = id.as_ref().map(|x| !x.is_null()).unwrap_or(false);
    match (method, id) {
        // Plugin→host response landed (host replying to our request).
        (None, Some(id_v)) if has_id => {
            if let Some(id) = id_v.as_i64() {
                let payload = parse_host_response(&v);
                host.resolve(id, payload).await;
            }
            true
        }
        // Notification (method present, no id). The only one the PDK
        // reacts to is `exit`; everything else is dropped per SPEC
        // §4.1 forward-compat.
        (Some(m), _id) if !has_id => {
            // `exit` — the host wants the plugin to terminate within
            // `shutdown_timeout_ms`. Returning `false` ends the loop;
            // the caller's `tokio::main` then drops back to `main()`
            // and the process exits 0 cleanly.
            m != "exit"
        }
        // Host→plugin request (method + id).
        (Some(m), Some(id_v)) => {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            let reply = handle_request(info, handlers, &host, &m, &id_v, params).await;
            let _ = write_frame(&mut *host.out.lock().await, &reply).await;
            true
        }
        // Frame with neither a method nor a meaningful id is malformed;
        // drop it and continue.
        _ => true,
    }
}

/// Handle one host→plugin request. Routes lifecycle methods
/// (`initialize` / `ping` / `shutdown`) internally; everything else
/// is looked up in the `handlers` table the macro provided.
async fn handle_request(
    info: &PluginInfo,
    handlers: &HandlerTable,
    host: &Arc<HostHandle>,
    method: &str,
    id: &Value,
    params: Value,
) -> Value {
    match method {
        "initialize" => initialize_reply(info, id),
        "ping" => ping_reply(id, &params),
        "shutdown" => json!({"jsonrpc": "2.0", "id": id, "result": null}),
        m => match handlers.get(m) {
            Some(handler) => match handler(params, host.clone()).await {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(e) => handler_error_reply(id, e),
            },
            None => method_not_found_reply(id, method),
        },
    }
}

/// Build the SPEC §4.2.3 `initialize` reply from compile-time
/// [`PluginInfo`].
fn initialize_reply(info: &PluginInfo, id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "name": info.id,
            "version": info.version,
            "api": info.api,
            "capabilities": info.capabilities,
            "host_methods": [],
        }
    })
}

/// Build the SPEC §4.2.3 `ping` reply — echoes the `ts` the host sent.
fn ping_reply(id: &Value, params: &Value) -> Value {
    let ts = params.get("ts").cloned().unwrap_or_else(|| json!(0));
    json!({"jsonrpc": "2.0", "id": id, "result": {"ts": ts}})
}

/// Build a JSON-RPC `-32601 METHOD_NOT_FOUND` reply.
fn method_not_found_reply(id: &Value, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": codes::METHOD_NOT_FOUND,
            "message": "METHOD_NOT_FOUND",
            "data": {"method": method}
        }
    })
}

/// Convert an `anyhow::Error` returned by a plugin handler into a
/// JSON-RPC error response. If the error downcasts to
/// [`HandlerError`], propagate its `code` + `data`; otherwise emit a
/// generic `-32603 INTERNAL_ERROR`.
fn handler_error_reply(id: &Value, e: anyhow::Error) -> Value {
    if let Some(he) = e.downcast_ref::<HandlerError>() {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": he.code,
                "message": he.message,
                "data": he.data,
            }
        });
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": codes::INTERNAL_ERROR,
            "message": "INTERNAL_ERROR",
            "data": {"detail": e.to_string()}
        }
    })
}

/// Convert a host response frame into the typed payload the
/// `HostHandle::call` future resolves to. Errors are mapped to
/// [`RpcError::Host`].
fn parse_host_response(v: &Value) -> Result<Value, RpcError> {
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0) as i32;
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let data = err.get("data").cloned().unwrap_or(Value::Null);
        return Err(RpcError::Host {
            code,
            message,
            data,
        });
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// Build a JSON-RPC parse-error notification (no id) for frames whose
/// JSON body failed to decode.
fn parse_error_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {
            "code": -32700,
            "message": "PARSE_ERROR",
            "data": null
        }
    })
}

/// Read one Content-Length-framed frame from an `AsyncRead`. Returns
/// `Ok(None)` on EOF before any header byte arrived.
pub async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut headers = Vec::with_capacity(64);
    let mut last4: [u8; 4] = [0; 4];
    loop {
        let mut b = [0u8; 1];
        let n = reader.read(&mut b).await?;
        if n == 0 {
            if headers.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::other("EOF mid-header"));
        }
        headers.push(b[0]);
        last4 = [last4[1], last4[2], last4[3], b[0]];
        if last4 == *b"\r\n\r\n" {
            break;
        }
        if headers.len() > 16 * 1024 {
            return Err(std::io::Error::other("header block exceeded 16 KiB"));
        }
    }
    let header_text = std::str::from_utf8(&headers)
        .map_err(|e| std::io::Error::other(format!("header utf8: {e}")))?;
    let mut length: Option<usize> = None;
    for line in header_text.split("\r\n") {
        if let Some(rest) = line.strip_prefix(CONTENT_LENGTH_HEADER) {
            length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = length.ok_or_else(|| std::io::Error::other("missing Content-Length"))?;
    if len > MAX_PAYLOAD_BYTES {
        return Err(std::io::Error::other(format!(
            "frame::too_large advertised {len} bytes exceeds 16 MiB cap"
        )));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// Write a JSON value framed with `Content-Length:`. Errors propagate
/// as `io::Error` so the caller can decide how to respond.
pub async fn write_frame<W, T>(out: &mut W, body: &T) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
    T: Serialize,
{
    let bytes =
        serde_json::to_vec(body).map_err(|e| std::io::Error::other(format!("serialize: {e}")))?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(std::io::Error::other(format!(
            "frame::too_large encoded {} bytes exceeds 16 MiB cap",
            bytes.len()
        )));
    }
    let header = format!("{} {}\r\n\r\n", CONTENT_LENGTH_HEADER, bytes.len());
    out.write_all(header.as_bytes()).await?;
    out.write_all(&bytes).await?;
    out.flush().await?;
    Ok(())
}

/// RFC 4648 base64 decoder used by [`crate::types::host::fs::read`] to
/// turn the host's `bytes_base64` field back into `Vec<u8>` without
/// pulling in a `base64` crate dep. Mirrors the inline decoder in
/// `src/plugin/host_fns.rs` so the two sides stay byte-compatible.
pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    const TBL: [i8; 256] = build_table();
    let s = s.trim();
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    let mut padding = 0usize;
    for &b in bytes {
        if b == b'=' {
            padding += 1;
            continue;
        }
        let v = TBL[b as usize];
        if v < 0 {
            return Err(format!("invalid base64 char: 0x{b:02x}"));
        }
        buf = (buf << 6) | (v as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    if padding > 2 {
        return Err("too many '=' padding chars".into());
    }
    Ok(out)
}

const fn build_table() -> [i8; 256] {
    let mut t = [-1i8; 256];
    let mut i = 0u8;
    while i < 26 {
        t[(b'A' + i) as usize] = i as i8;
        t[(b'a' + i) as usize] = (i + 26) as i8;
        i += 1;
    }
    let mut j = 0u8;
    while j < 10 {
        t[(b'0' + j) as usize] = (j + 52) as i8;
        j += 1;
    }
    t[b'+' as usize] = 62;
    t[b'/' as usize] = 63;
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::AsyncWriteExt;

    /// `read_frame` round-trips a small JSON body written by
    /// `write_frame` — the wire-shape baseline every higher test rests
    /// on.
    #[tokio::test(flavor = "current_thread")]
    async fn frame_roundtrip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &json!({"hello": "world"}))
            .await
            .unwrap();
        let mut cur = Cursor::new(buf);
        let body = read_frame(&mut cur).await.unwrap().expect("frame");
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["hello"], "world");
    }

    /// Frame whose advertised `Content-Length` exceeds the 16 MiB cap
    /// is rejected before we attempt to allocate the body buffer.
    #[tokio::test(flavor = "current_thread")]
    async fn read_frame_rejects_oversize() {
        let mut buf: Vec<u8> = Vec::new();
        buf.write_all(b"Content-Length: 99999999999\r\n\r\n")
            .await
            .unwrap();
        let mut cur = Cursor::new(buf);
        let err = read_frame(&mut cur).await.expect_err("oversize must error");
        assert!(err.to_string().contains("too_large"), "{err}");
    }

    /// EOF before any header byte returns `Ok(None)`. Mirrors the
    /// shape `sy-plugin-fake` uses to exit cleanly when stdin closes.
    #[tokio::test(flavor = "current_thread")]
    async fn read_frame_eof_at_start_returns_none() {
        let mut cur = Cursor::new(Vec::<u8>::new());
        let res = read_frame(&mut cur).await.unwrap();
        assert!(res.is_none());
    }

    /// `base64_decode` round-trips a canonical PNG-shaped payload.
    #[test]
    fn base64_decode_roundtrip() {
        // "hello" → "aGVsbG8="
        let out = base64_decode("aGVsbG8=").unwrap();
        assert_eq!(out, b"hello");
    }

    /// Invalid base64 characters are rejected with a descriptive
    /// error rather than panicking.
    #[test]
    fn base64_decode_rejects_invalid_char() {
        let err = base64_decode("hello!").expect_err("must reject '!'");
        assert!(err.contains("invalid base64"), "{err}");
    }
}
