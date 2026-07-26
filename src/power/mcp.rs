//! Stdio JSON-RPC MCP server exposing one tool — `power_status` — so
//! agents can self-throttle (SPEC §3 ML "IN" list). Mirrors the
//! transport shape of `src/knowledge/mcp.rs` and `src/stack/mcp.rs`:
//! line-delimited JSON, one request per line, one response per line.
//!
//! Tools advertised:
//!   - `power_status` — return the live `sy.power.status/v1` document
//!     the daemon would emit on `sy power status --json`.
//!
//! The handler is decoupled from the daemon dial via the
//! [`StatusFetcher`] trait — production wires
//! [`SystemStatusFetcher`] which dials the live IPC socket; the
//! unit tests inject a canned `StatusResponse` so the contract can be
//! pinned without standing up `sy-powerd`.

use std::io::{BufRead, Write};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::ipc::STATUS_SCHEMA;

/// MCP protocol revision implemented by every `sy *` server in tree —
/// pinned across `stack`, `knowledge`, and now `power` so an upstream
/// host can negotiate one capability table.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// The single tool name advertised by `sy power mcp`. Stable wire
/// identifier — bumping it is a breaking change to every agent that
/// has it baked into a prompt template.
pub const POWER_STATUS_TOOL: &str = "power_status";

/// JSON-RPC error code returned when the daemon is unreachable. Distinct
/// from the SPEC §4 CLI exit code (4) — JSON-RPC is its own contract.
/// `-32000` is the documented "server error" band; the textual
/// `message` carries the actionable detail.
const ERR_DAEMON_UNREACHABLE: i64 = -32000;

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

/// Trait the `power_status` tool reads from. Production wires
/// [`SystemStatusFetcher`]; tests inject a canned response so the
/// handler contract can be pinned without a live daemon.
pub trait StatusFetcher {
    /// Return the live `sy.power.status/v1` document. Errors map to a
    /// JSON-RPC error frame with [`ERR_DAEMON_UNREACHABLE`].
    fn fetch_status_v1(&self) -> Result<Value>;
}

/// Production fetcher — dials `super::daemon::socket_path()` over the
/// same Unix socket `sy power status` uses, builds the `sy.power.status/v1`
/// value, and returns it as a `serde_json::Value`.
pub struct SystemStatusFetcher;

impl StatusFetcher for SystemStatusFetcher {
    fn fetch_status_v1(&self) -> Result<Value> {
        super::cli::build_live_status_value()
    }
}

/// Run the MCP server on stdin/stdout against the production
/// [`SystemStatusFetcher`]. Returns when stdin reaches EOF.
pub fn run() -> Result<()> {
    let fetcher = SystemStatusFetcher;
    run_with(&fetcher, std::io::stdin().lock(), std::io::stdout().lock())
}

/// Drive the stdio loop against an injected fetcher + reader/writer.
/// Extracted for tests; production callers go through [`run`].
pub fn run_with<R, W>(fetcher: &dyn StatusFetcher, reader: R, mut writer: W) -> Result<()>
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
        let resp = handle(fetcher, &req.method, &req.params)
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

/// Dispatch one JSON-RPC method against the injected fetcher.
fn handle(fetcher: &dyn StatusFetcher, method: &str, params: &Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "sy-power", "version": env!("CARGO_PKG_VERSION") }
        })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call_tool(fetcher, params),
        _ => Err(anyhow::anyhow!("method not implemented: {method}")),
    }
}

/// Static tool table. One entry — `power_status` — exposed today; future
/// agent-facing reads (`power_explain`, etc.) extend this list.
fn tools() -> Value {
    json!([
        {
            "name": POWER_STATUS_TOOL,
            "description": "Return the current sy-power orchestrator status (sensors, applied arm, shield state, onboarding, drift). Mirrors `sy power status --json` — the response is the live sy.power.status/v1 document.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            },
            "outputSchema": {
                "type": "object",
                "description": STATUS_SCHEMA
            }
        }
    ])
}

