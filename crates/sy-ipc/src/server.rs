//! IPC v1 server scaffold (SPEC §4.2). Wraps an accepted
//! `tokio::net::UnixStream` in the framing codec, enforces the
//! `SO_PEERCRED` origin gate (peer uid must equal `geteuid()`), and
//! dispatches each decoded `Request` to a user-supplied `Handler`.
//!
//! Reserved-method (`system.{describe,health,cancel}`) wiring lands
//! in Step 3; this module only carries the bare connection loop.

use std::io;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use metrics::counter;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{RequestCodec, ResponseCodec};
use crate::envelope::{Request, Response};

/// Async request handler. One instance is shared across every
/// connection the `Server` accepts (`Send + Sync + 'static`), so
/// per-connection state lives behind interior-mutability locks
/// inside the impl.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: Request) -> impl std::future::Future<Output = Response> + Send;
}

/// Bare IPC v1 server. Accepts UDS connections, gates on
/// `SO_PEERCRED`, dispatches each frame to `H::handle`.
pub struct Server<H: Handler> {
    handler: Arc<H>,
}

impl<H: Handler> Server<H> {
    pub fn new(handler: H) -> Self {
        Self {
            handler: Arc::new(handler),
        }
    }

    /// Accept-loop. Returns only on listener error; per-connection
    /// errors are logged-and-dropped so a misbehaving client cannot
    /// take the whole daemon down.
    pub async fn serve(self, listener: UnixListener) -> io::Result<()> {
        loop {
            let (stream, _addr) = listener.accept().await?;
            if !is_peer_self(&stream) {
                // SPEC §4.2 origin check: reject anything that isn't
                // the same uid as the daemon. Silently drop the
                // connection — no error response, since the wire
                // contract requires the peer to be trusted to even
                // parse the frame.
                drop(stream);
                continue;
            }
            let handler = Arc::clone(&self.handler);
            tokio::spawn(serve_connection(stream, handler));
        }
    }
}

/// `SO_PEERCRED`-equivalent origin check. Tokio's `peer_cred()`
/// returns the kernel-verified credentials of the connected peer;
/// `rustix::process::geteuid()` returns this process's effective UID.
/// Equality is the SPEC §4.2 admission rule on a single-user host.
fn is_peer_self(stream: &UnixStream) -> bool {
    let Ok(cred) = stream.peer_cred() else {
        return false;
    };
    cred.uid() == rustix::process::geteuid().as_raw()
}

