//! SPEC §4.2 wire shape: `Request`, `Response`, `ErrorBody`, `BlobRef`.
//!
//! Pure data types. Serialisation is the contract — byte-for-byte
//! compatibility with the SPEC §4.2 examples is enforced by the
//! round-trip tests in this module. Deviations require a SPEC
//! amendment (not a unilateral type tweak here).

use serde::{Deserialize, Serialize};
use sy_core::{ErrorCode, Priority};
use ulid::Ulid;

// `TraceId` and `SpanId` moved to `sy_core::trace` in arch-observability
// Step 4 — they are observability primitives, not IPC primitives. The
// re-export below keeps every `arch-ipc-v1` Step 1 call site that
// imports `sy_ipc::envelope::{TraceId, SpanId}` working unchanged.
// Wire format is unaffected: both types still serialise to a bare hex
// string via `#[serde(transparent)]`.
pub use sy_core::trace::{SpanId, TraceId};

/// Wire schema version. `1` is the inaugural cutover (SPEC §3.4 "no
/// backward-compat for unversioned IPC"). Bumping this is a flag-day
/// upgrade across all four daemons.
pub const SCHEMA_VERSION: u32 = 1;

/// IPC v1 request envelope (SPEC §4.2). Field order matches the
/// canonical SPEC example; renaming or removing a field is a wire
/// break that mandates a `SCHEMA_VERSION` bump.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub schema_version: u32,
    pub request_id: Ulid,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trace_id: Option<TraceId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parent_span_id: Option<SpanId>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub deadline_ms: Option<u64>,
    pub priority: Priority,
    pub method: String,
    pub params: serde_json::Value,
}

/// IPC v1 response envelope (SPEC §4.2). Untagged at the wire,
/// distinguished by `result` (Ok) vs `error` (Err) field presence.
/// Both variants carry `schema_version` and `request_id` at the top
/// level — a deserialise that lacks `result` and `error` is malformed
/// and bubbles up as a serde error to the caller (it is not silently
/// interpreted as one variant or the other).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Ok {
        schema_version: u32,
        request_id: Ulid,
        result: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        blob: Option<BlobRef>,
    },
    Err {
        schema_version: u32,
        request_id: Ulid,
        error: ErrorBody,
    },
}

/// Out-of-band blob descriptor. The fd itself rides on `SCM_RIGHTS`
/// alongside the JSON frame (SPEC §4.2 "Blob channel"); this struct
/// just carries the metadata that lets the receiver mmap + verify.
/// memfd seals (`F_SEAL_WRITE|SHRINK|GROW`) are checked by the
/// receiver before mmap — this layer doesn't touch the fd, so the
/// seal check lives in the consumer of `sy-ipc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub kind: BlobKind,
    pub len: u64,
    pub sha256: String,
}

/// Variants reserved for future transports (SPEC §3.3 Zone 2 "OUT"
/// covers the memfd channel; bus/network transports are explicitly
/// off the table per SPEC §3.4). `Memfd` is the only kind in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobKind {
    Memfd,
}

/// Structured error body for the response envelope (SPEC §4.2 error
/// example). `code` is the wire-stable `ErrorCode` from `sy-core`;
/// `retry_after_ms` is set only when the daemon wants the caller to
/// back off (e.g. `Overloaded`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub retry_after_ms: Option<u64>,
    #[serde(default)]
    pub details: serde_json::Value,
}

/// Strict request parser. Beyond plain `serde_json::from_slice`,
/// this enforces SPEC §3.4 "daemons reject `null`/missing versions
/// with `IncompatibleSchema`" by translating two specific failure
/// modes — missing `schema_version` and a non-`SCHEMA_VERSION` value
/// — into a tagged `ParseRequestError::IncompatibleSchema`. Other
/// shape errors (bad ULID, unknown priority, malformed JSON) fall
/// through as `BadRequest`.
pub fn parse_request_strict(bytes: &[u8]) -> Result<Request, ParseRequestError> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| ParseRequestError::BadRequest(e.to_string()))?;
    let obj = v
        .as_object()
        .ok_or_else(|| ParseRequestError::BadRequest("request must be a JSON object".into()))?;
    match obj.get("schema_version") {
        None => return Err(ParseRequestError::IncompatibleSchema { got: None }),
        Some(sv) => match sv.as_u64() {
            None => {
                return Err(ParseRequestError::IncompatibleSchema { got: None });
            }
            Some(n) if n != u64::from(SCHEMA_VERSION) => {
                return Err(ParseRequestError::IncompatibleSchema {
                    got: u32::try_from(n).ok(),
                });
            }
            Some(_) => {}
        },
    }
    serde_json::from_value(v).map_err(|e| ParseRequestError::BadRequest(e.to_string()))
}

/// Failure modes for `parse_request_strict`. `IncompatibleSchema`
/// maps onto the `ErrorCode::IncompatibleSchema` wire response;
/// `BadRequest` maps onto `ErrorCode::BadRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseRequestError {
    IncompatibleSchema { got: Option<u32> },
    BadRequest(String),
}

