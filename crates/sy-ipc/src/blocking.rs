//! Synchronous wire helpers used by sync clients (CLI/MCP code that
//! doesn't already host a tokio runtime). The framing matches the
//! async codecs in [`crate::codec`] byte-for-byte: 4-byte big-endian
//! length prefix, JSON payload.
//!
//! Async daemons + reqwest-style callers go through [`crate::Client`]
//! / [`crate::Server`]; these helpers exist so a short-lived CLI
//! invocation doesn't need to spin up a runtime just to send one
//! frame.

use std::io::{self, Read, Write};

use ulid::Ulid;

use crate::envelope::{Request, Response, SpanId, TraceId, SCHEMA_VERSION};
use crate::stream::Event;
use sy_core::Priority;

/// Build a v1 [`Request`] with sensible defaults filled in. Callers
/// override `priority`/`deadline_ms`/`request_id` after construction
/// when they need something other than `Interactive` / `5000 ms` /
/// fresh ULID — the foreground CLI/MCP case is exactly the default.
pub fn build_request(
    method: &str,
    params: serde_json::Value,
    priority: Priority,
    deadline_ms: Option<u64>,
    request_id: Option<Ulid>,
    trace_id: Option<TraceId>,
    parent_span_id: Option<SpanId>,
) -> Request {
    // `Ulid::default()` is the zero-ULID, not a fresh one, so the
    // unwrap-or-default lint's suggestion is semantically wrong here.
    // Spell the `None` arm explicitly to keep the fresh-id semantics.
    let request_id = match request_id {
        Some(id) => id,
        None => Ulid::new(),
    };
    Request {
        schema_version: SCHEMA_VERSION,
        request_id,
        trace_id,
        parent_span_id,
        deadline_ms,
        priority,
        method: method.into(),
        params,
    }
}

/// Write a single length-prefixed frame.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len =
        u32::try_from(payload.len()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read a single length-prefixed frame, returning the payload bytes.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(payload)
}

/// Encode a [`Request`] as JSON and write it as one v1 frame.
pub fn write_request<W: Write>(w: &mut W, req: &Request) -> io::Result<()> {
    let bytes = serde_json::to_vec(req)?;
    write_frame(w, &bytes)
}

/// Read one [`Response`] frame.
pub fn read_response<R: Read>(r: &mut R) -> io::Result<Response> {
    let payload = read_frame(r)?;
    serde_json::from_slice(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Read one [`Event`] frame.
pub fn read_event<R: Read>(r: &mut R) -> io::Result<Event> {
    let payload = read_frame(r)?;
    serde_json::from_slice(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Encode an [`Event`] as JSON and write it as one v1 frame.
pub fn write_event<W: Write>(w: &mut W, evt: &Event) -> io::Result<()> {
    let bytes = serde_json::to_vec(evt)?;
    write_frame(w, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_response_round_trip_through_blocking_helpers() {
        // Sync framing must produce the same bytes as the async codec
        // (matched payload, matched 4-byte BE length prefix). Round-
        // tripping through a `Vec<u8>` proves the two halves agree.
        let req = build_request(
            "system.health",
            serde_json::json!({}),
            Priority::Interactive,
            Some(5000),
            None,
            None,
            None,
        );
        let mut wire = Vec::new();
        write_request(&mut wire, &req).expect("write request");
        let mut cur = Cursor::new(&wire);
        let back: Request = serde_json::from_slice(&read_frame(&mut cur).expect("read frame"))
            .expect("decode request");
        assert_eq!(req, back);
    }

    #[test]
    fn event_round_trip_through_blocking_helpers() {
        let evt = Event::closed(Ulid::new());
        let mut wire = Vec::new();
        write_event(&mut wire, &evt).expect("write event");
        let mut cur = Cursor::new(&wire);
        let back = read_event(&mut cur).expect("read event");
        assert_eq!(evt, back);
    }
}
