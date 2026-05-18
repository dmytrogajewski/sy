//! v1 method shim for `sy-agentd`. Translates the domain
//! `ClientReq`/`ClientReply` shapes onto the IPC v1 envelope from
//! [`sy_ipc`] (SPEC §4.2). The daemon's bridge handler unpacks a
//! [`sy_ipc::Request`] into a [`ClientReq`] via [`from_request`];
//! the client builds an envelope via [`to_request`].

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::agt::protocol::{ClientReply, ClientReq, DaemonEvent};

/// All v1 method names served by `sy-agentd`. Surfaced through
/// `system.describe` so clients can introspect the surface.
pub const METHOD_RUN: &str = "agt.run";
pub const METHOD_LIST: &str = "agt.list";
pub const METHOD_PROMPT: &str = "agt.prompt";
pub const METHOD_STOP: &str = "agt.stop";
pub const METHOD_TAIL: &str = "agt.tail";
pub const METHOD_PERMISSION_DECISION: &str = "agt.permission_decision";
pub const METHOD_DIAG: &str = "agt.diag";
pub const METHOD_SHUTDOWN: &str = "agt.shutdown";
/// arch-agent-sandbox Step 6: TTY-driven consent approval. Body:
/// `{"token": "<uuid>"}`. Returns `{"approved": "<uuid>"}` on success
/// or `BadRequest` if the token is unknown / expired.
pub const METHOD_APPROVE: &str = "agt.approve";

/// Streaming event kinds the daemon emits on `agt.tail`. Wire-stable
/// strings matching `DaemonEvent`'s `kind` discriminator after the
/// envelope split (Step 6 moves event delivery from the in-band
/// `ClientReply::Event` wrapper to a separate streaming envelope).
pub const EVENT_TRANSCRIPT: &str = "transcript";
pub const EVENT_STATUS: &str = "status";
pub const EVENT_PERMISSION: &str = "permission";
pub const EVENT_CLOSED: &str = "closed";

pub const ALL_METHODS: &[&str] = &[
    METHOD_RUN,
    METHOD_LIST,
    METHOD_PROMPT,
    METHOD_STOP,
    METHOD_TAIL,
    METHOD_PERMISSION_DECISION,
    METHOD_DIAG,
    METHOD_SHUTDOWN,
    METHOD_APPROVE,
];

/// Marshal a [`ClientReq`] into the v1 `(method, params)` pair.
pub fn to_request(req: &ClientReq) -> (&'static str, Value) {
    match req {
        ClientReq::Run { agent, cwd, prompt } => (
            METHOD_RUN,
            serde_json::json!({
                "agent": agent,
                "cwd": cwd,
                "prompt": prompt,
            }),
        ),
        ClientReq::List => (METHOD_LIST, serde_json::json!({})),
        ClientReq::Prompt { session_id, text } => (
            METHOD_PROMPT,
            serde_json::json!({
                "session_id": session_id,
                "text": text,
            }),
        ),
        ClientReq::Stop { session_id } => {
            (METHOD_STOP, serde_json::json!({ "session_id": session_id }))
        }
        ClientReq::Tail {
            session_id,
            follow,
            replay,
        } => (
            METHOD_TAIL,
            serde_json::json!({
                "session_id": session_id,
                "follow": follow,
                "replay": replay,
            }),
        ),
        ClientReq::PermissionDecision { request_id, allow } => (
            METHOD_PERMISSION_DECISION,
            serde_json::json!({
                "request_id": request_id,
                "allow": allow,
            }),
        ),
        ClientReq::Diag => (METHOD_DIAG, serde_json::json!({})),
        ClientReq::Shutdown => (METHOD_SHUTDOWN, serde_json::json!({})),
    }
}

