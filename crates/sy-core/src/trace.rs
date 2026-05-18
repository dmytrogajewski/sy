//! W3C-`traceparent`-compatible trace context primitives. SPEC §4.6
//! places `trace_id` / `span_id` in the observability vocabulary, not
//! the IPC envelope's — `sy-ipc` re-exports the types from here for
//! backward-compatibility with arch-ipc-v1 Step 1 callsites.
//!
//! Wire format is an opaque hex string: 32 lowercase hex digits for
//! `TraceId` (16 bytes), 16 lowercase hex digits for `SpanId` (8
//! bytes). The envelope layer treats them as strings — validation of
//! the hex shape stays here so future tightening doesn't touch the
//! wire contract.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// W3C `traceparent`-compatible 16-byte hex identifier. Serialises as
/// a 32-character lowercase hex string via `#[serde(transparent)]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TraceId(pub String);

/// W3C `traceparent`-compatible 8-byte hex parent span identifier.
/// Serialises as a 16-character lowercase hex string via
/// `#[serde(transparent)]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SpanId(pub String);

impl TraceId {
    /// Generate a fresh `TraceId` from a ULID's 128-bit body. ULIDs
    /// embed a millisecond timestamp in their top 48 bits so two
    /// ids minted in sequence sort naturally; the remaining 80 bits
    /// are CSPRNG-derived inside `ulid::Ulid::new()`. Hex-encoding
    /// the full 16 bytes yields a W3C-`traceparent`-compatible id
    /// without pulling in a `rand` dep.
    pub fn new() -> Self {
        Self(format!("{:032x}", Ulid::new().0))
    }

    /// Borrow the underlying hex string. Useful for stamping the id
    /// into a `tracing` field without an allocation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl SpanId {
    /// Generate a fresh `SpanId` from the low 64 bits of a ULID. The
    /// random portion of a ULID lives in its low 80 bits; truncating
    /// to 64 bits matches the W3C `traceparent` span-id width and
    /// preserves enough entropy (2^64) to keep collisions astronomical
    /// for a single host's IPC traffic.
    pub fn new() -> Self {
        let bits = Ulid::new().0 as u64;
        Self(format!("{bits:016x}"))
    }

    /// Borrow the underlying hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SpanId {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hex width for the W3C `traceparent` `trace-id` field.
    const TRACE_ID_HEX_LEN: usize = 32;

    /// Hex width for the W3C `traceparent` `parent-id` field.
    const SPAN_ID_HEX_LEN: usize = 16;

    #[test]
    fn trace_id_new_is_32_hex_chars() {
        let t = TraceId::new();
        assert_eq!(t.as_str().len(), TRACE_ID_HEX_LEN);
        assert!(t.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn span_id_new_is_16_hex_chars() {
        let s = SpanId::new();
        assert_eq!(s.as_str().len(), SPAN_ID_HEX_LEN);
        assert!(s.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn trace_id_serialises_transparently() {
        // SPEC §4.2 wire shape: the envelope serialises a `TraceId`
        // as a bare JSON string, not a tagged object. Breaking this
        // would invalidate every `arch-ipc-v1` Step 1 round-trip.
        let t = TraceId("0af7651916cd43dd8448eb211c80319c".into());
        let json = serde_json::to_string(&t).expect("serialise");
        assert_eq!(json, "\"0af7651916cd43dd8448eb211c80319c\"");
    }

    #[test]
    fn span_id_serialises_transparently() {
        let s = SpanId("b7ad6b7169203331".into());
        let json = serde_json::to_string(&s).expect("serialise");
        assert_eq!(json, "\"b7ad6b7169203331\"");
    }
}
