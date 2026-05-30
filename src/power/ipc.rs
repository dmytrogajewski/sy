//! `sy-powerd` IPC wire format — Unix socket, length-prefixed JSON
//! (Step 10).
//!
//! Step 10 ships the minimum surface Step 11's `sy power status --json`
//! consumes: one [`StatusRequest`] in, one [`StatusResponse`] back.
//! Wire shape is deliberately smaller than the workspace's v1
//! envelope (`sy-ipc`) — `sy power` doesn't share methods with the
//! aiplane / knowledge / agentd surfaces, so the extra envelope
//! ceremony would be cargo-culted noise. The frame is:
//!
//! ```text
//!   ┌────────────┬──────────────────┐
//!   │ u32 BE len │ JSON body (utf8) │
//!   └────────────┴──────────────────┘
//! ```
//!
//! Frames cap at [`MAX_FRAME_BYTES`] so a malformed peer can't pin
//! daemon memory. A short read / oversize length is a hard error —
//! the daemon drops the connection and logs the framing error.
//!
//! On-the-wire `op` and `schema` strings are pinned: Step 11's CLI
//! parses against them, and the SPEC §4 `sy.power.status/v1` schema
//! id is the single source of truth for downstream agents.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::power::drift::DriftStatus;
use crate::power::log::AuditEntry;
use crate::power::snapshot::Snapshot;

/// SPEC §4 schema id pinned on every [`StatusResponse`]. Matches the
/// constant in `cli::STATUS_SCHEMA` — bumping one without the other
/// is a breaking change to the documented `sy power status --json`
/// contract.
pub const STATUS_SCHEMA: &str = "sy.power.status/v1";

/// Largest accepted frame body. A snapshot serialises to <2 KiB
/// today; 64 KiB leaves a 30× headroom for future schema growth
/// without making the cap trivial to exhaust by accident.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Request frame. Tagged on `op` so future variants (`shield`,
/// `bandit`, etc.) can land without a wire bump.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "PascalCase")]
pub enum StatusRequest {
    /// Return the most recent snapshot + hash. Step 11's CLI handler
    /// dials this once per invocation.
    Status,
    /// Manual pin: force the daemon to apply the named arm on every
    /// tick until cleared. Step 19's `sy power profile <name>` CLI
    /// dials this; the daemon validates the name against the
    /// configured arm table and replies with [`ProfileAck`].
    ProfileSet { name: String },
    /// Clear any active manual pin. Step 19's `sy power profile --auto`
    /// CLI dials this; subsequent ticks fall back to the rules baseline.
    ProfileClear,
}

/// Reply to [`StatusRequest::ProfileSet`] / [`StatusRequest::ProfileClear`].
/// Carries the now-active pin (or `None` after `ProfileClear`) plus an
/// `ok` flag the CLI surfaces as exit 0 on success / non-zero on
/// "unknown arm name" (the daemon rejects the pin without mutating
/// state).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileAck {
    pub schema: String,
    pub ok: bool,
    pub pinned: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ProfileAck {
    /// Construct an OK ack carrying the new pin state.
    pub fn ok(pinned: Option<String>) -> Self {
        Self {
            schema: STATUS_SCHEMA.to_string(),
            ok: true,
            pinned,
            error: None,
        }
    }

    /// Construct a failed ack (unknown arm name, etc.); state on the
    /// daemon side is unchanged.
    pub fn rejected(msg: impl Into<String>) -> Self {
        Self {
            schema: STATUS_SCHEMA.to_string(),
            ok: false,
            pinned: None,
            error: Some(msg.into()),
        }
    }
}