/// Inverse of [`to_request`]: parse an incoming v1 envelope back into
/// a `ClientReq`. Returns `Err` for unknown methods and for malformed
/// `params` so the daemon answers `BadRequest`.
pub fn from_request(method: &str, params: &Value) -> Result<ClientReq> {
    match method {
        METHOD_RUN => Ok(ClientReq::Run {
            agent: pluck_str(params, "agent")?,
            cwd: PathBuf::from(pluck_str(params, "cwd")?),
            prompt: pluck_str(params, "prompt")?,
        }),
        METHOD_LIST => Ok(ClientReq::List),
        METHOD_PROMPT => Ok(ClientReq::Prompt {
            session_id: pluck_str(params, "session_id")?,
            text: pluck_str(params, "text")?,
        }),
        METHOD_STOP => Ok(ClientReq::Stop {
            session_id: pluck_str(params, "session_id")?,
        }),
        METHOD_TAIL => Ok(ClientReq::Tail {
            session_id: pluck_str(params, "session_id")?,
            follow: pluck_bool(params, "follow")?,
            replay: pluck_bool(params, "replay")?,
        }),
        METHOD_PERMISSION_DECISION => Ok(ClientReq::PermissionDecision {
            request_id: pluck_str(params, "request_id")?,
            allow: pluck_bool(params, "allow")?,
        }),
        METHOD_DIAG => Ok(ClientReq::Diag),
        METHOD_SHUTDOWN => Ok(ClientReq::Shutdown),
        other => Err(anyhow!("unknown agt method: {other}")),
    }
}

/// Pack a non-streaming `ClientReply` into the v1 `Response.result`
/// JSON payload. Streaming replies (`ClientReply::Event`) flow over
/// the streaming envelope in `sy_ipc::stream::Event` instead — the
/// daemon never wraps an event in a Response.
pub fn reply_to_result(reply: &ClientReply) -> Result<Value> {
    match reply {
        ClientReply::RunReply { session_id } => Ok(serde_json::json!({
            "session_id": session_id,
        })),
        ClientReply::ListReply { sessions } => Ok(serde_json::json!({
            "sessions": sessions,
        })),
        ClientReply::Ack => Ok(serde_json::json!({})),
        ClientReply::DiagReply { agents } => Ok(serde_json::json!({
            "agents": agents,
        })),
        ClientReply::Event { .. } => Err(anyhow!(
            "ClientReply::Event must flow as a streaming sy_ipc::Event, not Response.result"
        )),
        ClientReply::Error { message, code } => Err(anyhow!("{message} (legacy code={code})")),
    }
}

/// Inverse of [`reply_to_result`]: build a `ClientReply` from the
/// `Response.result` JSON the daemon returned for `method`.
pub fn result_to_reply(method: &str, result: &Value) -> Result<ClientReply> {
    match method {
        METHOD_RUN => Ok(ClientReply::RunReply {
            session_id: pluck_str(result, "session_id")?,
        }),
        METHOD_LIST => {
            let sessions = result
                .get("sessions")
                .cloned()
                .unwrap_or(Value::Array(Vec::new()));
            Ok(ClientReply::ListReply {
                sessions: serde_json::from_value(sessions)?,
            })
        }
        METHOD_DIAG => {
            let agents = result
                .get("agents")
                .cloned()
                .unwrap_or(Value::Array(Vec::new()));
            Ok(ClientReply::DiagReply {
                agents: serde_json::from_value(agents)?,
            })
        }
        METHOD_PROMPT | METHOD_STOP | METHOD_PERMISSION_DECISION | METHOD_SHUTDOWN => {
            Ok(ClientReply::Ack)
        }
        METHOD_TAIL => Ok(ClientReply::Ack),
        other => Err(anyhow!("no reply mapping for method: {other}")),
    }
}

