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

/// REQ-6 abstain message surfaced in the `tool_search` envelope when the
/// calibrated confidence is below the caller's `abstain_threshold`.
const ABSTAIN_REASON: &str = "no high-confidence match";

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
            "description": "Semantic search over the user's OWN indexed corpus — their Telegram messages, notes, emails, code, and personal files. This is NOT the public web and NOT a question-answering assistant; it returns passages of the user's stored text ranked by meaning.\n\nHOW TO WRITE `query`: it is matched against the *content of stored text*, so phrase it as the words/topics/entities you expect to appear IN that content — never the user's question verbatim. Translate the user's intent into search terms. Example: if the user asks \"what should I buy for my wife?\", do NOT search \"what to buy for wife\" — search the concrete signals likely in their messages, e.g. \"wife birthday gift wishlist she wants\", and scope with `from`/`kind`/`date_from` when you know the sender, channel, or timeframe. Prefer several focused searches over one broad question.\n\nMECHANICS: two-stage by default: embed → qdrant top-`candidates` → bge-reranker-v2-m3 cross-encoder → top-`limit`. Set `rerank=false` for the low-latency embed-only path. Responses are size-bounded: `limit` is capped at 20, `candidates` at 64, and each `chunk_text` is clipped to ~2000 chars (with `truncated:true` and the original `chunk_chars` set) — fetch the full chunk via `knowledge_get_chunk`. Pre-search payload filters narrow scope before scoring: `date_from`/`date_to` (RFC-3339 window), `from` (senders), `kind` (source kinds), `include_sources`/`exclude_sources` (registered source names). The agent's OWN output is EXCLUDED from default scope to avoid self-poisoning (the model quoting its own prior answers as if they were the user's data): both `claude-transcripts` and `agent-history` (session logs/caches under ~/.claude, ~/.codex, ~/.cursor, ~/.gemini, ~/.agents, ~/.antigravity, ~/.hermes) are dropped by default — pass e.g. `kind:[\"agent-history\"]` or an `include_sources` naming such a source to opt it back in.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":      { "type": "string", "description": "Semantic search phrase: the topics, entities, names, or keywords you expect to literally appear in the user's stored text — NOT the user's question verbatim and NOT a question addressed to an assistant. GOOD: 'wife birthday gift wishlist preferences'; 'apartment lease renewal deadline'; 'flight booking confirmation Lisbon'. BAD: 'what should I buy for my wife?'; 'when is my lease up?'. Prefer 2–8 content words and run multiple focused queries rather than one broad question." },
                    "limit":      { "type": "integer", "default": 8 },
                    "source":     { "type": "string", "description": "Optional registered source path prefix to restrict to" },
                    "rerank":     { "type": "boolean", "default": true, "description": "Apply cross-encoder rerank on top of qdrant cosine retrieval. Adds ~350 ms per candidate on AMD NPU (default 8 candidates ≈ 2.8 s)." },
                    "candidates": { "type": "integer", "default": 8, "description": "Top-N from qdrant before reranking. Ignored when rerank=false. Each candidate is one NPU dispatch at ~350 ms; bump only when recall outranks latency." },
                    "date_from":  { "type": "string", "description": "Inclusive lower date bound (RFC-3339), e.g. 2024-01-01T00:00:00Z." },
                    "date_to":    { "type": "string", "description": "Inclusive upper date bound (RFC-3339)." },
                    "from":       { "type": "array", "items": { "type": "string" }, "description": "Restrict to these senders (any-of)." },
                    "kind":       { "type": "array", "items": { "type": "string", "enum": ["telegram", "claude-transcripts", "agent-history", "email", "slack", "notes", "code", "generic"] }, "description": "Restrict to these source kinds (any-of). The self-poisoning kinds `claude-transcripts` and `agent-history` (agent dotfile session logs/caches under ~/.claude, ~/.codex, ~/.cursor, etc.) are EXCLUDED by default — name one here only when you explicitly want the agent's own past output back in scope." },
                    "include_sources": { "type": "array", "items": { "type": "string" }, "description": "Registered source names that must be present (must)." },
                    "exclude_sources": { "type": "array", "items": { "type": "string" }, "description": "Registered source names that must be absent (must-not)." },
                    "abstain_threshold": { "type": "number", "description": "REQ-6: when set in [0,1], the tool abstains (returns {results:[], abstained:true, reason, confidence}) if the calibrated top-1 confidence is below it, rather than returning low-confidence noise." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "knowledge_get_chunk",
            "description": "Fetch the FULL, uncapped text of one chunk by the stable `chunk_id` that every `knowledge_search` result carries (REQ-10). Use this to drill into a single hit after a bounded search instead of dumping every full chunk into context. Returns { chunk_id, file_path, chunk_index, kind, source_name, payload, text } with the complete chunk text (not the ~2000-char search cap). Returns an empty/`null` chunk if the id is unknown.",
            "inputSchema": {
                "type": "object",
                "properties": { "chunk_id": { "type": "string", "description": "The chunk_id from a knowledge_search result." } },
                "required": ["chunk_id"]
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
        "knowledge_get_chunk" => tool_get_chunk(&args)?,
        "knowledge_list_sources" => tool_list()?,
        "knowledge_index" => tool_index(&args)?,
        other => return Err(anyhow::anyhow!("unknown tool: {other}")),
    };
    Ok(json!({
        "content": [{ "type": "text", "text": payload }],
        "isError": false
    }))
}

