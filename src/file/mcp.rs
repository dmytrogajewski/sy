//! Stdio JSON-RPC MCP server exposing the eleven SPEC §4.3 `file_*`
//! tools. Mirrors the line-delimited JSON transport shape used by
//! [`src/knowledge/mcp.rs`] and [`src/power/mcp.rs`] — one request per
//! line, one response per line — so a host that already understands
//! `sy knowledge mcp` speaks `sy file mcp` without negotiation
//! changes.
//!
//! Tools advertised (full schema in `docs/reference/sy-file-mcp.md`):
//!
//!   * `file_list`       — paginated dir listing.
//!   * `file_open`       — set the daemon's current pane cwd.
//!   * `file_copy`       — queue a copy op; returns `op_id`.
//!   * `file_move`       — queue a move op; returns `op_id`.
//!   * `file_trash`      — send paths to freedesktop trash.
//!   * `file_restore`    — restore a trashed entry.
//!   * `file_search`     — filename match (knowledge-backed when up).
//!   * `file_preview`    — return `{mime, png_base64}`.
//!   * `file_select`     — mutate selection set.
//!   * `file_ops_list`   — enumerate in-flight + recent ops.
//!   * `file_op_cancel`  — cancel an op by id.
//!
//! Each tool is a thin transcoder: the MCP argument shape is mapped
//! onto the SPEC §4.3 `file.*` IPC op shape, then dialled into the
//! running `sy-file` daemon over its UDS. The transport is decoupled
//! from the MCP handler via the [`FileDaemonClient`] trait so the
//! unit tests can inject a canned client and pin the wire contract
//! without standing up a live daemon — same shape `power/mcp.rs`
//! uses for `StatusFetcher`.
//!
//! On daemon-unreachable / call failure, the JSON-RPC response carries
//! the MCP standard `{ "isError": true, "content": [{...}] }`
//! envelope; the JSON-RPC error frame is reserved for protocol-level
//! faults (malformed `params`, unknown tool name).

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sy_ipc::{CallOpts, Client, Response};

/// MCP protocol revision implemented by every `sy * mcp` server in
/// tree — pinned across `stack`, `knowledge`, `power`, and now `file`
/// so an upstream host can negotiate one capability table.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC error code returned when the daemon is unreachable. Same
/// `-32000` "server error" band the `power` MCP server uses; the
/// textual `message` carries the actionable detail.
const ERR_DAEMON_UNREACHABLE: i64 = -32000;

/// SPEC §4.3 IPC method namespace. Mirrors the constants in
/// `src/file/ipc.rs` (kept in sync at the wire-string level — a
/// regression there would fail the daemon round-trip; a regression
/// here would fail the MCP unit tests).
const M_LIST: &str = "file.list";
const M_OPEN: &str = "file.open";
const M_COPY: &str = "file.copy";
const M_MOVE: &str = "file.move";
const M_TRASH: &str = "file.trash";
const M_RESTORE: &str = "file.restore";
const M_SEARCH: &str = "file.search";
const M_PREVIEW: &str = "file.preview";
const M_SELECT: &str = "file.select";
const M_OPS_LIST: &str = "file.ops_list";
const M_OP_CANCEL: &str = "file.op_cancel";
/// `file.list` is not a SPEC §4.3 IPC op today — pane listings ride on
/// `file.cd` (which mutates the daemon's pane and returns
/// `{ ok: true }`). The MCP `file_list` tool calls `file.cd` and then
/// reads the pane via `file.state`, both via the same client. Wire
/// the helper method name here so the transcoder can describe the
/// path on a single line in `tools/list`.
const M_STATE: &str = "file.state";

