//! LSP-style framed transport for the `sy file` plugin runtime.
//!
//! Implements the `Content-Length`-framed JSON-RPC 2.0 wire format from
//! [plugin SPEC
//! §4.2.1](../../../specs/research/sy-file-manager-plugins/SPEC.md#421-framing)
//! as a [`tokio_util::codec::Decoder`] + [`tokio_util::codec::Encoder`]
//! pair, so the host can sit on either side of a
//! `tokio_util::codec::Framed<_, JsonRpcCodec>` against any
//! `AsyncRead`/`AsyncWrite`.
//!
//! Frame layout on the wire:
//!
//! ```text
//! Content-Length: <n>\r\n
//! Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n   (optional)
//! \r\n
//! <n bytes of UTF-8 JSON body>
//! ```
//!
//! The codec carries `serde_json::Value` so the SPEC §4.2.2 request /
//! response / notification shapes can be encoded uniformly; the typed
//! [`crate::plugin::rpc`] wrappers are what callers usually construct
//! before encoding.
use std::io;

use bytes::{Buf, BufMut, BytesMut};
use serde_json::Value;
use tokio_util::codec::{Decoder, Encoder};

/// Maximum permitted frame payload size in bytes (16 MiB).
///
/// Hardens the host against a runaway plugin shoving a multi-gigabyte
/// payload through stdout and OOM-ing the daemon. 16 MiB is the SPEC
/// §4.2.1 ceiling — large enough to hold a >2 MiB base64-PNG response
/// (journey beat J3) with headroom for thumbnails / OCR snippets while
/// still bounded.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// `Content-Length:` header literal (LSP framing).
const CONTENT_LENGTH_HEADER: &str = "Content-Length:";

/// Double CRLF marking the header / body boundary (SPEC §4.2.1).
const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

/// JSON-RPC codec over LSP framing.
///
/// `Message = serde_json::Value` so callers can encode any of the SPEC
/// §4.2.2 shapes (request / response / notification) through the same
/// codec; the typed wrappers in [`crate::plugin::rpc`] all serialise to
/// `Value` first.
#[derive(Debug, Default)]
pub struct JsonRpcCodec {
    /// Once we've parsed the `Content-Length` header for the current
    /// frame we cache it here so a partial-body read can resume on the
    /// next `decode` call without re-scanning the headers.
    expected_body_len: Option<usize>,
}

impl Encoder<Value> for JsonRpcCodec {
    type Error = io::Error;

    fn encode(&mut self, item: Value, dst: &mut BytesMut) -> io::Result<()> {
        let body =
            serde_json::to_vec(&item).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if body.len() > MAX_PAYLOAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame::too_large encoded body {} bytes exceeds {} MiB cap",
                    body.len(),
                    MAX_PAYLOAD_BYTES / (1024 * 1024)
                ),
            ));
        }
        let header = format!("{} {}\r\n\r\n", CONTENT_LENGTH_HEADER, body.len());
        dst.reserve(header.len() + body.len());
        dst.put_slice(header.as_bytes());
        dst.put_slice(&body);
        Ok(())
    }
}

impl Decoder for JsonRpcCodec {
    type Item = Value;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Value>> {
        if self.expected_body_len.is_none() {
            let Some(header_end) = find_subslice(src, HEADER_TERMINATOR) else {
                return Ok(None);
            };
            let header_bytes = &src[..header_end];
            let header_str = std::str::from_utf8(header_bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame::bad_header non-utf8: {e}"),
                )
            })?;
            let body_len = parse_content_length(header_str)?;
            if body_len > MAX_PAYLOAD_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "frame::too_large advertised body {} bytes exceeds {} MiB cap",
                        body_len,
                        MAX_PAYLOAD_BYTES / (1024 * 1024)
                    ),
                ));
            }
            src.advance(header_end + HEADER_TERMINATOR.len());
            self.expected_body_len = Some(body_len);
        }
        let body_len = self.expected_body_len.expect("body len cached above");
        if src.len() < body_len {
            return Ok(None);
        }
        let body = src.split_to(body_len);
        self.expected_body_len = None;
        let value = serde_json::from_slice::<Value>(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(value))
    }
}