/// Response frame for a [`StatusRequest::Status`] call. `snapshot` is
/// the daemon's most recent 1 Hz observation, serialised as JSON so
/// the wire decode stays decoupled from `Snapshot`'s `Serialize`-only
/// shape (the `schema: &'static str` field on `Snapshot` doesn't
/// roundtrip through `Deserialize`). Step 11's CLI parses the inner
/// payload via `serde_json::Value` against the documented SPEC §4
/// keys — exactly how an agent would consume the surface.
///
/// Step 19 adds the optional `last_audit` slot so `sy power status
/// --json`'s `applied_policy` field reflects the most recently
/// applied arm without re-tailing the audit log. The field is
/// `skip_serializing_if = Option::is_none` for backwards compatibility
/// with Step 10 wire snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    /// Always [`STATUS_SCHEMA`]. Owned `String` because `&'static`
    /// can't roundtrip through `Deserialize` — the stamping below
    /// keeps the value pinned without needing a custom deserializer.
    pub schema: String,
    pub snapshot_hash: String,
    pub snapshot: serde_json::Value,
    /// Most recent [`AuditEntry`] the daemon's apply loop produced.
    /// Populated by `handle_connection_full` from the cached
    /// `LatestAuditEntry`. Absent before the first tick lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_audit: Option<AuditEntry>,
    /// Step 31 drift block. Populated from the daemon's shared
    /// `LatestDriftStatus` slot. Defaults to "all-clear" — the
    /// pre-Step-31 wire shape with no `drift` key roundtrips into
    /// the default via `serde(default)`, keeping the IPC backward
    /// compatible.
    #[serde(default)]
    pub drift: DriftStatus,
    /// Step T3 (BUG-20260525-2352) model-health block. Populated by
    /// the daemon when the most recent retrain attempt aborted with
    /// per-class-coverage / per-class-recall gates so
    /// `sy power status --json | jq .model.missing_classes` surfaces
    /// the gap. `None` means "last retrain succeeded or hasn't fired".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelStatus>,
}

/// Step T3 model-health surface carried on [`StatusResponse::model`].
/// Currently a single field — the trainer's last-known missing-classes
/// list — but kept in its own struct so future health signals
/// (training wall-time, validation accuracy, version-sha) can be added
/// without re-bumping the wire schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelStatus {
    /// Names of activity classes that fell below the trainer's
    /// per-class row floor on the last retrain attempt. Empty when the
    /// last attempt failed for a different reason; absent (`None` on
    /// [`StatusResponse::model`]) when no retrain has ever fired.
    #[serde(default)]
    pub missing_classes: Vec<String>,
}

impl StatusResponse {
    /// Construct a response from the latest snapshot. The schema id
    /// is stamped here so callers can't accidentally desync it.
    /// `last_audit` is `None` until the daemon overrides it from the
    /// cached `LatestAuditEntry`.
    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        let snapshot_hash = snapshot.snapshot_hash.clone();
        let value = serde_json::to_value(&snapshot).unwrap_or(serde_json::Value::Null);
        Self {
            schema: STATUS_SCHEMA.to_string(),
            snapshot_hash,
            snapshot: value,
            last_audit: None,
            drift: DriftStatus::default(),
            model: None,
        }
    }
}

/// Encode a serialisable frame as `u32-BE length || JSON body`.
/// Returns `Err` only on `serde_json` failure (in practice
/// unreachable — our types are fixed-shape).
pub fn encode_frame<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("encode: {e}")))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame body {} > {MAX_FRAME_BYTES}", body.len()),
        ));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Read one frame from a tokio `AsyncRead`, decode as `T`. Returns
/// `UnexpectedEof` on partial reads — callers treat that as "peer
/// closed", not a protocol error.
pub async fn read_frame<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncReadExt + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame len {len} > {MAX_FRAME_BYTES}"),
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("decode: {e}")))
}