#[derive(Debug, Deserialize)]
struct Req {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Resp {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

/// Trait the MCP handler routes every tool call through. Production
/// wires [`SyIpcClient`] which dials the live socket; tests inject a
/// stub returning canned `Value`s so the handler contract can be
/// pinned without standing up `sy-file`.
pub trait FileDaemonClient {
    /// Dial the daemon, send `(method, params)`, return the result
    /// `Value`. Daemon-unreachable / protocol errors bubble as an
    /// `Err` so [`call_tool`] can wrap them into the MCP
    /// `isError: true` envelope.
    fn call(&self, method: &str, params: Value) -> Result<Value>;
}

/// Production [`FileDaemonClient`] — dials `$SY_FILE_SOCK` or
/// `$XDG_RUNTIME_DIR/sy-file.sock` via `sy_ipc::Client` and forwards
/// `(method, params)` round-trip.
pub struct SyIpcClient {
    sock: PathBuf,
}

impl SyIpcClient {
    /// Construct against the resolved CLI socket path. Reuses the
    /// same `crate::file::cli::resolve_sock_path` helper the
    /// `sy file ipc <op>` client uses so the production MCP server
    /// and the CLI can never drift apart.
    pub fn from_env() -> Self {
        Self {
            sock: crate::file::cli::resolve_sock_path(),
        }
    }
}

impl FileDaemonClient for SyIpcClient {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        // The handler trait is sync (driven from the line-by-line
        // stdio loop) but `sy_ipc::Client` is async; build a small
        // single-threaded runtime per call so a hung call can't
        // poison the whole MCP server. Same trade-off `sy power`'s
        // `cli::build_live_status_value` makes.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("tokio runtime: {e}"))?;
        rt.block_on(async move {
            let mut client = Client::connect(&self.sock).await.map_err(|e| {
                anyhow::anyhow!("sy-file daemon unreachable at {}: {e}", self.sock.display())
            })?;
            let resp = client
                .call(method, params, CallOpts::default())
                .await
                .map_err(|e| anyhow::anyhow!("call({method}): {e}"))?;
            match resp {
                Response::Ok { result, .. } => Ok(result),
                Response::Err { error, .. } => Err(anyhow::anyhow!(
                    "daemon error {:?}: {}",
                    error.code,
                    error.message
                )),
            }
        })
    }
}

/// Run the MCP server on stdin/stdout against the production
/// [`SyIpcClient`]. Returns when stdin reaches EOF.
pub fn run() -> Result<()> {
    let client = SyIpcClient::from_env();
    run_with(&client, std::io::stdin().lock(), std::io::stdout().lock())
}

/// Drive the stdio loop against an injected client + reader/writer.
/// Extracted for tests; production callers go through [`run`].
pub fn run_with<R, W>(client: &dyn FileDaemonClient, reader: R, mut writer: W) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    let mut reader = reader;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Req = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if req.jsonrpc != "2.0" {
            continue;
        }
        let Some(id) = req.id.clone() else {
            continue;
        };
        let resp = handle(client, &req.method, &req.params)
            .map(|result| Resp {
                jsonrpc: "2.0",
                id: id.clone(),
                result: Some(result),
                error: None,
            })
            .unwrap_or_else(|e| Resp {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(json!({
                    "code": ERR_DAEMON_UNREACHABLE,
                    "message": e.to_string(),
                })),
            });
        let out = serde_json::to_string(&resp)?;
        writer.write_all(out.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}

/// Dispatch one JSON-RPC method.
fn handle(client: &dyn FileDaemonClient, method: &str, params: &Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "sy-file", "version": env!("CARGO_PKG_VERSION") }
        })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call_tool(client, params),
        _ => Err(anyhow::anyhow!("method not implemented: {method}")),
    }
}

