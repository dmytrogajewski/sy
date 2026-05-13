//! Stdio JSON-RPC MCP server exposing stack tools to AI agents.
//!
//! Implements the subset of the Model Context Protocol an agent needs to
//! discover + invoke tools:
//!   - `initialize`       (server info + capabilities)
//!   - `tools/list`       (tool descriptors)
//!   - `tools/call`       (run a tool, return content)
//!   - `notifications/initialized` (sink, no-op)
//!
//! Tools:
//!   - stack_push  { kind, name?, content? | path? }
//!   - stack_list  {}
//!   - stack_get   { id }
//!   - stack_remove { id }
//!
//! Frame: line-delimited JSON on stdin/stdout (the simpler variant of MCP
//! commonly used by ACP-style clients). One request per line, one response
//! per line.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{ipc, state, Kind};

const PROTOCOL_VERSION: &str = "2024-11-05";

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

pub fn run() -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
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
        // Notifications (no id) get no reply.
        let Some(id) = req.id.clone() else {
            continue;
        };
        let resp = handle(&req.method, &req.params)
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
                error: Some(json!({"code": -32000, "message": e.to_string()})),
            });
        let out = serde_json::to_string(&resp)?;
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(out.as_bytes())?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

fn handle(method: &str, params: &Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "sy-stack", "version": env!("CARGO_PKG_VERSION") }
        })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call_tool(params),
        _ => Err(anyhow::anyhow!("method not implemented: {method}")),
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "stack_push",
            "description": "Push an artifact onto the sy stack. Provide either `content` (text) or `path` (existing file). Defaults to kind=app.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["app", "user"], "default": "app" },
                    "name": { "type": "string" },
                    "content": { "type": "string" },
                    "path":    { "type": "string" }
                }
            }
        },
        {
            "name": "stack_list",
            "description": "List all stack items (app + user pools).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "stack_get",
            "description": "Get an item's metadata + content/path by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        },
        {
            "name": "stack_remove",
            "description": "Remove an item by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        }
    ])
}

fn call_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("tools/call missing name"))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let content = match name {
        "stack_push" => tool_push(&args)?,
        "stack_list" => tool_list()?,
        "stack_get" => tool_get(&args)?,
        "stack_remove" => tool_remove(&args)?,
        other => return Err(anyhow::anyhow!("unknown tool: {other}")),
    };
    Ok(json!({
        "content": [{ "type": "text", "text": content }],
        "isError": false
    }))
}

fn tool_push(args: &Value) -> Result<String> {
    let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("app");
    let kind = Kind::parse(kind)?;
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let item = if let Some(content) = args.get("content").and_then(|v| v.as_str()) {
        let display_name = name.unwrap_or_else(|| {
            content
                .lines()
                .next()
                .unwrap_or("(content)")
                .chars()
                .take(40)
                .collect()
        });
        state::push_content(kind, display_name, content.as_bytes(), "text")?
    } else if let Some(path_str) = args.get("path").and_then(|v| v.as_str()) {
        let p = std::path::Path::new(path_str);
        if !p.exists() {
            return Err(anyhow::anyhow!("path not found: {path_str}"));
        }
        let display_name = name.unwrap_or_else(|| {
            p.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("(file)")
                .to_string()
        });
        state::push_file(kind, display_name, p)?
    } else {
        return Err(anyhow::anyhow!("stack_push requires `content` or `path`"));
    };

    let id = item.id.clone();
    let mut items = state::load()?;
    items.items.push(item);
    state::check_caps(&mut items, 8, 8);
    state::save(&items)?;
    let _ = ipc::send(&ipc::Op::Refresh);
    Ok(format!(r#"{{"id":"{id}"}}"#))
}

fn tool_list() -> Result<String> {
    let items = state::load()?;
    Ok(serde_json::to_string(&items)?)
}

fn tool_get(args: &Value) -> Result<String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing id"))?;
    let items = state::load()?;
    let it = state::find(&items, id)?;
    let body = if let Some(p) = &it.path {
        fs::read_to_string(p).unwrap_or_else(|_| format!("(binary file at {})", p.display()))
    } else {
        let bytes = state::read_payload(&it.id).unwrap_or_default();
        String::from_utf8_lossy(&bytes).to_string()
    };
    Ok(json!({
        "id": it.id,
        "kind": it.kind.as_str(),
        "name": it.name,
        "content_kind": it.content_kind,
        "size": it.size,
        "path": it.path.as_ref().map(|p| p.display().to_string()),
        "content": body,
    })
    .to_string())
}

fn tool_remove(args: &Value) -> Result<String> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing id"))?;
    let mut items = state::load()?;
    let idx = items
        .items
        .iter()
        .position(|i| i.id == id)
        .ok_or_else(|| state::not_found(id))?;
    let it = items.items.remove(idx);
    state::delete_blobs(&it.id);
    state::save(&items)?;
    let _ = ipc::send(&ipc::Op::Refresh);
    Ok(format!(r#"{{"removed":"{id}"}}"#))
}
