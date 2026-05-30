//! Stdio JSON-RPC MCP server exposing knowledge tools.
//!
//! Tools:
//!   - knowledge_search { query, limit?=8, source? }
//!   - knowledge_list_sources {}
//!   - knowledge_index { source? }
//!
//! Frame: line-delimited JSON, mirrors `src/stack/mcp.rs`.

use std::io::{BufRead, BufReader, Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{cli, ipc, qdrant, runctx::RunCtx, sources, state, status};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Hardening bounds for tool responses. Whatever this MCP returns is appended
/// to the calling agent's context and lives on its heap for the whole session,
/// so every response must be size-bounded regardless of what the caller asks
/// for. Without these, a single large-`limit` search (or a loop of searches)
/// could dump unbounded text into the agent and balloon its memory.
const MAX_LIMIT: u64 = 20;
const MAX_CANDIDATES: u64 = 64;
const MAX_CHUNK_CHARS: usize = 2000;

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
            "description": "Semantic search over the user's indexed files. Two-stage by default: embed → qdrant top-`candidates` → bge-reranker-v2-m3 cross-encoder → top-`limit`. Set `rerank=false` for the low-latency embed-only path. Responses are size-bounded: `limit` is capped at 20, `candidates` at 64, and each `chunk_text` is clipped to ~2000 chars (with `truncated:true` and the original `chunk_chars` set) — re-read `file_path` for the full chunk when needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":      { "type": "string" },
                    "limit":      { "type": "integer", "default": 8 },
                    "source":     { "type": "string", "description": "Optional registered source path prefix to restrict to" },
                    "rerank":     { "type": "boolean", "default": true, "description": "Apply cross-encoder rerank on top of qdrant cosine retrieval. Adds ~350 ms per candidate on AMD NPU (default 8 candidates ≈ 2.8 s)." },
                    "candidates": { "type": "integer", "default": 8, "description": "Top-N from qdrant before reranking. Ignored when rerank=false. Each candidate is one NPU dispatch at ~350 ms; bump only when recall outranks latency." }
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
            "description": "Trigger an incremental index pass. When the knowledge daemon is running this enqueues the pass on it and returns immediately (`queued:true`); otherwise it runs inline and returns the report when complete (`queued:false`). Optional `source` restricts to one path and always runs inline.",
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
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(8)
        .min(MAX_LIMIT) as usize;
    let prefix = args
        .get("source")
        .and_then(|v| v.as_str())
        .and_then(|s| sources::expand(s).map(|p| p.display().to_string()).ok());
    let rerank = args.get("rerank").and_then(|v| v.as_bool()).unwrap_or(true);
    let candidates = args
        .get("candidates")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .min(MAX_CANDIDATES) as usize;
    // Delegate to the shared helper. The daemon owns the NPU, so
    // when it's up we round-trip a single Search/SearchRerank request
    // and avoid loading a second ORT session in this process. If the
    // daemon is down, the helper falls back to in-process embedding so
    // the MCP server still works offline.
    let hits = cli::search_hits_opts(
        query,
        limit,
        prefix.as_deref(),
        rerank,
        candidates,
        sy_core::Priority::Interactive,
    )?;
    let arr: Vec<_> = hits
        .iter()
        .map(|h| {
            let (text, truncated) = truncate_chars(&h.chunk_text, MAX_CHUNK_CHARS);
            let mut row = json!({
                "score": h.score,
                "file_path": h.file_path,
                "chunk_index": h.chunk_index,
                "chunk_text": text,
            });
            let obj = row.as_object_mut().unwrap();
            if truncated {
                // Signal that the chunk was clipped so the agent knows to
                // re-read `file_path` if it needs the rest, rather than
                // assuming it has the full chunk.
                obj.insert("truncated".into(), json!(true));
                obj.insert("chunk_chars".into(), json!(h.chunk_text.chars().count()));
            }
            if let Some(es) = h.embed_score {
                obj.insert("embed_score".into(), json!(es));
            }
            row
        })
        .collect();
    Ok(serde_json::to_string(&arr)?)
}

/// Bound a chunk to `max` chars on a char boundary, returning the (possibly
/// clipped) text and whether it was cut. The agent still gets `file_path` +
/// `chunk_index` to re-read the full chunk on demand, so a single search can
/// never dump an unbounded payload into its context.
fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => {
            let mut out = s[..byte_idx].to_string();
            out.push('…');
            (out, true)
        }
        None => (s.to_string(), false),
    }
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
        .and_then(|s| sources::expand(s).ok());

    // When the daemon is up, hand the pass off to it instead of embedding
    // inline. Running our own embedder here forks a second ORT session that
    // contends for the single-context NPU (see `cli::sync`) and blocks this
    // single-threaded MCP server for the entire pass — so a large index keeps
    // one `tools/call` in flight for minutes while the server can serve
    // nothing else. The daemon owns the NPU and indexes incrementally in the
    // background; enqueue via IndexNow and return immediately. A `source`-
    // scoped request has no daemon equivalent, so it still runs inline.
    if src.is_none()
        && status::load()
            .ok()
            .filter(|s| status::is_fresh(s) && s.daemon_running)
            .is_some()
    {
        ipc::send(&ipc::Op::IndexNow).context("send IndexNow to daemon")?;
        return Ok(json!({
            "queued": true,
            "daemon": true,
            "note": "incremental index running on the knowledge daemon; poll `knowledge_list_sources` or `sy knowledge status` for progress",
        })
        .to_string());
    }

    qdrant::ensure_collection()?;
    let mut idx = state::load().unwrap_or_default();
    let ctx = RunCtx::interactive();
    let report = cli::run_index(&mut idx, src.as_deref(), false, &ctx)?;
    idx.last_sync_unix = state::now_secs();
    state::save(&idx)?;
    Ok(json!({
        "queued": false,
        "scanned": report.scanned,
        "indexed": report.indexed,
        "skipped": report.skipped,
        "deleted": report.deleted,
        "elapsed_ms": report.elapsed_ms,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_leaves_short_text_untouched() {
        let (out, cut) = truncate_chars("hello", 2000);
        assert_eq!(out, "hello");
        assert!(!cut);
    }

    #[test]
    fn truncate_chars_clips_at_the_char_limit() {
        let s = "a".repeat(2500);
        let (out, cut) = truncate_chars(&s, MAX_CHUNK_CHARS);
        assert!(cut);
        // 2000 kept chars + the ellipsis marker.
        assert_eq!(out.chars().count(), MAX_CHUNK_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_chars_splits_on_a_char_boundary_for_multibyte() {
        // Each 'é' is 2 bytes; a naive byte slice at the limit would panic.
        let s = "é".repeat(10);
        let (out, cut) = truncate_chars(&s, 4);
        assert!(cut);
        assert_eq!(out.chars().filter(|&c| c == 'é').count(), 4);
    }

    #[test]
    fn truncate_chars_exact_length_is_not_cut() {
        let s = "x".repeat(MAX_CHUNK_CHARS);
        let (out, cut) = truncate_chars(&s, MAX_CHUNK_CHARS);
        assert!(!cut);
        assert_eq!(out.chars().count(), MAX_CHUNK_CHARS);
    }
}
