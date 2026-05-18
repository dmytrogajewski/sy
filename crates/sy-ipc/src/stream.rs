//! Streaming response envelope (SPEC §7 Q6 / ROADMAP arch-ipc-v1
//! Step 6). A streaming method's daemon first writes a standard
//! `Response::Ok` to ack the call, then writes a sequence of
//! [`Event`] frames terminated by the sentinel [`Event::closed`].
//! Client reads events until it sees that sentinel.
//!
//! The wire framing is identical to [`crate::codec`]: 4-byte BE
//! length-prefixed JSON. Events ride on the same connection as the
//! original request — no separate socket, no SCM_RIGHTS.

use std::io;

use bytes::BytesMut;
use serde::{Deserialize, Serialize};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};
use ulid::Ulid;

use crate::envelope::SCHEMA_VERSION;

/// Sentinel `kind` written as the final event in any stream. Clients
/// stop reading once they receive an event whose `kind` matches.
pub const KIND_CLOSED: &str = "closed";

/// Streaming-response envelope. Correlates back to the originating
/// `Request.request_id` so a client multiplexing several long-lived
/// streams can route events to the right consumer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub request_id: Ulid,
    /// Discriminator under which the daemon serialised the payload.
    /// Each method documents its own kinds (e.g. `agt.tail` emits
    /// `transcript`, `status`, `permission`, `closed`).
    pub kind: String,
    /// Free-form params for the event. Empty object for sentinels.
    pub params: serde_json::Value,
}

impl Event {
    /// Closing sentinel: clients consume until they see one of these
    /// and then close the stream half of the connection.
    pub fn closed(request_id: Ulid) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            request_id,
            kind: KIND_CLOSED.into(),
            params: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// True for the closing sentinel — convenience for read loops.
    pub fn is_closed(&self) -> bool {
        self.kind == KIND_CLOSED
    }
}

/// 4-byte big-endian length-prefixed JSON codec for [`Event`] frames.
/// Mirrors [`crate::codec::ResponseCodec`] so events flow over the
/// same connection without a framing renegotiation.
#[derive(Debug, Default)]
pub struct EventCodec {
    inner: LdJsonCodec,
}

#[derive(Debug)]
struct LdJsonCodec(LengthDelimitedCodec);

impl Default for LdJsonCodec {
    fn default() -> Self {
        Self(
            LengthDelimitedCodec::builder()
                .length_field_length(4)
                .big_endian()
                .new_codec(),
        )
    }
}

impl Encoder<Event> for EventCodec {
    type Error = io::Error;
    fn encode(&mut self, item: Event, dst: &mut BytesMut) -> io::Result<()> {
        let bytes =
            serde_json::to_vec(&item).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.inner.0.encode(bytes.into(), dst)
    }
}

impl Decoder for EventCodec {
    type Item = Event;
    type Error = io::Error;
    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Event>> {
        let Some(frame) = self.inner.0.decode(src)? else {
            return Ok(None);
        };
        let item = serde_json::from_slice::<Event>(&frame)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(item))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ULID: &str = "01HXYZ0000000000000000000Z";

    #[test]
    fn event_round_trip_via_codec() {
        let ulid = Ulid::from_string(SAMPLE_ULID).expect("ulid");
        let evt = Event {
            schema_version: SCHEMA_VERSION,
            request_id: ulid,
            kind: "transcript".into(),
            params: serde_json::json!({"text": "hi"}),
        };
        let mut codec = EventCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(evt.clone(), &mut buf).expect("encode");
        let decoded = codec
            .decode(&mut buf)
            .expect("decode")
            .expect("frame ready");
        assert_eq!(decoded, evt);
        assert!(buf.is_empty());
    }

    #[test]
    fn closed_sentinel_is_recognised() {
        // SPEC §4.2 / ROADMAP Step 6 streaming contract: clients stop
        // reading the moment they see `kind = "closed"`. The sentinel
        // helper must produce that exact shape — flipping it would
        // strand readers in `recv()` forever.
        let ulid = Ulid::from_string(SAMPLE_ULID).expect("ulid");
        let sentinel = Event::closed(ulid);
        assert!(sentinel.is_closed());
        assert_eq!(sentinel.kind, KIND_CLOSED);
        assert_eq!(sentinel.request_id, ulid);
        assert_eq!(sentinel.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn event_frame_uses_4_byte_big_endian_length() {
        // Lock the wire shape down at the byte level — the streaming
        // codec must match `ResponseCodec` exactly so clients can
        // share a single `LengthDelimitedCodec`-shaped reader for the
        // initial `Response::Ok` and the subsequent `Event` frames.
        let ulid = Ulid::from_string(SAMPLE_ULID).expect("ulid");
        let evt = Event::closed(ulid);
        let mut codec = EventCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(evt, &mut buf).expect("encode");
        assert!(buf.len() > 4, "frame must include header + payload");
        let header = [buf[0], buf[1], buf[2], buf[3]];
        let payload_len = u32::from_be_bytes(header) as usize;
        assert_eq!(
            payload_len,
            buf.len() - 4,
            "header must encode payload length"
        );
    }
}