/// Write one frame to a tokio `AsyncWrite`. Wraps [`encode_frame`]
/// so callers don't double-buffer.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let buf = encode_frame(value)?;
    writer.write_all(&buf).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::snapshot::{SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
    use chrono::TimeZone;
    use tokio::net::{UnixListener, UnixStream};

    fn pinned_snapshot() -> Snapshot {
        Snapshot {
            schema: SCHEMA_ID,
            ts: chrono::Utc
                .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
                .single()
                .expect("pinned UTC instant"),
            features: [0.0_f32; FEATURE_LEN],
            raw: SnapshotRaw::default(),
            snapshot_hash: "deadbeef".repeat(8),
        }
    }

    /// Step 10 DoD test: `StatusRequest::Status` written by a client
    /// is decoded server-side, and the server's `StatusResponse`
    /// roundtrips back to the client over a real Unix socket. Both
    /// halves use [`read_frame`] / [`write_frame`] — the wire
    /// contract Step 11's CLI will dial against.
    #[tokio::test]
    async fn status_round_trips_over_unix_socket() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp.path().join("powerd.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let snapshot = pinned_snapshot();
        let expected_hash = snapshot.snapshot_hash.clone();

        // Server task: accept one connection, decode the request,
        // reply with the pinned snapshot.
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let req: StatusRequest = read_frame(&mut stream).await.expect("read request");
            assert_eq!(req, StatusRequest::Status);
            let resp = StatusResponse::from_snapshot(snapshot);
            write_frame(&mut stream, &resp).await.expect("write resp");
        });

        let mut client = UnixStream::connect(&sock).await.expect("connect");
        write_frame(&mut client, &StatusRequest::Status)
            .await
            .expect("send request");
        let resp: StatusResponse = read_frame(&mut client).await.expect("read response");

        assert_eq!(resp.schema, STATUS_SCHEMA);
        assert_eq!(resp.snapshot_hash, expected_hash);
        assert_eq!(
            resp.snapshot["snapshot_hash"].as_str(),
            Some(expected_hash.as_str()),
            "wire snapshot must carry the same hash as the response envelope",
        );
        server.await.expect("server task");
    }

    /// Wire-format sanity: a length-prefixed frame decodes back to
    /// the same value through a fresh byte buffer. Catches any
    /// regression in the `u32-BE || body` layout without touching a
    /// socket.
    #[test]
    fn encode_decode_round_trip_in_memory() {
        let req = StatusRequest::Status;
        let buf = encode_frame(&req).expect("encode");
        // First 4 bytes are big-endian length.
        let len = u32::from_be_bytes(buf[..4].try_into().expect("4-byte prefix")) as usize;
        assert_eq!(len, buf.len() - 4);
        let decoded: StatusRequest =
            serde_json::from_slice(&buf[4..]).expect("decode body as request");
        assert_eq!(decoded, req);
    }

    /// Step 19: the manual-pin ops round-trip through serde with the
    /// same `op`-tagged shape as `Status`. The wire form is what `sy
    /// power profile <name>` dials and what the daemon decodes — drift
    /// here breaks the CLI ↔ daemon contract.
    #[test]
    fn profile_ops_round_trip_through_serde() {
        let set = StatusRequest::ProfileSet {
            name: "build".to_string(),
        };
        let json = serde_json::to_string(&set).expect("encode set");
        assert!(
            json.contains("\"op\":\"ProfileSet\""),
            "ProfileSet must serialize op tag: {json}"
        );
        assert!(
            json.contains("\"name\":\"build\""),
            "ProfileSet must carry name: {json}"
        );
        let back: StatusRequest = serde_json::from_str(&json).expect("decode set");
        assert_eq!(back, set);

        let clear = StatusRequest::ProfileClear;
        let json = serde_json::to_string(&clear).expect("encode clear");
        let back: StatusRequest = serde_json::from_str(&json).expect("decode clear");
        assert_eq!(back, clear);

        let ack = ProfileAck::ok(Some("build".to_string()));
        let line = serde_json::to_string(&ack).expect("encode ack");
        let back: ProfileAck = serde_json::from_str(&line).expect("decode ack");
        assert_eq!(back, ack);
    }
}