/// Parse a repeatable string param: accepts a JSON array of strings or a
/// single string. Unknown/absent → empty. Trims blanks.
fn str_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Compile the MCP `knowledge_search` filter params into an
/// [`ipc::SearchFilter`], mirroring the CLI args and applying the REQ-1
/// default-exclude of `claude-transcripts` (the path the failing agent
/// session used).
fn search_filter_from_args(args: &Value) -> ipc::SearchFilter {
    let opt_str = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let include = str_list(args, "include_sources");
    let opts_in = cli::include_opts_into_excluded_kinds(&include);
    cli::build_search_filter(
        opt_str("date_from"),
        opt_str("date_to"),
        str_list(args, "from"),
        str_list(args, "kind"),
        include,
        str_list(args, "exclude_sources"),
        opts_in,
    )
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
    let filter = search_filter_from_args(args);
    let abstain_threshold = args
        .get("abstain_threshold")
        .and_then(|v| v.as_f64())
        .map(|t| t as f32);
    // Delegate to the shared helper. The daemon owns the NPU, so
    // when it's up we round-trip a single Search/SearchRerank request
    // and avoid loading a second ORT session in this process.
    let outcome = cli::search_outcome_filtered(
        query,
        limit,
        prefix.as_deref(),
        rerank,
        candidates,
        sy_core::Priority::Interactive,
        Some(filter),
        abstain_threshold,
    )?;
    // Coach small models that pass the user's question verbatim ("what to
    // buy for my wife?") instead of a content-descriptive phrase. The hint
    // is advisory only — we never rewrite the query, which would silently
    // change retrieval semantics; the agent reformulates on the next call.
    let hint = looks_like_raw_question(query).then_some(QUERY_HINT);
    // REQ-6: an abstained response carries no results, just the reason +
    // confidence so the agent stops digging instead of quoting noise. A
    // verbatim-question query is exactly when an abstain most needs the
    // reformulation hint, so carry it here too.
    if outcome.abstained {
        let mut env = json!({
            "results": [],
            "abstained": true,
            "reason": ABSTAIN_REASON,
            "confidence": outcome.confidence,
        });
        if let Some(h) = hint {
            env["query_hint"] = json!(h);
        }
        return Ok(serde_json::to_string(&env)?);
    }
    Ok(search_envelope(&outcome.hits, outcome.confidence, hint))
}