/// Linear search for the first occurrence of `needle` inside `haystack`.
///
/// LSP frame headers are tiny (< 200 bytes in practice) so a naive scan
/// is faster than pulling in `memchr`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parse the `Content-Length: <n>` header out of an LSP header block.
///
/// `Content-Type` (and any other LSP-permitted header) is ignored —
/// only `Content-Length` is mandatory per SPEC §4.2.1.
fn parse_content_length(header_block: &str) -> io::Result<usize> {
    for line in header_block.split("\r\n") {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            return value.trim().parse::<usize>().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame::bad_content_length {e}"),
                )
            });
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "frame::missing_content_length",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Round-trip a canonical `preview` request through encode + decode
    /// and assert the decoded `Value` matches the input byte-for-byte
    /// after serialisation — the wire contract every later journey beat
    /// rides on (SPEC §4.2.2 request shape).
    #[test]
    fn encode_then_decode_roundtrip() {
        let mut codec = JsonRpcCodec::default();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "preview",
            "params": { "path": "README.md", "mime": "text/markdown" }
        });
        let mut buf = BytesMut::new();
        codec.encode(req.clone(), &mut buf).expect("encode");
        let decoded = codec
            .decode(&mut buf)
            .expect("decode")
            .expect("frame ready");
        assert_eq!(decoded, req);
        assert!(buf.is_empty(), "no trailing bytes after one full frame");
    }

    /// Feed the framed bytes one byte at a time. `decode` must return
    /// `Ok(None)` for every partial state and only the final byte
    /// completes the frame. Stress-tests that the cached
    /// `expected_body_len` survives partial reads.
    #[test]
    fn decode_streamed_partial_frame() {
        let mut writer = JsonRpcCodec::default();
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping", "params": null });
        let mut full = BytesMut::new();
        writer.encode(req.clone(), &mut full).expect("encode");

        let bytes = full.to_vec();
        let mut reader = JsonRpcCodec::default();
        let mut staged = BytesMut::new();
        for (i, b) in bytes.iter().enumerate() {
            staged.extend_from_slice(&[*b]);
            let decoded = reader.decode(&mut staged).expect("decode partial");
            if i + 1 < bytes.len() {
                assert!(decoded.is_none(), "partial frame must not decode early");
            } else {
                assert_eq!(decoded, Some(req.clone()));
            }
        }
    }

    /// A frame with headers but no `Content-Length` line must be
    /// rejected as `InvalidData` — bare `Content-Type` headers are not
    /// enough to know where the body ends.
    #[test]
    fn decode_rejects_missing_content_length() {
        let mut codec = JsonRpcCodec::default();
        let mut buf = BytesMut::new();
        buf.put_slice(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n");
        let err = codec.decode(&mut buf).expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// An optional `Content-Type` header preceding `Content-Length`
    /// must be accepted (SPEC §4.2.1 declares `Content-Type` optional).
    #[test]
    fn decode_handles_optional_content_type() {
        let mut codec = JsonRpcCodec::default();
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":null}"#;
        let mut buf = BytesMut::new();
        buf.put_slice(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n");
        buf.put_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        buf.put_slice(body);
        let decoded = codec
            .decode(&mut buf)
            .expect("decode")
            .expect("frame ready");
        assert_eq!(decoded["method"], "ping");
    }

    /// Bodies that legitimately contain `\r\n` inside JSON string
    /// values (base64 PNG payloads with embedded newlines, prose text
    /// with line breaks) must encode + decode byte-identical. This is
    /// the journey-J3 preview-response shape.
    #[test]
    fn encode_payload_with_newlines_in_string() {
        let mut codec = JsonRpcCodec::default();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "image": { "png_base64": "AAA\r\nBBB\r\nCCC", "w": 1, "h": 1 } }
        });
        let mut buf = BytesMut::new();
        codec.encode(req.clone(), &mut buf).expect("encode");
        let decoded = codec
            .decode(&mut buf)
            .expect("decode")
            .expect("frame ready");
        assert_eq!(decoded, req);
    }

    /// Advertising a frame > 16 MiB must be rejected with
    /// `frame::too_large` (SPEC §4.2.2 `-32094 FRAME_TOO_LARGE` is the
    /// peer-facing error code; the codec itself surfaces this as
    /// `io::ErrorKind::InvalidData` carrying the marker string the rpc
    /// layer maps to that JSON-RPC error code).
    #[test]
    fn max_payload_16_mib_enforced() {
        let mut codec = JsonRpcCodec::default();
        let oversize = MAX_PAYLOAD_BYTES + 1;
        let mut buf = BytesMut::new();
        buf.put_slice(format!("Content-Length: {oversize}\r\n\r\n").as_bytes());
        let err = codec.decode(&mut buf).expect_err("oversize rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("frame::too_large"),
            "error must carry frame::too_large marker, got: {err}"
        );
    }
}
