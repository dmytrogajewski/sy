//! IPC v1 client scaffold (SPEC §4.2). Connects to a UDS socket,
//! sends a `Request` framed by `RequestCodec`, awaits the matching
//! `Response` framed by `ResponseCodec`.
//!
//! `Client::call` defaults (`Priority::Interactive`, 5000 ms deadline,
//! auto-generated `Ulid` request_id) implement the SPEC §5 friction-
//! map promise: "every IPC call site has to be touched" is satisfied
//! with one line because the defaults already match what 90 % of
//! callers want.

use std::io;
use std::path::Path;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{
    unix::{OwnedReadHalf, OwnedWriteHalf},
    UnixStream,
};
use tokio_util::codec::{FramedRead, FramedWrite};
use ulid::Ulid;

use crate::codec::{RequestCodec, ResponseCodec};
use crate::envelope::{Request, Response, SpanId, TraceId, SCHEMA_VERSION};
use crate::stream::EventCodec;
use sy_core::Priority;

/// Per-call envelope overrides. Defaults cover the foreground CLI /
/// MCP case (Interactive, 5 s deadline, fresh ULID, no trace/span).
/// Callers that need a non-default class only set the fields that
/// differ from `CallOpts::default()`.
#[derive(Debug, Clone)]
pub struct CallOpts {
    pub priority: Priority,
    pub deadline_ms: Option<u64>,
    pub trace_id: Option<TraceId>,
    pub parent_span_id: Option<SpanId>,
    /// Explicit request id. `None` ⇒ generate a fresh `Ulid` per
    /// call. Setting this lets callers correlate a `system.cancel`
    /// or a long-running streaming response without parsing the
    /// outgoing JSON.
    pub request_id: Option<Ulid>,
}

const DEFAULT_DEADLINE_MS: u64 = 5000;

impl Default for CallOpts {
    fn default() -> Self {
        Self {
            priority: Priority::Interactive,
            deadline_ms: Some(DEFAULT_DEADLINE_MS),
            trace_id: None,
            parent_span_id: None,
            request_id: None,
        }
    }
}

/// IPC v1 unary client. One in-flight call at a time — multiplexing
/// is a Zone 3 concern (the scheduler) and a v2 capability if it
/// ever lands.
pub struct Client {
    req_sink: FramedWrite<OwnedWriteHalf, RequestCodec>,
    resp_stream: FramedRead<OwnedReadHalf, ResponseCodec>,
}

impl Client {
    pub async fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            req_sink: FramedWrite::new(writer, RequestCodec::default()),
            resp_stream: FramedRead::new(reader, ResponseCodec::default()),
        })
    }

    pub async fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
        opts: CallOpts,
    ) -> io::Result<Response> {
        let req = build_request(method, params, opts);
        // arch-observability Step 4: stamp the outgoing `trace_id`
        // onto the local span context too, so client-side logs
        // emitted between `send` and `next` carry the same id the
        // daemon will eventually log. `build_request` guarantees
        // `trace_id = Some(_)` (auto-generated when unset), so the
        // `unwrap_or_else` only runs if a future caller bypasses
        // the helper.
        let trace_id = req.trace_id.clone().unwrap_or_default();
        let parent = req.parent_span_id.clone();
        let req_sink = &mut self.req_sink;
        let resp_stream = &mut self.resp_stream;
        sy_core::obs::with_trace_id_async(trace_id, parent, async move {
            req_sink.send(req).await?;
            match resp_stream.next().await {
                Some(Ok(resp)) => Ok(resp),
                Some(Err(e)) => Err(e),
                None => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed connection before responding",
                )),
            }
        })
        .await
    }

    /// Convert this client into a streaming event reader. Used by
    /// callers that opened a streaming method (e.g. `agt.tail`): once
    /// the initial `Response::Ok` ack has been observed via
    /// [`Self::call`], the same connection switches to `Event` frames
    /// terminated by the [`crate::stream::Event::closed`] sentinel.
    /// Consumes the client so subsequent unary calls on this connection
    /// are not possible — open a new client for further calls.
    ///
    /// Carries over the unread bytes still buffered in the response
    /// `FramedRead` so the first event frame isn't lost if it arrived
    /// piggy-backed onto the ack frame on the same syscall.
    pub fn into_event_stream(self) -> FramedRead<OwnedReadHalf, EventCodec> {
        let carry = self.resp_stream.read_buffer().clone();
        let reader = self.resp_stream.into_inner();
        let mut framed = FramedRead::new(reader, EventCodec::default());
        framed.read_buffer_mut().extend_from_slice(&carry);
        framed
    }
}

