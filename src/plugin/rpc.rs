//! Typed JSON-RPC 2.0 message wrappers for the `sy file` plugin
//! runtime.
//!
//! Implements the request / response / notification / error shapes
//! from [plugin SPEC
//! §4.2.2](../../../specs/research/sy-file-manager-plugins/SPEC.md#422-requests--responses--notifications).
//! The wire codec in [`crate::plugin::transport`] carries
//! `serde_json::Value`; these wrappers are what callers usually
//! construct + `serde_json::to_value` before encoding.
//!
//! Custom error codes match SPEC §4.2.2 verbatim — the integer values
//! are part of the plugin contract and must never drift.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC version literal — the only value the SPEC permits.
pub const JSONRPC_VERSION: &str = "2.0";

/// Plugin called a host fn it did not declare in `[needs]`
/// (SPEC §4.2.2). Equivalent to LSP's reserved range; lifted to
/// `-32099` so JSON-RPC's reserved `-32000..=-32099` range still
/// contains it.
pub const CAP_NOT_GRANTED: i32 = -32099;

/// Host's `api` set is disjoint from manifest's `api_min..api_max`
/// (SPEC §4.2.2).
pub const API_VERSION_MISMATCH: i32 = -32098;

/// Plugin breached memory / CPU / nofile budget (SPEC §4.2.2). The
/// SPEC names this `LIMIT_EXCEEDED` in the §4.2.2 table; we expose
/// the same numeric code under the more behavioural name
/// `RLIMIT_BREACH` (the supervisor in step 10 will signal both
/// readings — same wire code).
pub const RLIMIT_BREACH: i32 = -32097;

/// SPEC §4.2.2 alias for the same `-32097` slot, for consumers that
/// want to match the SPEC table verbatim.
pub const LIMIT_EXCEEDED: i32 = RLIMIT_BREACH;

/// Capability predicate doesn't parse (SPEC §4.2.2).
pub const BAD_PREDICATE: i32 = -32096;

/// `host.fs.*` got a path outside scoped roots (SPEC §4.2.2).
pub const INVALID_PATH: i32 = -32095;

/// Frame exceeds the 16 MiB ceiling enforced by
/// [`crate::plugin::transport::MAX_PAYLOAD_BYTES`]. Not in the SPEC
/// §4.2.2 table verbatim — added in roadmap Step 2 to surface the
/// codec-level too-large error as a stable peer-facing JSON-RPC code
/// instead of an opaque transport `io::Error`. Sits one slot below
/// `INVALID_PATH` so the next SPEC revision can claim it.
pub const FRAME_TOO_LARGE: i32 = -32094;

/// JSON-RPC 2.0 request (SPEC §4.2.2).
///
/// `id` is `serde_json::Value` so both integer and string ids
/// round-trip — the JSON-RPC spec allows either, and LSP/MCP both
/// exercise the string-id path for client-side correlation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Request {
    /// Always `"2.0"` (see [`JSONRPC_VERSION`]).
    pub jsonrpc: String,
    /// Correlation id — integer or string per JSON-RPC 2.0.
    pub id: Value,
    /// Method name. Dot-separated namespace (e.g. `host.fs.read`).
    pub method: String,
    /// Method-specific params. `null` is permitted; default to
    /// `Value::Null` when callers want to omit the field.
    #[serde(default)]
    pub params: Value,
}

/// JSON-RPC 2.0 response (SPEC §4.2.2).
///
/// Exactly one of `result` / `error` is present per the JSON-RPC spec;
/// the host's request layer constructs the variant directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Echoes the request id so multiplexed in-flight requests can be
    /// correlated.
    pub id: Value,
    /// Success payload. Mutually exclusive with `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Failure payload. Mutually exclusive with `result`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObj>,
}

/// JSON-RPC 2.0 notification (SPEC §4.2.2). No `id`, no response
/// expected — used for `$/progress`, `$/preview/update`, `exit`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Notification {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Notification method (e.g. `$/progress`).
    pub method: String,
    /// Notification payload.
    #[serde(default)]
    pub params: Value,
}

/// JSON-RPC 2.0 error object (SPEC §4.2.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorObj {
    /// Numeric error code — see the `CAP_NOT_GRANTED` /
    /// `API_VERSION_MISMATCH` / … constants in this module.
    pub code: i32,
    /// Short human-readable summary. SPEC §4.2.2 examples use the
    /// uppercase enum-style name (e.g. `"CAP_NOT_GRANTED"`).
    pub message: String,
    /// Structured detail block. Defaults to `null` when the error
    /// carries no extra context.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock the SPEC §4.2.2 custom error-code numeric values in place.
    /// These are part of the plugin contract — a silent renumbering
    /// would break every published plugin's error-handling path.
    #[test]
    fn error_codes_match_spec() {
        assert_eq!(CAP_NOT_GRANTED, -32099);
        assert_eq!(API_VERSION_MISMATCH, -32098);
        assert_eq!(LIMIT_EXCEEDED, -32097);
        assert_eq!(RLIMIT_BREACH, -32097);
        assert_eq!(BAD_PREDICATE, -32096);
        assert_eq!(INVALID_PATH, -32095);
        assert_eq!(FRAME_TOO_LARGE, -32094);
    }

    /// Sanity-check the Request / Response / Notification / ErrorObj
    /// shapes round-trip through JSON without losing fields — the
    /// codec carries `Value`, so we serialise via `to_value` first.
    #[test]
    fn shapes_round_trip_through_json() {
        let req = Request {
            jsonrpc: JSONRPC_VERSION.into(),
            id: serde_json::json!(7),
            method: "preview".into(),
            params: serde_json::json!({ "path": "README.md" }),
        };
        let v = serde_json::to_value(&req).expect("to_value");
        let back: Request = serde_json::from_value(v).expect("from_value");
        assert_eq!(back, req);

        let err = ErrorObj {
            code: CAP_NOT_GRANTED,
            message: "CAP_NOT_GRANTED".into(),
            data: serde_json::json!({ "needed": "network" }),
        };
        let resp = Response {
            jsonrpc: JSONRPC_VERSION.into(),
            id: serde_json::json!(7),
            result: None,
            error: Some(err.clone()),
        };
        let v = serde_json::to_value(&resp).expect("to_value");
        let back: Response = serde_json::from_value(v).expect("from_value");
        assert_eq!(back, resp);

        let note = Notification {
            jsonrpc: JSONRPC_VERSION.into(),
            method: "$/progress".into(),
            params: serde_json::json!({ "op_id": "x", "done": 1, "total": 2 }),
        };
        let v = serde_json::to_value(&note).expect("to_value");
        let back: Notification = serde_json::from_value(v).expect("from_value");
        assert_eq!(back, note);
    }
}
