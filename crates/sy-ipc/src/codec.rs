//! 4-byte big-endian length-prefixed framing for IPC v1 (SPEC §4.2
//! "Framing"). Thin wrappers over `tokio_util::codec::LengthDelimitedCodec`
//! that bolt on `serde_json` encode/decode for `Request` and `Response`.
//!
//! `Framed`-based async I/O lands in Step 2 once the `tokio` runtime
//! is wired in; this step verifies the codec contract directly via
//! `Encoder::encode` + `Decoder::decode` on a shared `BytesMut`, which
//! is the same byte-level guarantee `Framed` relies on.

use std::io;

use bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

use crate::envelope::{Request, Response};

/// Length-delimited JSON codec for IPC v1 `Request` frames.
///
/// Bytes on the wire: 4-byte big-endian length, followed by JSON.
/// Same shape as `ResponseCodec`; kept as separate types so the
/// `Framed<UnixStream, RequestCodec>` (Step 2 server side) is
/// typed-distinct from `Framed<UnixStream, ResponseCodec>` (client).
#[derive(Debug, Default)]
pub struct RequestCodec {
    inner: LdJsonCodec,
}

/// Length-delimited JSON codec for IPC v1 `Response` frames.
#[derive(Debug, Default)]
pub struct ResponseCodec {
    inner: LdJsonCodec,
}

#[derive(Debug)]
struct LdJsonCodec(LengthDelimitedCodec);

impl Default for LdJsonCodec {
    fn default() -> Self {
        // Explicit length-field config; `LengthDelimitedCodec`'s
        // defaults happen to match (4-byte BE), but pinning them
        // here guards against an upstream default change quietly
        // breaking the wire shape.
        Self(
            LengthDelimitedCodec::builder()
                .length_field_length(4)
                .big_endian()
                .new_codec(),
        )
    }
}

fn encode_json<T: serde::Serialize>(
    codec: &mut LdJsonCodec,
    item: &T,
    dst: &mut BytesMut,
) -> io::Result<()> {
    let bytes =
        serde_json::to_vec(item).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    codec.0.encode(bytes.into(), dst)
}

fn decode_json<T: serde::de::DeserializeOwned>(
    codec: &mut LdJsonCodec,
    src: &mut BytesMut,
) -> io::Result<Option<T>> {
    let Some(frame) = codec.0.decode(src)? else {
        return Ok(None);
    };
    let item = serde_json::from_slice::<T>(&frame)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(item))
}

impl Encoder<Request> for RequestCodec {
    type Error = io::Error;
    fn encode(&mut self, item: Request, dst: &mut BytesMut) -> io::Result<()> {
        encode_json(&mut self.inner, &item, dst)
    }
}

impl Decoder for RequestCodec {
    type Item = Request;
    type Error = io::Error;
    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Request>> {
        decode_json(&mut self.inner, src)
    }
}

impl Encoder<Response> for ResponseCodec {
    type Error = io::Error;
    fn encode(&mut self, item: Response, dst: &mut BytesMut) -> io::Result<()> {
        encode_json(&mut self.inner, &item, dst)
    }
}

impl Decoder for ResponseCodec {
    type Item = Response;
    type Error = io::Error;
    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Response>> {
        decode_json(&mut self.inner, src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{ErrorBody, Request, Response, SCHEMA_VERSION};
    use sy_core::{ErrorCode, Priority};
    use ulid::Ulid;

    const SAMPLE_ULID: &str = "01HXYZ0000000000000000000Z";

    fn sample_request() -> Request {
        Request {
            schema_version: SCHEMA_VERSION,
            request_id: Ulid::from_string(SAMPLE_ULID).expect("ulid"),
            trace_id: None,
            parent_span_id: None,
            deadline_ms: Some(5000),
            priority: Priority::Interactive,
            method: "aiplane.run".into(),
            params: serde_json::json!({"workload": "embed", "input": "hi"}),
        }
    }

    #[test]
    fn frame_round_trip_via_codec() {
        // Encode → decode the canonical Request through `RequestCodec`,
        // then the same for a Response — proves the 4-byte BE length
        // header and JSON payload survive the codec layer.
        let mut req_codec = RequestCodec::default();
        let req = sample_request();
        let mut buf = BytesMut::new();
        req_codec.encode(req.clone(), &mut buf).expect("encode req");
        let decoded = req_codec
            .decode(&mut buf)
            .expect("decode req")
            .expect("frame ready");
        assert_eq!(req, decoded);
        assert!(buf.is_empty(), "no trailing bytes after full frame");

        let mut resp_codec = ResponseCodec::default();
        let resp = Response::Err {
            schema_version: SCHEMA_VERSION,
            request_id: Ulid::from_string(SAMPLE_ULID).expect("ulid"),
            error: ErrorBody {
                code: ErrorCode::Overloaded,
                message: "queue full".into(),
                retry_after_ms: Some(200),
                details: serde_json::json!({}),
            },
        };
        resp_codec
            .encode(resp.clone(), &mut buf)
            .expect("encode resp");
        let decoded = resp_codec
            .decode(&mut buf)
            .expect("decode resp")
            .expect("frame ready");
        assert_eq!(resp, decoded);
    }

    #[test]
    fn partial_frame_returns_none_then_completes() {
        // SPEC §4.2 framing is length-prefixed: a half-arrived frame
        // must not eagerly mis-parse — the decoder returns `Ok(None)`
        // until enough bytes have landed.
        let mut codec = RequestCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(sample_request(), &mut buf)
            .expect("encode full frame");

        // Drain the first byte (length prefix), feed the remainder
        // in two halves; the decoder should report `None` until the
        // tail lands.
        let full = buf.split_to(buf.len());
        let mut staged = BytesMut::new();
        staged.extend_from_slice(&full[..1]);
        assert!(codec.decode(&mut staged).expect("decode partial").is_none());
        staged.extend_from_slice(&full[1..full.len() - 1]);
        assert!(codec.decode(&mut staged).expect("decode partial").is_none());
        staged.extend_from_slice(&full[full.len() - 1..]);
        let decoded = codec
            .decode(&mut staged)
            .expect("decode complete")
            .expect("frame ready");
        assert_eq!(decoded, sample_request());
    }

    #[test]
    fn wire_header_is_4_byte_big_endian_length() {
        // SPEC §4.2: 4-byte big-endian length prefix. Lock this in as
        // a byte-level assertion so a future swap to little-endian or
        // a 2-byte prefix breaks the test, not just the consumers.
        let mut codec = RequestCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(sample_request(), &mut buf)
            .expect("encode frame");
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