fn build_request(method: &str, params: serde_json::Value, opts: CallOpts) -> Request {
    // `Ulid::default()` is the zero-ULID, not a fresh one — so the
    // unwrap-or-default lint's suggestion is semantically wrong here.
    // Spell out the `None` arm explicitly instead.
    let request_id = match opts.request_id {
        Some(id) => id,
        None => Ulid::new(),
    };
    // arch-observability Step 4: mint a fresh `TraceId` at the CLI/
    // MCP edge so every IPC dispatch carries one — daemons stamp the
    // id onto every log line they emit while handling the request.
    // Precedence (high → low): explicit `opts.trace_id` →
    // the active `with_trace_id` span's id (so CLI logs and daemon
    // logs share an id) → a freshly minted one. The third arm
    // covers callers that don't seed a root span.
    let trace_id = Some(opts.trace_id.unwrap_or_else(|| {
        sy_core::obs::current_trace_ctx()
            .map(|tc| tc.trace_id)
            .unwrap_or_default()
    }));
    Request {
        schema_version: SCHEMA_VERSION,
        request_id,
        trace_id,
        parent_span_id: opts.parent_span_id,
        deadline_ms: opts.deadline_ms,
        priority: opts.priority,
        method: method.to_string(),
        params,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_defaults_priority_interactive() {
        // SPEC §5 friction-map: `CallOpts::default()` produces a
        // `Priority::Interactive` envelope on the wire so call sites
        // that don't care can be grep-replaced without touching
        // priority logic. A regression that flipped the default would
        // silently demote every CLI/MCP caller to Background.
        let req = build_request(
            "knowledge.search",
            serde_json::json!({}),
            CallOpts::default(),
        );
        assert_eq!(req.priority, Priority::Interactive);
        assert_eq!(req.deadline_ms, Some(DEFAULT_DEADLINE_MS));
        assert_eq!(req.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn call_auto_generates_request_id_when_unset() {
        // Two back-to-back calls with `request_id: None` must produce
        // distinct ULIDs; otherwise concurrent `system.cancel` calls
        // would correlate to the wrong in-flight request.
        let a = build_request("a", serde_json::json!({}), CallOpts::default());
        let b = build_request("b", serde_json::json!({}), CallOpts::default());
        assert_ne!(a.request_id, b.request_id);
    }

    #[test]
    fn call_auto_generates_trace_id_when_unset() {
        // arch-observability Step 4: every outbound IPC request must
        // carry a `trace_id` so the daemon can stamp it on its logs.
        // A regression that left `trace_id: None` would break the
        // SPEC §4.6 promise "trace_id is set at the CLI/MCP edge".
        let req = build_request(
            "knowledge.search",
            serde_json::json!({}),
            CallOpts::default(),
        );
        let tid = req.trace_id.expect("trace_id auto-generated");
        assert_eq!(tid.as_str().len(), 32);
        assert!(tid.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn call_inherits_ambient_trace_id_from_with_trace_id() {
        // SPEC §4.6: when the CLI seeds a root `with_trace_id`, an
        // IPC call inside that scope must carry the *seeded* id —
        // not a fresh one — so CLI logs and daemon logs share an
        // id. The precedence in `build_request` is
        // explicit-opts → ambient → fresh; this test pins the
        // ambient arm.
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Registry;
        const T: &str = "0af7651916cd43dd8448eb211c80319c";
        let subscriber = Registry::default().with(sy_core::obs::TraceCtxLayer::new());
        let dispatch = tracing::Dispatch::new(subscriber);
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None::<TraceId>));
        let observed_clone = observed.clone();
        tracing::dispatcher::with_default(&dispatch, || {
            sy_core::obs::with_trace_id(TraceId(T.into()), None, || {
                let req = build_request("m", serde_json::json!({}), CallOpts::default());
                *observed_clone.lock().expect("mutex") = req.trace_id;
            });
        });
        assert_eq!(*observed.lock().expect("mutex"), Some(TraceId(T.into())));
    }

    #[test]
    fn call_respects_explicit_trace_id() {
        // Explicit ids (CLI `--trace-id <id>`, MCP-propagated parents)
        // must round-trip unchanged. A regression that minted a fresh
        // id over the user's supplied one would break trace stitching.
        const T: &str = "0af7651916cd43dd8448eb211c80319c";
        let opts = CallOpts {
            trace_id: Some(TraceId(T.into())),
            ..CallOpts::default()
        };
        let req = build_request("a", serde_json::json!({}), opts);
        assert_eq!(req.trace_id, Some(TraceId(T.into())));
    }

    #[test]
    fn call_respects_explicit_request_id() {
        let fixed = Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");
        let opts = CallOpts {
            request_id: Some(fixed),
            ..CallOpts::default()
        };
        let req = build_request("knowledge.search", serde_json::json!({}), opts);
        assert_eq!(req.request_id, fixed);
    }
}
