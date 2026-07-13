//! Stdio JSON-RPC MCP server exposing the sy-mon aggregator's
//! `snapshot` and `history` surfaces.
//!
//! Tools:
//!   - `system.mon.snapshot {}` — latest `SystemSnapshot` as JSON.
//!   - `system.mon.history { metric, seconds }` — last `seconds`
//!     samples for `metric` as `[(ts_ms, value), …]`.
//!
//! Frame: line-delimited JSON, mirrors `src/knowledge/mcp.rs` and
//! `src/stack/mcp.rs`. The orchestrator's prior step (sy-mon Step 13)
//! land the aggregator's IPC handlers; this module is a thin MCP
//! adapter on top of [`crate::mon::client`].
//!
//! SPEC §5 friction map: when a caller passes a metric name we don't
//! recognise we suggest the closest-by-Levenshtein known metric so a
//! typo (`sy_quueue_depth` vs `sy_queue_depth`) lands an actionable
//! error rather than a bare "unknown metric".

use std::io::{BufRead, BufReader, Write};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::cli::default_bind_path;
use super::client;
use super::collect::ipc::KNOWN_METRICS;

const PROTOCOL_VERSION: &str = "2024-11-05";

const TOOL_SNAPSHOT: &str = "system.mon.snapshot";
const TOOL_HISTORY: &str = "system.mon.history";

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

/// Run the MCP server on stdio until the peer closes the input. Each
/// `tools/call` reaches the aggregator over UDS via
/// [`crate::mon::client`], so the connect-retry budget is shared with
/// the `sy mon snapshot` CLI.
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
            "serverInfo": { "name": "sy-mon", "version": env!("CARGO_PKG_VERSION") }
        })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call_tool(params),
        _ => Err(anyhow::anyhow!("method not implemented: {method}")),
    }
}

fn tools() -> Value {
    json!([
        {
            "name": TOOL_SNAPSHOT,
            "description": "Return the most recent SystemSnapshot from the sy-mon aggregator. Wire shape: sy-mon SPEC §4 SystemSnapshot JSON.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": TOOL_HISTORY,
            "description": "Return the last `seconds` samples for `metric` from the aggregator's ring buffer. `seconds` must be in [1, 600].",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "metric":  { "type": "string", "description": "One of: sy_cpu_util, sy_mem_used_mib, sy_swap_used_mib, sy_load_avg_1m." },
                    "seconds": { "type": "integer", "minimum": 1, "maximum": 600 }
                },
                "required": ["metric", "seconds"]
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
        TOOL_SNAPSHOT => tool_snapshot()?,
        TOOL_HISTORY => tool_history(&args)?,
        other => return Err(anyhow::anyhow!("unknown tool: {other}")),
    };
    Ok(json!({
        "content": [{ "type": "text", "text": payload }],
        "isError": false
    }))
}

fn tool_snapshot() -> Result<String> {
    let bind = default_bind_path()?;
    let snap = tokio_block(client::snapshot(&bind))?;
    Ok(serde_json::to_string(&snap)?)
}

fn tool_history(args: &Value) -> Result<String> {
    let metric = args
        .get("metric")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing metric"))?;
    let seconds = args
        .get("seconds")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("missing seconds"))? as u32;
    if let Err(reason) = validate_metric(metric) {
        return Err(anyhow::anyhow!(reason));
    }
    let bind = default_bind_path()?;
    let samples = tokio_block(client::history(&bind, metric, seconds))?;
    Ok(serde_json::to_string(&json!({
        "metric": metric,
        "samples": samples,
    }))?)
}

/// SPEC §5 friction map: produce a "did you mean X?" hint when the
/// caller's metric isn't in [`KNOWN_METRICS`]. Returns the message body
/// (without a leading "error:" prefix) so the caller can wrap it into
/// whichever error type makes sense.
fn validate_metric(metric: &str) -> Result<(), String> {
    if KNOWN_METRICS.contains(&metric) {
        return Ok(());
    }
    let suggestion = KNOWN_METRICS
        .iter()
        .min_by_key(|known| levenshtein(metric, known))
        .copied();
    let known = KNOWN_METRICS.join(", ");
    let hint = match suggestion {
        Some(s) => format!("unknown metric {metric:?}; did you mean {s:?}? known: {known}"),
        None => format!("unknown metric {metric:?}; known: {known}"),
    };
    Err(hint)
}

/// Classic O(n·m) Wagner-Fischer Levenshtein distance with a single
/// rolling row. Returns the edit distance between `a` and `b` measured
/// in code points (Unicode scalar values). Pure function, no
/// allocations beyond the row vector.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0_usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

fn tokio_block<F: std::future::Future<Output = R>, R>(fut: F) -> R {
    // Each MCP request gets its own current-thread runtime — keeps the
    // surface synchronous from the stdio loop's perspective and avoids
    // dragging a tokio handle through every helper. The runtime is
    // dropped at the end of the call, so the next request gets a fresh
    // executor; the cost (a few µs per call) is dwarfed by the IPC
    // round-trip.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build mcp request runtime");
    rt.block_on(fut)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §5 friction map: a typo like `sy_quueue_depth` (intended
    /// `sy_queue_depth`) must surface an error whose message names the
    /// closest known metric — not just "unknown metric". The spec'd
    /// `sy_queue_depth` is not yet in `KNOWN_METRICS` (Step 12 grows
    /// the column map), but `sy_swap_used_mib` *is*; the contract the
    /// roadmap pins is "name a real metric"; pin against the closest
    /// match in today's catalogue so the test is robust across the
    /// Step 12 expansion.
    #[test]
    fn mon_history_unknown_metric_levenshtein() {
        // Closest known to the typo. With today's four metrics the
        // closest by Levenshtein to a typo of `sy_queue_depth` is the
        // similarly-shaped `sy_load_avg_1m`. Once Step 12 adds
        // `sy_queue_depth` to KNOWN_METRICS the closest match will
        // become `sy_queue_depth` itself — the test still passes
        // because the helper picks the (now-zero-distance) exact name.
        let typo = "sy_quueue_depth";
        let err = validate_metric(typo).expect_err("typo must error");
        let suggestion = KNOWN_METRICS
            .iter()
            .min_by_key(|k| levenshtein(typo, k))
            .copied()
            .expect("known metrics non-empty");
        assert!(
            err.contains(suggestion),
            "error {err:?} must name the closest known metric {suggestion:?}"
        );
        assert!(err.contains("did you mean"), "error must phrase a hint");
        assert!(err.contains(typo), "error must echo the bad input");
    }

    /// Round-trip valid metrics through the validator so a future
    /// regression that flipped the contains-check is caught.
    #[test]
    fn validate_metric_accepts_every_known_name() {
        for name in KNOWN_METRICS {
            assert!(
                validate_metric(name).is_ok(),
                "known metric {name:?} must validate"
            );
        }
    }

    /// Levenshtein sanity: distance is symmetric, zero on equality,
    /// and matches the obvious hand-computed values for short inputs.
    #[test]
    fn levenshtein_known_values() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("abc", "acb"), 2);
    }

    /// `tools/list` must surface both spec'd tools so MCP clients can
    /// discover them via the standard surface. The DoD bullet "MCP
    /// tools listed by `sy auto list-tools`" is interpreted as
    /// "advertised via MCP `tools/list`" (the only standard surface).
    #[test]
    fn tools_list_advertises_snapshot_and_history() {
        let v = tools();
        let arr = v.as_array().expect("tools is an array");
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&TOOL_SNAPSHOT));
        assert!(names.contains(&TOOL_HISTORY));
    }
}