/// Build an [`sy_ipc::Event`] params payload for a [`DaemonEvent`]
/// fired during `agt.tail` streaming. The envelope's `kind` discriminator
/// stays attached to the inner event so consumers can keep using
/// `serde(tag = "kind")` to deserialise.
pub fn event_to_stream_payload(event: &DaemonEvent) -> (&'static str, Value) {
    let kind = match event {
        DaemonEvent::Transcript { .. } => EVENT_TRANSCRIPT,
        DaemonEvent::Status { .. } => EVENT_STATUS,
        DaemonEvent::Permission { .. } => EVENT_PERMISSION,
        DaemonEvent::Closed { .. } => EVENT_CLOSED,
    };
    // Strip the outer `kind` wrapper that `serde(tag = "kind")`
    // injects — clients reattach it via [`stream_payload_to_event`]
    // before deserialising. Keeps the wire payload flat and matches
    // the rest of the v1 namespace style.
    let mut v = serde_json::to_value(event).unwrap_or(Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.remove("kind");
    }
    (kind, v)
}

/// Inverse of [`event_to_stream_payload`]: re-attach the outer `kind`
/// tag from the [`sy_ipc::Event::kind`] field and deserialise into a
/// `DaemonEvent`.
pub fn stream_payload_to_event(kind: &str, payload: Value) -> Result<DaemonEvent> {
    let mut v = payload;
    if !v.is_object() {
        v = serde_json::json!({});
    }
    let tag = match kind {
        EVENT_TRANSCRIPT => "Transcript",
        EVENT_STATUS => "Status",
        EVENT_PERMISSION => "Permission",
        EVENT_CLOSED => "Closed",
        other => return Err(anyhow!("unknown agt event kind: {other}")),
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert("kind".into(), Value::String(tag.into()));
    }
    Ok(serde_json::from_value(v)?)
}

fn pluck_str(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing string field: {key}"))
}

fn pluck_bool(v: &Value, key: &str) -> Result<bool> {
    v.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("missing bool field: {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agt::protocol::{SessionStatus, TranscriptEntry};

    #[test]
    fn request_round_trip_preserves_run() {
        let req = ClientReq::Run {
            agent: "claude".into(),
            cwd: PathBuf::from("/tmp"),
            prompt: "hi".into(),
        };
        let (method, params) = to_request(&req);
        assert_eq!(method, METHOD_RUN);
        let back = from_request(method, &params).expect("from_request");
        match (req, back) {
            (
                ClientReq::Run {
                    agent: a1,
                    cwd: c1,
                    prompt: p1,
                },
                ClientReq::Run {
                    agent: a2,
                    cwd: c2,
                    prompt: p2,
                },
            ) => {
                assert_eq!(a1, a2);
                assert_eq!(c1, c2);
                assert_eq!(p1, p2);
            }
            other => panic!("variant mismatch: {other:?}"),
        }
    }

    #[test]
    fn event_round_trip_preserves_transcript() {
        let evt = DaemonEvent::Transcript {
            session_id: "abc".into(),
            entry: TranscriptEntry::AgentText {
                text: "hello".into(),
            },
            ts: "2026-01-01T00:00:00Z".into(),
        };
        let (kind, payload) = event_to_stream_payload(&evt);
        assert_eq!(kind, EVENT_TRANSCRIPT);
        let back = stream_payload_to_event(kind, payload).expect("decode");
        match back {
            DaemonEvent::Transcript {
                session_id, entry, ..
            } => {
                assert_eq!(session_id, "abc");
                assert!(matches!(entry, TranscriptEntry::AgentText { text } if text == "hello"));
            }
            other => panic!("variant mismatch: {other:?}"),
        }
    }

    #[test]
    fn event_round_trip_preserves_status() {
        let evt = DaemonEvent::Status {
            session_id: "abc".into(),
            status: SessionStatus::Working,
        };
        let (kind, payload) = event_to_stream_payload(&evt);
        assert_eq!(kind, EVENT_STATUS);
        let back = stream_payload_to_event(kind, payload).expect("decode");
        match back {
            DaemonEvent::Status { status, .. } => {
                assert!(matches!(status, SessionStatus::Working));
            }
            other => panic!("variant mismatch: {other:?}"),
        }
    }

    #[test]
    fn from_request_rejects_unknown_method() {
        assert!(from_request("agt.nonsense", &serde_json::json!({})).is_err());
    }
}