impl std::fmt::Display for ParseRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseRequestError::IncompatibleSchema { got: Some(v) } => {
                write!(
                    f,
                    "incompatible schema_version: got {v}, want {SCHEMA_VERSION}"
                )
            }
            ParseRequestError::IncompatibleSchema { got: None } => {
                write!(f, "incompatible schema_version: missing or non-integer")
            }
            ParseRequestError::BadRequest(msg) => write!(f, "bad request: {msg}"),
        }
    }
}

impl std::error::Error for ParseRequestError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trip() {
        let req = Request {
            schema_version: SCHEMA_VERSION,
            request_id: Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid"),
            trace_id: Some(TraceId("0af7651916cd43dd8448eb211c80319c".into())),
            parent_span_id: Some(SpanId("b7ad6b7169203331".into())),
            deadline_ms: Some(5000),
            priority: Priority::Interactive,
            method: "aiplane.run".into(),
            params: json!({ "workload": "embed", "input": "hi", "blob": null }),
        };
        let s = serde_json::to_string(&req).expect("serialize");
        let back: Request = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(req, back);
    }

    #[test]
    fn response_ok_and_err_share_shape() {
        // SPEC §4.2: both response variants surface `schema_version`
        // and `request_id` at the top level. A regression that
        // tucked either field inside `result`/`error` would invalidate
        // every example in the doc and break `system.cancel` correlation.
        let id = Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");
        let ok = Response::Ok {
            schema_version: SCHEMA_VERSION,
            request_id: id,
            result: serde_json::json!({"hits": []}),
            blob: None,
        };
        let err = Response::Err {
            schema_version: SCHEMA_VERSION,
            request_id: id,
            error: ErrorBody {
                code: ErrorCode::Overloaded,
                message: "queue full for class=Background".into(),
                retry_after_ms: Some(200),
                details: serde_json::json!({"class": "Background"}),
            },
        };
        for (label, resp) in [("ok", &ok), ("err", &err)] {
            let v: serde_json::Value =
                serde_json::to_value(resp).unwrap_or_else(|e| panic!("{label}: {e}"));
            let obj = v
                .as_object()
                .unwrap_or_else(|| panic!("{label} not object"));
            assert_eq!(
                obj.get("schema_version")
                    .and_then(serde_json::Value::as_u64),
                Some(u64::from(SCHEMA_VERSION)),
                "{label} missing schema_version at top level"
            );
            assert!(
                obj.contains_key("request_id"),
                "{label} missing request_id at top level"
            );
        }
        // Untagged round-trip preserves variant identity via the
        // result-vs-error distinguisher.
        let ok_json = serde_json::to_string(&ok).expect("serialize ok");
        let err_json = serde_json::to_string(&err).expect("serialize err");
        let ok_back: Response = serde_json::from_str(&ok_json).expect("deserialize ok");
        let err_back: Response = serde_json::from_str(&err_json).expect("deserialize err");
        assert_eq!(ok, ok_back);
        assert_eq!(err, err_back);
    }

    #[test]
    fn missing_schema_version_rejects() {
        // SPEC §3.4 anti-goal: daemons reject `null`/missing versions
        // with `IncompatibleSchema`. A bare JSON object without the
        // field must not be silently coerced to the current schema.
        let raw = br#"{
            "request_id": "01HXYZ0000000000000000000Z",
            "priority": "Interactive",
            "method": "knowledge.search",
            "params": {}
        }"#;
        match parse_request_strict(raw) {
            Err(ParseRequestError::IncompatibleSchema { got: None }) => {}
            other => panic!("expected IncompatibleSchema {{ got: None }}, got {other:?}"),
        }
    }

    #[test]
    fn wrong_schema_version_rejects() {
        // A request that advertises `schema_version: 2` must surface
        // as `IncompatibleSchema { got: Some(2) }`; the daemon never
        // tries to parse it under v1 semantics.
        let raw = br#"{
            "schema_version": 2,
            "request_id": "01HXYZ0000000000000000000Z",
            "priority": "Interactive",
            "method": "knowledge.search",
            "params": {}
        }"#;
        match parse_request_strict(raw) {
            Err(ParseRequestError::IncompatibleSchema { got: Some(2) }) => {}
            other => panic!("expected IncompatibleSchema {{ got: Some(2) }}, got {other:?}"),
        }
    }

    #[test]
    fn parse_request_strict_accepts_canonical_v1() {
        // Smoke: the strict parser must still admit a well-formed v1
        // request. Guards against an over-eager reject from a future
        // tightening of the validator.
        let req = Request {
            schema_version: SCHEMA_VERSION,
            request_id: Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid"),
            trace_id: None,
            parent_span_id: None,
            deadline_ms: None,
            priority: Priority::Interactive,
            method: "knowledge.search".into(),
            params: serde_json::json!({"q": "hi"}),
        };
        let bytes = serde_json::to_vec(&req).expect("serialize");
        let back = parse_request_strict(&bytes).expect("strict parse");
        assert_eq!(req, back);
    }
}