async fn serve_connection<H: Handler>(stream: UnixStream, handler: Arc<H>) {
    let (reader, writer) = stream.into_split();
    let mut req_stream = FramedRead::new(reader, RequestCodec::default());
    let mut resp_sink = FramedWrite::new(writer, ResponseCodec::default());
    while let Some(decoded) = req_stream.next().await {
        let req = match decoded {
            Ok(req) => req,
            Err(_) => break,
        };
        // arch-observability Step 4 / SPEC §4.6: stamp the
        // request's trace context onto the dispatch span so every
        // event the handler emits carries `trace_id` / `span_id`.
        // Skip the wrapping span when the envelope has no
        // `trace_id` — falling through to `handle` directly keeps
        // legacy callers (no observability subscriber installed)
        // free of the overhead.
        let endpoint = req.method.clone();
        let resp = match req.trace_id.clone() {
            Some(trace_id) => {
                let parent = req.parent_span_id.clone();
                sy_core::obs::with_trace_id_async(trace_id, parent, handler.handle(req)).await
            }
            None => handler.handle(req).await,
        };
        // arch-observability Step 7 / SPEC §4.6: bump
        // `sy_ipc_errors_total{endpoint, kind}` on every error
        // response. `endpoint` is the request's `method`; `kind` is
        // the wire-stable `ErrorCode` string.
        if let Response::Err { error, .. } = &resp {
            counter!(
                "sy_ipc_errors_total",
                "endpoint" => endpoint,
                "kind" => error.code.as_str(),
            )
            .increment(1);
        }
        if resp_sink.send(resp).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{CallOpts, Client};
    use crate::envelope::{SpanId, TraceId, SCHEMA_VERSION};
    use std::sync::{Arc as StdArc, Mutex, OnceLock};
    use sy_core::obs::{TraceCtx, TraceCtxLayer};
    use sy_core::Priority;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;
    use ulid::Ulid;

    /// Install a process-global `Registry + TraceCtxLayer` once so
    /// the tokio task `Server::serve` spawns inherits a subscriber
    /// that records `TraceCtx` on `with_trace_id_async`'s span. The
    /// `set_global_default` lock means only the first test through
    /// here wins; subsequent calls become no-ops and reuse the
    /// installed dispatcher. Using a `Once` keeps parallel `cargo
    /// test` workers race-free.
    fn install_trace_ctx_subscriber() {
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            let subscriber = Registry::default().with(TraceCtxLayer::new());
            // `set_global_default` succeeds at most once per process;
            // a second install attempt errors but the global already
            // matches what we want, so swallow the result.
            let _ = tracing::subscriber::set_global_default(subscriber);
        });
    }

    struct Echo;
    impl Handler for Echo {
        async fn handle(&self, req: Request) -> Response {
            Response::Ok {
                schema_version: SCHEMA_VERSION,
                request_id: req.request_id,
                result: serde_json::json!({
                    "method": req.method,
                    "priority": req.priority,
                }),
                blob: None,
            }
        }
    }

    #[tokio::test]
    async fn client_server_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp.path().join("ipc.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let server = Server::new(Echo);
        let server_handle = tokio::spawn(async move {
            // Single connection is enough; the test never reuses the
            // listener so a `_` on the result is fine here.
            let _ = server.serve(listener).await;
        });

        let mut client = Client::connect(&sock).await.expect("connect");
        let fixed_id = Ulid::from_string("01HXYZ0000000000000000000Z").expect("ulid");
        let opts = CallOpts {
            request_id: Some(fixed_id),
            ..CallOpts::default()
        };
        let resp = client
            .call("system.health", serde_json::json!({}), opts)
            .await
            .expect("call");
        match resp {
            Response::Ok {
                schema_version,
                request_id,
                result,
                blob,
            } => {
                assert_eq!(schema_version, SCHEMA_VERSION);
                assert_eq!(request_id, fixed_id);
                assert_eq!(result["method"], "system.health");
                assert_eq!(result["priority"], serde_json::json!(Priority::Interactive));
                assert!(blob.is_none());
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        server_handle.abort();
    }

    /// Handler that snapshots the `TraceCtx` from `Span::current()`
    /// into a shared slot. The mutex slot is the only side-channel
    /// — the response itself doesn't echo the id, so a server that
    /// dropped the `with_trace_id_async` wrap would leave the slot
    /// `None` and the assertion would fire.
    struct TraceObserver {
        observed: StdArc<Mutex<Option<TraceCtx>>>,
    }

    impl Handler for TraceObserver {
        async fn handle(&self, req: Request) -> Response {
            // Grab the current span; downcast the dispatcher to a
            // `Registry` so we can read `TraceCtx` from extensions.
            // Mirrors how `obs::otel_fmt::trace_ctx_from_event`
            // walks the scope at format time.
            let current = tracing::Span::current();
            let snapshot = self.observed.clone();
            current.with_subscriber(|(id, dispatch)| {
                let Some(reg) = dispatch.downcast_ref::<Registry>() else {
                    return;
                };
                let Some(span) =
                    <Registry as tracing_subscriber::registry::LookupSpan>::span(reg, id)
                else {
                    return;
                };
                let extensions = span.extensions();
                if let Some(tc) = extensions.get::<TraceCtx>() {
                    *snapshot.lock().expect("mutex") = Some(tc.clone());
                }
            });
            Response::Ok {
                schema_version: SCHEMA_VERSION,
                request_id: req.request_id,
                result: serde_json::json!({}),
                blob: None,
            }
        }
    }

    #[tokio::test]
    async fn server_picks_up_request_trace_id() {
        // SPEC §4.6 / arch-observability Step 4: the server must
        // wrap the handler dispatch in `with_trace_id_async` so the
        // request's `trace_id` ends up in the current span's
        // `TraceCtx` extension. The observer handler reads it back
        // and the test asserts equality with what the client sent.
        const TRACE_HEX: &str = "0af7651916cd43dd8448eb211c80319c";
        const PARENT_HEX: &str = "b7ad6b7169203331";
        install_trace_ctx_subscriber();

        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp.path().join("ipc.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let observed: StdArc<Mutex<Option<TraceCtx>>> = StdArc::new(Mutex::new(None));
        let server = Server::new(TraceObserver {
            observed: observed.clone(),
        });
        let server_handle = tokio::spawn(async move {
            let _ = server.serve(listener).await;
        });

        let mut client = Client::connect(&sock).await.expect("connect");
        let opts = CallOpts {
            trace_id: Some(TraceId(TRACE_HEX.into())),
            parent_span_id: Some(SpanId(PARENT_HEX.into())),
            ..CallOpts::default()
        };
        let _ = client
            .call("system.health", serde_json::json!({}), opts)
            .await
            .expect("call");

        let ctx = observed.lock().expect("mutex").clone().expect(
            "handler must observe a TraceCtx — server isn't wrapping dispatch in with_trace_id_async",
        );
        assert_eq!(ctx.trace_id, TraceId(TRACE_HEX.into()));
        assert_eq!(ctx.parent_span_id, Some(SpanId(PARENT_HEX.into())));
        server_handle.abort();
    }

    #[tokio::test]
    #[ignore = "requires the ability to drop privileges to a foreign uid; runs under root or with setuid sandbox only"]
    async fn rejects_foreign_uid() {
        // Coverage rationale (SPEC §4.2): the kernel also enforces
        // `0700`/`0600` on `$XDG_RUNTIME_DIR`, so a non-root userspace
        // cannot actually open a foreign-uid UDS in the default
        // single-user setup. This test stays `#[ignore]` until a
        // privileged CI runner can simulate the cross-uid scenario;
        // the implementation is exercised by inspection of
        // `is_peer_self` and the `client_server_round_trip` happy
        // path (which proves the gate admits the matching uid).
        panic!("ignored — see test docstring");
    }
}