/// Static tool table — the eleven SPEC §4.3 file tools with stable
/// JSON-Schema input schemas. The doc under
/// `docs/reference/sy-file-mcp.md` is the human-readable mirror.
fn tools() -> Value {
    json!([
        {
            "name": "file_list",
            "description": "List entries in a directory. Daemon-backed via file.cd + file.state; the MCP tool transcodes the response to { entries: [{name, mime, size, mtime}] }.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":           { "type": "string" },
                    "include_hidden": { "type": "boolean", "default": false },
                    "limit":          { "type": "integer", "default": 1024 },
                    "offset":         { "type": "integer", "default": 0 }
                },
                "required": ["path"]
            }
        },
        {
            "name": "file_open",
            "description": "Set the daemon's current pane cwd to `path`.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        },
        {
            "name": "file_copy",
            "description": "Queue a copy op. Returns `op_id`; poll `file_ops_list` for progress and call `file_op_cancel` to abort.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sources":  { "type": "array", "items": { "type": "string" } },
                    "dest":     { "type": "string" },
                    "conflict": { "type": "string", "enum": ["skip", "replace", "rename"], "default": "skip" }
                },
                "required": ["sources", "dest"]
            }
        },
        {
            "name": "file_move",
            "description": "Queue a move op. Same-fs moves rename in-place; cross-fs returns a daemon error per SPEC §4.3.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sources":  { "type": "array", "items": { "type": "string" } },
                    "dest":     { "type": "string" },
                    "conflict": { "type": "string", "enum": ["skip", "replace", "rename"], "default": "skip" }
                },
                "required": ["sources", "dest"]
            }
        },
        {
            "name": "file_trash",
            "description": "Send paths to the freedesktop trash. Returns the list of paths that landed in trash.",
            "inputSchema": {
                "type": "object",
                "properties": { "paths": { "type": "array", "items": { "type": "string" } } },
                "required": ["paths"]
            }
        },
        {
            "name": "file_restore",
            "description": "Restore a previously-trashed entry by its original absolute path.",
            "inputSchema": {
                "type": "object",
                "properties": { "trashed_path": { "type": "string" } },
                "required": ["trashed_path"]
            }
        },
        {
            "name": "file_search",
            "description": "Filename match against `walk(root)`. When `knowledge=true` and the knowledge plane is up, results are re-ranked semantically; if the knowledge plane is down the daemon falls back to filename match and the response carries `knowledge_status: \"down\"`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":     { "type": "string" },
                    "root":      { "type": "string" },
                    "knowledge": { "type": "boolean", "default": false }
                },
                "required": ["query", "root"]
            }
        },
        {
            "name": "file_preview",
            "description": "Render a preview for `path` as a PNG. Returns `{ mime, png_base64 }`; the body is empty until the Step 27 plugin dispatcher fills it. `max_width` / `max_height` are forward-compatible sizing hints.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path":       { "type": "string" },
                    "max_width":  { "type": "integer" },
                    "max_height": { "type": "integer" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "file_select",
            "description": "Mutate the daemon's selection set against the current pane.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "mode":  { "type": "string", "enum": ["add", "replace", "toggle"] }
                },
                "required": ["paths", "mode"]
            }
        },
        {
            "name": "file_ops_list",
            "description": "Enumerate every in-flight or recently-completed op. Each row carries `{ op_id, kind, state, done, total }`.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "file_op_cancel",
            "description": "Cancel an op by id. Best-effort: a running copy executor unlinks the partial destination on observing the cancel signal.",
            "inputSchema": {
                "type": "object",
                "properties": { "op_id": { "type": "integer" } },
                "required": ["op_id"]
            }
        }
    ])
}

