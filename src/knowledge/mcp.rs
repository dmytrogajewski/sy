//! Stdio JSON-RPC MCP server exposing knowledge tools.
//!
//! Tools:
//!   - knowledge_search { query, limit?=8, source? }
//!   - knowledge_list_sources {}
//!   - knowledge_index { source? }
//!
//! Frame: line-delimited JSON, mirrors `src/stack/mcp.rs`.

use std::io::{BufRead, BufReader, Write};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{cli, qdrant, runctx::RunCtx, sources, state};

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
            "serverInfo": { "name": "sy-knowledge", "version": env!("CARGO_PKG_VERSION") }
        })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call_tool(params),
        _ => Err(anyhow::anyhow!("method not implemented: {method}")),
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "knowledge_search",
            "description": "Semantic search over the user's indexed files. Two-stage by default: embed → qdrant top-`candidates` → bge-reranker-v2-m3 cross-encoder → top-`limit`. Set `rerank=false` for the low-latency embed-only path.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":      { "type": "string" },
                    "limit":      { "type": "integer", "default": 8 },
                    "source":     { "type": "string", "description": "Optional registered source path prefix to restrict to" },
                    "rerank":     { "type": "boolean", "default": true, "description": "Apply cross-encoder rerank on top of qdrant cosine retrieval. Adds ~0.5–1 s." },
                    "candidates": { "type": "integer", "default": 30, "description": "Top-N from qdrant before reranking. Ignored when rerank=false." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "knowledge_list_sources",
            "description": "List the registered index sources, their enabled state, and last-indexed times.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "knowledge_index",
            "description": "Trigger an incremental index pass (returns when complete). Optional `source` restricts to one path.",
            "inputSchema": {
                "type": "object",
                "properties": { "source": { "type": "string" } }
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
    let payload = match name {
        "knowledge_search" => tool_search(&args)?,
        "knowledge_list_sources" => tool_list()?,
        "knowledge_index" => tool_index(&args)?,
        other => return Err(anyhow::anyhow!("unknown tool: {other}")),
    };
    Ok(json!({
        "content": [{ "type": "text", "text": payload }],
        "isError": false
    }))
}

fn tool_search(args: &Value) -> Result<String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing query"))?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8) as usize;
    let prefix = args
        .get("source")
        .and_then(|v| v.as_str())
        .map(|s| sources::expand(s).map(|p| p.display().to_string()).ok())
        .flatten();
    let rerank = args.get("rerank").and_then(|v| v.as_bool()).unwrap_or(true);
    let candidates = args
        .get("candidates")
        .and_then(|v| v.as_u64())
        .unwrap_or(30) as usize;
    // Delegate to the shared helper. The daemon owns the NPU, so
    // when it's up we round-trip a single Search/SearchRerank request
    // and avoid loading a second ORT session in this process. If the
    // daemon is down, the helper falls back to in-process embedding so
    // the MCP server still works offline.
    let hits = cli::search_hits_opts(query, limit, prefix.as_deref(), rerank, candidates)?;
    let arr: Vec<_> = hits
        .iter()
        .map(|h| {
            let mut row = json!({
                "score": h.score,
                "file_path": h.file_path,
                "chunk_index": h.chunk_index,
                "chunk_text": h.chunk_text,
            });
            if let Some(es) = h.embed_score {
                row.as_object_mut()
                    .unwrap()
                    .insert("embed_score".into(), json!(es));
            }
            row
        })
        .collect();
    Ok(serde_json::to_string(&arr)?)
}

fn tool_list() -> Result<String> {
    let section = sources::load()?;
    let idx = state::load().unwrap_or_default();
    let qdrant_count = qdrant::point_count().unwrap_or(0);
    let entries: Vec<_> = section
        .sources
        .iter()
        .map(|s| {
            let resolved = sources::expand(&s.path)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| s.path.clone());
            let last_indexed = idx
                .files
                .iter()
                .filter(|(p, _)| p.starts_with(&resolved))
                .map(|(_, e)| e.mtime)
                .max()
                .unwrap_or(0);
            json!({
                "path": s.path,
                "resolved": resolved,
                "enabled": s.enabled,
                "last_indexed_unix": last_indexed,
            })
        })
        .collect();
    Ok(serde_json::to_string(&json!({
        "qdrant_points": qdrant_count,
        "sources": entries,
    }))?)
}

fn tool_index(args: &Value) -> Result<String> {
    let src = args
        .get("source")
        .and_then(|v| v.as_str())
        .map(|s| sources::expand(s).ok())
        .flatten();
    qdrant::ensure_collection()?;
    let mut idx = state::load().unwrap_or_default();
    let ctx = RunCtx::interactive();
    let report = cli::run_index(&mut idx, src.as_deref(), false, &ctx)?;
    idx.last_sync_unix = state::now_secs();
    state::save(&idx)?;
    Ok(json!({
        "scanned": report.scanned,
        "indexed": report.indexed,
        "skipped": report.skipped,
        "deleted": report.deleted,
        "elapsed_ms": report.elapsed_ms,
    })
    .to_string())
}