/// Heuristic: does `query` read like the user's verbatim natural-language
/// question rather than a content-descriptive search phrase? Small models
/// (e.g. 30B-class) often pass the raw utterance straight through, which
/// embeds poorly against the user's stored text. When this fires the caller
/// attaches a non-destructive `query_hint` to the response. Conservative by
/// design: a false positive only adds an advisory string to a valid result.
fn looks_like_raw_question(query: &str) -> bool {
    let t = query.trim();
    if t.is_empty() {
        return false;
    }
    if t.ends_with('?') {
        return true;
    }
    let lower = t.to_lowercase();
    // Interrogative / imperative-ask openers that signal a question aimed at
    // an assistant rather than keywords aimed at a corpus.
    const LEADS: &[&str] = &[
        "what ", "who ", "whom ", "whose ", "where ", "when ", "why ", "how ",
        "which ", "should ", "could ", "would ", "can i", "can you", "do i",
        "do you", "does ", "is there", "are there", "tell me", "find out",
        "help me", "i want to know", "i need to know",
    ];
    LEADS.iter().any(|p| lower.starts_with(p))
}

/// Advisory attached as `query_hint` when [`looks_like_raw_question`] fires.
const QUERY_HINT: &str = "The `query` looks like the user's verbatim question. \
knowledge_search matches the *content* of the user's stored messages/files, not Q&A — \
reformulate as the topics, entities, names, or keywords you expect to appear in that \
content (e.g. 'wife birthday gift wishlist' instead of 'what to buy for my wife'), and \
narrow with the `from`/`kind`/`date_from` filters when you know the sender, channel, or \
timeframe.";

/// Shape a bounded, non-abstain search envelope (REQ-10). Every result
/// carries the stable `chunk_id` (so the agent can fetch the full chunk via
/// `knowledge_get_chunk`), each `chunk_text` is clipped to `MAX_CHUNK_CHARS`
/// (flagged `truncated`), and the envelope carries `total` (the hit count
/// before any per-response cap) next to the REQ-6 `confidence`/`abstained`.
fn search_envelope(hits: &[ipc::HitRow], confidence: f32, hint: Option<&str>) -> String {
    let arr: Vec<_> = hits
        .iter()
        .map(|h| {
            let (text, truncated) = truncate_chars(&h.chunk_text, MAX_CHUNK_CHARS);
            let mut row = json!({
                "chunk_id": h.chunk_id,
                "score": h.score,
                "file_path": h.file_path,
                "chunk_index": h.chunk_index,
                "chunk_text": text,
            });
            let obj = row.as_object_mut().expect("json object");
            if truncated {
                // Signal that the chunk was clipped so the agent knows to
                // fetch the full text via `knowledge_get_chunk` (or re-read
                // `file_path`) rather than assuming it has the full chunk.
                obj.insert("truncated".into(), json!(true));
                obj.insert("chunk_chars".into(), json!(h.chunk_text.chars().count()));
            }
            if let Some(es) = h.embed_score {
                obj.insert("embed_score".into(), json!(es));
            }
            row
        })
        .collect();
    let mut env = json!({
        "confidence": confidence,
        "abstained": false,
        "total": hits.len(),
        "results": arr,
    });
    if let Some(h) = hint {
        env["query_hint"] = json!(h);
    }
    env.to_string()
}