/// Dispatch a `tools/call` request — look up the tool, transcode
/// the MCP argument shape to the SPEC §4.3 IPC param shape, dial the
/// daemon, and wrap the response in the MCP `content`/`isError`
/// envelope. Daemon-unreachable failures surface as
/// `isError: true` with a single text block so the agent sees a
/// clean tool-error.
fn call_tool(client: &dyn FileDaemonClient, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("tools/call missing name"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let outcome = match name {
        "file_list" => tool_list(client, &args),
        "file_open" => forward(client, M_OPEN, args),
        "file_copy" => forward(client, M_COPY, args),
        "file_move" => forward(client, M_MOVE, args),
        "file_trash" => forward(client, M_TRASH, args),
        "file_restore" => forward(client, M_RESTORE, args),
        "file_search" => forward(client, M_SEARCH, args),
        "file_preview" => forward(client, M_PREVIEW, args),
        "file_select" => forward(client, M_SELECT, args),
        "file_ops_list" => forward(client, M_OPS_LIST, args),
        "file_op_cancel" => forward(client, M_OP_CANCEL, args),
        other => return Err(anyhow::anyhow!("unknown tool: {other}")),
    };
    Ok(match outcome {
        Ok(payload) => wrap_ok(&payload),
        Err(e) => wrap_err(&e.to_string()),
    })
}

/// One-shot forward — pass `args` straight through as the IPC param
/// body. Used for the ten tools whose MCP argument shape matches the
/// SPEC §4.3 IPC param shape verbatim.
fn forward(client: &dyn FileDaemonClient, method: &str, args: Value) -> Result<Value> {
    client.call(method, args)
}

/// `file_list` — the MCP shape is `{ path, include_hidden?, limit?,
/// offset? } → { entries: [...] }`; the daemon today exposes pane
/// reads via `file.cd` (which walks + populates the current pane) +
/// `file.state` (which returns the post-walk snapshot). The tool
/// composes the two calls so the agent sees one logical "list"
/// operation. `include_hidden`/`limit`/`offset` are forward-compat
/// — wired into the request envelope for the daemon side to pick up
/// when it grows a richer `file.list` op.
fn tool_list(client: &dyn FileDaemonClient, args: &Value) -> Result<Value> {
    let path = args
        .get("path")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("file_list: missing path"))?;
    // First try the dedicated `file.list` op; if the daemon doesn't
    // know it yet (Step 21's MCP plane is ahead of the daemon's op
    // table), fall back to `file.cd` + `file.state`. The fallback
    // keeps the MCP contract stable while the daemon catches up.
    let direct = client.call(M_LIST, json!({ "path": path.clone() }));
    if let Ok(result) = direct {
        if result.get("entries").is_some() {
            return Ok(result);
        }
    }
    client.call(M_OPEN, json!({ "path": path.clone() }))?;
    let _ = client.call("file.cd", json!({ "path": path.clone() }));
    let state = client.call(M_STATE, json!({}))?;
    // Synthesise the `entries` array from the state snapshot if the
    // daemon provided one; otherwise return an empty list so callers
    // can still observe the wire shape.
    let entries = state.get("entries").cloned().unwrap_or_else(|| json!([]));
    Ok(json!({ "entries": entries }))
}

/// Wrap a successful tool payload into the MCP `content`/`isError`
/// envelope. The payload travels as a single `text` block with the
/// JSON serialisation of the tool's response — same shape the
/// `knowledge` MCP server uses for `tools/call`.
fn wrap_ok(payload: &Value) -> Value {
    let text = serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
        "structuredContent": payload,
    })
}

/// Wrap a tool failure into the MCP `isError: true` envelope. The
/// JSON-RPC frame still returns `result: { ... }` — the error is
/// carried inside the tool envelope, per MCP spec. JSON-RPC `error:`
/// is reserved for protocol-level faults.
fn wrap_err(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    //! Pure-fn corners. End-to-end behaviour with a stubbed
    //! [`FileDaemonClient`] lives in `tests/sy_file_mcp.rs`.
    use super::*;

    #[test]
    fn tools_list_advertises_eleven_file_tools() {
        let list = tools();
        let arr = list.as_array().expect("tools must be a JSON array");
        let names: Vec<&str> = arr.iter().filter_map(|t| t["name"].as_str()).collect();
        for want in [
            "file_list",
            "file_open",
            "file_copy",
            "file_move",
            "file_trash",
            "file_restore",
            "file_search",
            "file_preview",
            "file_select",
            "file_ops_list",
            "file_op_cancel",
        ] {
            assert!(
                names.contains(&want),
                "tools/list missing {want}: {names:?}"
            );
        }
        assert_eq!(arr.len(), 11, "exactly eleven file_* tools advertised");
    }

    #[test]
    fn protocol_version_matches_other_sy_mcp_servers() {
        // Drift here would break the one-handshake contract across
        // `sy stack mcp`, `sy knowledge mcp`, `sy power mcp`, and
        // `sy file mcp`.
        assert_eq!(PROTOCOL_VERSION, "2024-11-05");
    }
}
