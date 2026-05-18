//! Synchronous IPC client used by `sy agt list/prompt/stop/run/diag/tail`.
//! Short-lived: open socket, send one v1 [`sy_ipc::Request`], read one
//! [`sy_ipc::Response`] (or stream [`sy_ipc::Event`] frames for `agt.tail`),
//! close.

use std::{
    io::{BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use sy_core::Priority;
use sy_ipc::{
    blocking::{build_request, read_event, read_response, write_request},
    Event, Response,
};
use ulid::Ulid;

use crate::agt::{
    protocol::{exit, ClientReply, ClientReq},
    socket_path, wire, AgtError,
};

/// 30 s deadline matches the legacy line-JSON read timeout — covers
/// the heaviest agentd operations (ACP `initialize` round-trip on
/// cold-start agents).
const CLIENT_DEADLINE_MS: u64 = 30_000;

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect() -> Result<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path).map_err(|e| AgtError {
            code: exit::DAEMON_UNAVAILABLE,
            msg: format!(
                "connect {} (is sy-agentd running? `sy agt diag` to check): {e}",
                path.display()
            ),
        })?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let writer = stream.try_clone().map_err(|e| AgtError {
            code: exit::DAEMON_UNAVAILABLE,
            msg: format!("clone socket: {e}"),
        })?;
        let reader = BufReader::new(stream);
        Ok(Self { reader, writer })
    }

    /// Send a unary [`ClientReq`] and block for one [`ClientReply`].
    pub fn round_trip(&mut self, req: &ClientReq) -> Result<ClientReply> {
        let (method, params) = wire::to_request(req);
        let envelope = build_request(
            method,
            params,
            Priority::Interactive,
            Some(CLIENT_DEADLINE_MS),
            None,
            None,
            None,
        );
        write_request(&mut self.writer, &envelope).context("write request")?;
        self.writer.flush().context("flush request")?;
        let resp = read_response(&mut self.reader).context("read response")?;
        decode_response(method, resp)
    }

    /// Send a unary IPC v1 request by raw `(method, params)` and
    /// return the parsed `Response.result` JSON. Used by the
    /// non-domain-modelled methods (`agt.approve`, future
    /// `agt.grant`) that don't fit the `ClientReq` enum.
    pub fn call_raw(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let envelope = build_request(
            method,
            params,
            Priority::Interactive,
            Some(CLIENT_DEADLINE_MS),
            None,
            None,
            None,
        );
        write_request(&mut self.writer, &envelope).context("write request")?;
        self.writer.flush().context("flush request")?;
        let resp = read_response(&mut self.reader).context("read response")?;
        match resp {
            Response::Ok { result, .. } => Ok(result),
            Response::Err { error, .. } => Err(AgtError {
                code: exit::DAEMON_UNAVAILABLE,
                msg: format!("{}: {}", error.code, error.message),
            }
            .into()),
        }
    }

    /// One-shot send for streaming methods. Caller follows up with
    /// [`Self::recv_event`] until [`Event::is_closed`].
    pub fn send_stream(&mut self, req: &ClientReq) -> Result<Ulid> {
        let (method, params) = wire::to_request(req);
        let envelope = build_request(
            method,
            params,
            Priority::Interactive,
            Some(CLIENT_DEADLINE_MS),
            None,
            None,
            None,
        );
        write_request(&mut self.writer, &envelope).context("write streaming request")?;
        self.writer.flush().context("flush request")?;
        // Consume the initial Response::Ok ack — it confirms the
        // daemon accepted the streaming method and the rest of the
        // connection now carries Event frames only.
        let ack = read_response(&mut self.reader).context("read stream ack")?;
        match ack {
            Response::Ok { request_id, .. } => Ok(request_id),
            Response::Err { error, .. } => Err(AgtError {
                code: exit::DAEMON_UNAVAILABLE,
                msg: format!("{}: {}", error.code, error.message),
            }
            .into()),
        }
    }

    /// Read one streaming event frame.
    pub fn recv_event(&mut self) -> Result<Event> {
        read_event(&mut self.reader).context("read event")
    }
}

fn decode_response(method: &str, resp: Response) -> Result<ClientReply> {
    match resp {
        Response::Ok { result, .. } => wire::result_to_reply(method, &result),
        Response::Err { error, .. } => Err(AgtError {
            code: error_exit_code(&error.message),
            msg: error.message,
        }
        .into()),
    }
}

/// Map a daemon error message back onto a stable CLI exit code. The
/// daemon emits `BadRequest`/`Internal` v1 codes; we recover the
/// session-vs-other distinction from the message because the v1
/// envelope doesn't carry a structured "not-found" category yet.
fn error_exit_code(message: &str) -> i32 {
    if message.contains("no such session") {
        exit::NO_SESSION
    } else {
        exit::DAEMON_UNAVAILABLE
    }
}

/// Stream events from `agt.tail` until the daemon's `closed` sentinel
/// or `on_event` returns false. Errors during decode propagate via
/// the closure's `false` return value plus a caller-set sentinel.
pub fn stream_events(
    client: &mut Client,
    mut on_event: impl FnMut(ClientReply) -> bool,
) -> Result<()> {
    loop {
        let event = match client.recv_event() {
            Ok(e) => e,
            Err(e) => {
                // EOF / closed socket — treat as a clean end of stream.
                if is_eof(&e) {
                    return Ok(());
                }
                return Err(e);
            }
        };
        if event.is_closed() {
            return Ok(());
        }
        if event.kind == "error" {
            let message = event
                .params
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("daemon error")
                .to_string();
            let code = event
                .params
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("Internal");
            let reply = ClientReply::Error {
                message,
                code: legacy_code_for(code),
            };
            if !on_event(reply) {
                return Ok(());
            }
            continue;
        }
        let daemon_event = match wire::stream_payload_to_event(&event.kind, event.params) {
            Ok(e) => e,
            Err(e) => return Err(anyhow!("decode event: {e}")),
        };
        if !on_event(ClientReply::Event {
            event: daemon_event,
        }) {
            return Ok(());
        }
    }
}

fn is_eof(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| matches!(e.downcast_ref::<std::io::Error>(), Some(io) if io.kind() == std::io::ErrorKind::UnexpectedEof))
}

fn legacy_code_for(code: &str) -> u16 {
    match code {
        "NotFound" => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_carries_method_and_priority() {
        let req = build_request(
            "agt.list",
            serde_json::json!({}),
            Priority::Interactive,
            Some(CLIENT_DEADLINE_MS),
            None,
            None,
            None,
        );
        assert_eq!(req.method, "agt.list");
        assert_eq!(req.priority, Priority::Interactive);
        assert_eq!(req.deadline_ms, Some(CLIENT_DEADLINE_MS));
    }

    #[test]
    fn error_message_pattern_maps_to_no_session() {
        assert_eq!(error_exit_code("no such session: abc"), exit::NO_SESSION);
        assert_eq!(
            error_exit_code("something else broke"),
            exit::DAEMON_UNAVAILABLE
        );
    }
}