/// REQ-10 fetch-by-id: resolve one chunk's full (uncapped) text by its stable
/// `chunk_id`. Returns the SPEC §4 shape `{ chunk_id, file_path, chunk_index,
/// kind, source_name, payload, text }`; `text` is never capped (the agent
/// asked for this chunk specifically). An unknown id yields `{ chunk: null }`.
fn tool_get_chunk(args: &Value) -> Result<String> {
    let chunk_id = args
        .get("chunk_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing chunk_id"))?;
    let chunk = cli::get_chunk_row(chunk_id)?;
    match chunk {
        None => Ok(serde_json::to_string(&json!({ "chunk": Value::Null }))?),
        Some(c) => Ok(serde_json::to_string(&json!({
            "chunk_id": c.chunk_id,
            "file_path": c.file_path,
            "chunk_index": c.chunk_index,
            "kind": c.kind,
            "source_name": c.source_name,
            "payload": { "kind": c.kind, "source_name": c.source_name },
            "text": c.text,
        }))?),
    }
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
    fn default_scope_excludes_self_poisoning_kinds() {
        // A search with no kind / include-source args must default-exclude
        // every self-poisoning kind: the agent's own transcripts AND the
        // broader agent-history (dotfile session logs / pasted sessions).
        let filter = search_filter_from_args(&json!({ "query": "новый год" }));
        for dk in cli::DEFAULT_EXCLUDED_KINDS {
            assert!(
                filter.exclude_kinds.contains(&dk.to_string()),
                "default scope must exclude {dk}"
            );
        }
    }

    #[test]
    fn explicit_kind_opts_only_that_kind_back_in() {
        // Naming one kind opts just IT back into scope; the other
        // self-poisoning kinds stay excluded.
        let filter = search_filter_from_args(&json!({
            "query": "q",
            "kind": ["agent-history"],
        }));
        assert!(
            !filter.exclude_kinds.contains(&"agent-history".to_string()),
            "explicit kind:agent-history must override its default exclude"
        );
        assert!(
            filter.exclude_kinds.contains(&"claude-transcripts".to_string()),
            "other self-poisoning kinds stay excluded"
        );
    }

    #[test]
    fn search_results_include_chunk_id_and_total() {
        // REQ-10: a bounded search envelope shapes every result with the
        // stable `chunk_id` (for fetch-by-id) and carries `total` (the hit
        // count before the per-response cap) alongside `truncated`.
        let hits = vec![
            ipc::HitRow {
                score: 0.9,
                chunk_id: "id-a".into(),
                file_path: "/x/a.md".into(),
                chunk_index: 0,
                chunk_text: "a".repeat(MAX_CHUNK_CHARS + 50),
                embed_score: None,
            },
            ipc::HitRow {
                score: 0.8,
                chunk_id: "id-b".into(),
                file_path: "/x/b.md".into(),
                chunk_index: 1,
                chunk_text: "short".into(),
                embed_score: None,
            },
        ];
        let v: Value = serde_json::from_str(&search_envelope(&hits, 0.95, None)).expect("json");
        assert_eq!(v["total"], 2);
        let results = v["results"].as_array().expect("results array");
        assert_eq!(results[0]["chunk_id"], "id-a");
        assert_eq!(results[1]["chunk_id"], "id-b");
        // The long chunk is clipped + flagged; the short one is intact.
        assert_eq!(results[0]["truncated"], true);
        assert!(results[1].get("truncated").is_none());
        // No hint passed → the envelope omits `query_hint` entirely.
        assert!(v.get("query_hint").is_none());
    }

    #[test]
    fn raw_question_queries_are_flagged_for_reformulation() {
        // The exact failure the user reported: a small model passing the
        // verbatim utterance instead of content keywords.
        assert!(looks_like_raw_question("what to buy for my wife?"));
        assert!(looks_like_raw_question("what should I buy for my wife"));
        assert!(looks_like_raw_question("When is my lease up?"));
        assert!(looks_like_raw_question("how do I reset the router"));
        assert!(looks_like_raw_question("tell me about the trip"));
        // Content-descriptive search phrases must NOT be flagged.
        assert!(!looks_like_raw_question("wife birthday gift wishlist"));
        assert!(!looks_like_raw_question("apartment lease renewal deadline"));
        assert!(!looks_like_raw_question("flight booking confirmation Lisbon"));
        assert!(!looks_like_raw_question(""));
    }

    #[test]
    fn envelope_carries_query_hint_when_present() {
        let v: Value =
            serde_json::from_str(&search_envelope(&[], 0.5, Some(QUERY_HINT))).expect("json");
        assert_eq!(v["query_hint"], QUERY_HINT);
    }

    #[test]
    fn truncate_chars_exact_length_is_not_cut() {
        let s = "x".repeat(MAX_CHUNK_CHARS);
        let (out, cut) = truncate_chars(&s, MAX_CHUNK_CHARS);
        assert!(!cut);
        assert_eq!(out.chars().count(), MAX_CHUNK_CHARS);
    }
}