/// Dispatch a `tools/call` request. Today only `power_status` is
/// recognised; any other name surfaces as a JSON-RPC error so a
/// mistyped tool name fails loudly.
fn call_tool(fetcher: &dyn StatusFetcher, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("tools/call missing name"))?;
    let payload = match name {
        POWER_STATUS_TOOL => fetcher.fetch_status_v1()?,
        other => return Err(anyhow::anyhow!("unknown tool: {other}")),
    };
    let text =
        serde_json::to_string(&payload).map_err(|e| anyhow::anyhow!("serialise status: {e}"))?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::config::PowerConfig;
    use crate::power::drift::DriftStatus;
    use crate::power::ipc::StatusResponse;
    use crate::power::shield::ShieldState;
    use crate::power::status::build_status_value;

    /// Canned-response fetcher for the unit tests. Returns the v1
    /// document derived from an `empty_response`-style `StatusResponse`
    /// so the handler contract can be pinned without a daemon.
    struct MockStatusFetcher {
        value: Value,
    }

    impl StatusFetcher for MockStatusFetcher {
        fn fetch_status_v1(&self) -> Result<Value> {
            Ok(self.value.clone())
        }
    }

    /// Empty-snapshot fixture: shared with `status.rs::tests::empty_response`
    /// (DRY would be tighter if `status` exposed it, but the cross-module
    /// re-export pulls a lot of `pub(crate)` baggage; copy-paste keeps
    /// the mcp tests hermetic).
    fn canned_v1_value() -> Value {
        let resp = StatusResponse {
            schema: STATUS_SCHEMA.to_string(),
            snapshot_hash: "deadbeef".into(),
            snapshot: json!({}),
            last_audit: None,
            drift: DriftStatus::default(),
            model: None,
            onboarding: None,
        };
        let cfg = PowerConfig::default();
        let onboarding = super::super::onboarding::OnboardingStatus {
            active: true,
            days_collected: 0,
            ready_at: chrono::Utc::now(),
        };
        build_status_value(&resp, &cfg, ShieldState::CoolAc, &onboarding)
    }

    /// Confirm the tool descriptor exposes one tool — `power_status` —
    /// with an empty `inputSchema` and an `outputSchema` advertising
    /// the `sy.power.status/v1` schema id.
    #[test]
    fn tool_schema_matches_status_v1() {
        let list = tools();
        let arr = list.as_array().expect("tools must be a JSON array");
        assert_eq!(arr.len(), 1, "exactly one tool advertised");
        let t = &arr[0];
        assert_eq!(t["name"].as_str(), Some(POWER_STATUS_TOOL));
        let input = t["inputSchema"]
            .as_object()
            .expect("inputSchema must be an object");
        assert_eq!(
            input["type"].as_str(),
            Some("object"),
            "inputSchema.type must be object"
        );
        let props = input["properties"]
            .as_object()
            .expect("inputSchema.properties must be an object");
        assert!(props.is_empty(), "power_status takes no arguments");
        let required = input["required"]
            .as_array()
            .expect("inputSchema.required must be an array");
        assert!(required.is_empty(), "power_status has no required fields");
        assert_eq!(
            t["outputSchema"]["description"].as_str(),
            Some(STATUS_SCHEMA),
            "outputSchema must advertise the sy.power.status/v1 schema id"
        );
    }

    /// Feed a mock `tools/call power_status` request through the
    /// handler; assert the response carries the SPEC §4 keys
    /// (`schema`, `sensors`, `bandit`, `shield_state`, `applied_policy`,
    /// `drift`, `onboarding`).
    #[test]
    fn call_returns_live_status() {
        let fetcher = MockStatusFetcher {
            value: canned_v1_value(),
        };
        let params = json!({"name": POWER_STATUS_TOOL, "arguments": {}});
        let out = call_tool(&fetcher, &params).expect("call_tool must succeed");
        assert_eq!(out["isError"].as_bool(), Some(false));
        let content = out["content"].as_array().expect("content must be an array");
        assert_eq!(content.len(), 1, "single text block");
        assert_eq!(content[0]["type"].as_str(), Some("text"));
        let text = content[0]["text"].as_str().expect("text must be a string");
        let parsed: Value = serde_json::from_str(text).expect("text must parse as JSON");
        for key in [
            "schema",
            "ts",
            "onboarding",
            "model",
            "shield_state",
            "activity_label",
            "forecast",
            "bandit",
            "applied_policy",
            "sensors",
            "drift",
            "snapshot_hash",
        ] {
            assert!(
                parsed.get(key).is_some(),
                "v1 schema missing key {key:?}: {parsed}"
            );
        }
        assert_eq!(parsed["schema"].as_str(), Some(STATUS_SCHEMA));
    }

    /// MCP error path: when the fetcher reports daemon-unreachable,
    /// the JSON-RPC frame surfaces an `error` object — not a
    /// transport-level panic. Agents see a clean error.
    #[test]
    fn daemon_down_surfaces_jsonrpc_error() {
        struct Down;
        impl StatusFetcher for Down {
            fn fetch_status_v1(&self) -> Result<Value> {
                Err(anyhow::anyhow!("sy-powerd unreachable: ENOENT"))
            }
        }
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"power_status\",\"arguments\":{}}}\n";
        let mut buf: Vec<u8> = Vec::new();
        run_with(&Down, &req[..], &mut buf).expect("loop must terminate cleanly");
        let line = std::str::from_utf8(&buf).expect("utf8");
        let parsed: Value = serde_json::from_str(line.trim()).expect("response is JSON");
        assert!(parsed["error"].is_object(), "error frame expected");
        assert_eq!(
            parsed["error"]["code"].as_i64(),
            Some(ERR_DAEMON_UNREACHABLE)
        );
        assert!(parsed["result"].is_null(), "no result on error path");
    }

    /// Stdio handshake: `initialize` → `tools/list` → `tools/call
    /// power_status`. Round-trips through `run_with` against a canned
    /// fetcher so the line-framed transport stays pinned.
    #[test]
    fn stdio_handshake_round_trips() {
        let fetcher = MockStatusFetcher {
            value: canned_v1_value(),
        };
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n\
                      {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n\
                      {\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"power_status\",\"arguments\":{}}}\n";
        let mut buf: Vec<u8> = Vec::new();
        run_with(&fetcher, &input[..], &mut buf).expect("loop must terminate cleanly");
        let lines: Vec<&str> = std::str::from_utf8(&buf).expect("utf8").lines().collect();
        assert_eq!(lines.len(), 3, "one response per request");
        let init: Value = serde_json::from_str(lines[0]).expect("init JSON");
        assert_eq!(init["id"].as_i64(), Some(1));
        assert_eq!(
            init["result"]["serverInfo"]["name"].as_str(),
            Some("sy-power")
        );
        let list: Value = serde_json::from_str(lines[1]).expect("list JSON");
        assert_eq!(list["id"].as_i64(), Some(2));
        let tools = list["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"].as_str(), Some(POWER_STATUS_TOOL));
        let call: Value = serde_json::from_str(lines[2]).expect("call JSON");
        assert_eq!(call["id"].as_i64(), Some(3));
        assert_eq!(call["result"]["isError"].as_bool(), Some(false));
    }
}
