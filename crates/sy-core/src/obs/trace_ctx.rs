//! Trace context propagation for SPEC §4.6 / arch-observability Step 4.
//!
//! The IPC envelope (sy-ipc v1) carries a `trace_id` and optional
//! `parent_span_id` on every `Request`. This module connects those
//! ids into the `tracing` span tree so every event the handler emits
//! is stamped with them — independently of the call site.
//!
//! Two layers cooperate:
//!
//! - [`TraceCtxLayer`] is a Subscriber [`Layer`] installed below the
//!   formatter. On `new_span`, it reads the span's recorded
//!   `trace_id` / `parent_span_id` fields and stores a [`TraceCtx`]
//!   in the span's `Extensions`.
//! - [`super::otel_fmt::OtelFormatter`] walks `ctx.event_scope()` at
//!   format time and emits the innermost `TraceCtx` it finds.
//!
//! [`with_trace_id`] is the synchronous entry point: build a span,
//! record the ids on it, enter the span for the duration of `f`.
//! [`with_trace_id_async`] is the `.instrument()`-based variant for
//! futures — the server uses it because handlers are `async`.

use std::future::Future;

use tracing::field::{Field, Visit};
use tracing::span::Attributes;
use tracing::{Id, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::trace::{SpanId, TraceId};

/// Field name `with_trace_id` records on the span it creates. Kept as
/// a constant so the layer and the call site stay in lockstep — a
/// rename here without touching the layer would silently break trace
/// propagation.
const FIELD_TRACE_ID: &str = "trace_id";

/// Field name `with_trace_id` records for the optional parent span.
const FIELD_PARENT_SPAN_ID: &str = "parent_span_id";

/// Span name `with_trace_id` uses for the wrapping span. Read by the
/// OTel formatter's `span` slot so log lines emitted inside an IPC
/// dispatch carry `"span":"ipc.request"` by default. Inner
/// `#[instrument]`-style spans override it because the formatter
/// resolves the innermost span name.
pub const TRACE_SPAN_NAME: &str = "ipc.request";

/// Per-span trace context. Stored in [`tracing_subscriber::registry::SpanRef::extensions`]
/// by [`TraceCtxLayer::on_new_span`] and read back by the OTel
/// formatter at event time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceCtx {
    /// W3C `trace-id`. Constant across the IPC dispatch chain — the
    /// CLI mints it, the daemon receives it on the request envelope,
    /// every event the handler emits carries it.
    pub trace_id: TraceId,
    /// W3C `span-id` minted for this specific span by the layer.
    /// The OTel formatter emits it in the `span_id` slot of every
    /// event whose nearest enclosing `TraceCtx` is this one.
    pub span_id: SpanId,
    /// W3C `parent-id`, when the caller knew its own span. `None` at
    /// the root of the chain (CLI edge).
    pub parent_span_id: Option<SpanId>,
}

/// Subscriber [`Layer`] that lifts `trace_id` / `parent_span_id`
/// recorded fields into per-span [`TraceCtx`] extensions. Install it
/// alongside the formatter Layer; without this layer, `with_trace_id`
/// degrades to a plain span (no trace stamping in logs).
#[derive(Clone, Debug, Default)]
pub struct TraceCtxLayer;

impl TraceCtxLayer {
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for TraceCtxLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = TraceFieldVisitor::default();
        attrs.record(&mut visitor);
        let Some(trace_id) = visitor.trace_id.map(TraceId) else {
            return;
        };
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(TraceCtx {
                trace_id,
                // Fresh per-span id at creation time so events
                // emitted under this span carry a stable `span_id`
                // distinct from sibling spans on the same trace.
                span_id: SpanId::new(),
                parent_span_id: visitor.parent_span_id.map(SpanId),
            });
        }
    }
}

/// Visits a span's recorded fields and pulls out the trace ids. Other
/// fields are ignored; routing them to attributes is the formatter's
/// job, not this layer's.
#[derive(Default)]
struct TraceFieldVisitor {
    trace_id: Option<String>,
    parent_span_id: Option<String>,
}

impl Visit for TraceFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            FIELD_TRACE_ID if !value.is_empty() => self.trace_id = Some(value.to_string()),
            // Empty `parent_span_id` round-trips as `None`: the wire
            // never sends an all-zero parent, and an empty hex string
            // would deserialise to `Some(SpanId(""))` which is
            // meaningless to the formatter.
            FIELD_PARENT_SPAN_ID if !value.is_empty() => {
                self.parent_span_id = Some(value.to_string());
            }
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {
        // `with_trace_id` always records the ids via `record_str`
        // (they're `&str`-backed). Other field types are noise here.
    }
}

/// Run `f` inside a span that carries `trace_id` and an optional
/// `parent_span_id`. The span is named `TRACE_SPAN_NAME`.
///
/// Returns whatever `f` returns. Note that if `f` returns a future,
/// only its creation is wrapped in the span — to instrument the
/// poll-cycle of an async handler use [`with_trace_id_async`] instead.
///
/// This is the synchronous edge entry point: CLI / MCP code mints a
/// `TraceId` and wraps its top-level dispatch in `with_trace_id` so
/// every downstream log line carries the id.
pub fn with_trace_id<F, R>(trace_id: TraceId, parent: Option<SpanId>, f: F) -> R
where
    F: FnOnce() -> R,
{
    let span = build_trace_span(&trace_id, parent.as_ref());
    span.in_scope(f)
}

/// Async-aware variant of [`with_trace_id`]. Wraps `fut` with
/// [`tracing::Instrument`] so the span follows the future across
/// `.await` points. The server uses this; CLI code that doesn't
/// produce a future calls the synchronous [`with_trace_id`] instead.
pub async fn with_trace_id_async<Fut, R>(trace_id: TraceId, parent: Option<SpanId>, fut: Fut) -> R
where
    Fut: Future<Output = R>,
{
    use tracing::Instrument;
    let span = build_trace_span(&trace_id, parent.as_ref());
    fut.instrument(span).await
}

/// Read the nearest enclosing [`TraceCtx`] from the current span.
/// Returns `None` outside any `with_trace_id` / `with_trace_id_async`
/// scope, or when no [`TraceCtxLayer`] is installed. Callers use this
/// to propagate the active trace_id into outgoing IPC requests — the
/// alternative (the CLI threading a `TraceId` through every function
/// signature) is what SPEC §4.6 explicitly rejects.
pub fn current_trace_ctx() -> Option<TraceCtx> {
    use tracing_subscriber::registry::LookupSpan;

    let current = tracing::Span::current();
    let mut snapshot: Option<TraceCtx> = None;
    current.with_subscriber(|(id, dispatch)| {
        let Some(reg) = dispatch.downcast_ref::<tracing_subscriber::Registry>() else {
            return;
        };
        if let Some(span) = <tracing_subscriber::Registry as LookupSpan>::span(reg, id) {
            // Walk parents in case the nearest span doesn't carry a
            // `TraceCtx` itself (e.g. an inner `#[instrument]` nested
            // inside `with_trace_id`).
            for ancestor in span.scope() {
                if let Some(tc) = ancestor.extensions().get::<TraceCtx>() {
                    snapshot = Some(tc.clone());
                    return;
                }
            }
        }
    });
    snapshot
}

fn build_trace_span(trace_id: &TraceId, parent: Option<&SpanId>) -> tracing::Span {
    // `info_span!` records `trace_id` / `parent_span_id` as `&str`
    // fields; `TraceCtxLayer::on_new_span` picks them up. An empty
    // string in the `parent_span_id` slot means "no parent" — the
    // layer's `Option<String>` round-trips through the visitor.
    let parent_str = parent.map(SpanId::as_str).unwrap_or("");
    tracing::info_span!(
        TRACE_SPAN_NAME,
        trace_id = trace_id.as_str(),
        parent_span_id = parent_str,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    const T_HEX: &str = "0af7651916cd43dd8448eb211c80319c";
    const PARENT_HEX: &str = "b7ad6b7169203331";

    #[test]
    fn with_trace_id_stores_trace_ctx_in_span_extensions() {
        // Behaviour: after `with_trace_id` enters its span, the
        // current span's `TraceCtx` extension carries the supplied
        // ids. This is what the OTel formatter reads at event time;
        // a regression that dropped the insert would silently emit
        // empty `trace_id` fields.
        let subscriber = Registry::default().with(TraceCtxLayer::new());
        let dispatch = tracing::Dispatch::new(subscriber);
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None::<TraceCtx>));
        let observed_clone = observed.clone();
        tracing::dispatcher::with_default(&dispatch, || {
            with_trace_id(
                TraceId(T_HEX.into()),
                Some(SpanId(PARENT_HEX.into())),
                || {
                    let current = tracing::Span::current();
                    current.with_subscriber(|(id, dispatch)| {
                        let reg = dispatch
                            .downcast_ref::<Registry>()
                            .expect("registry available");
                        let span = reg.span(id).expect("current span lives in the registry");
                        let ctx = span
                            .extensions()
                            .get::<TraceCtx>()
                            .cloned()
                            .expect("TraceCtx present");
                        *observed_clone.lock().expect("mutex") = Some(ctx);
                    });
                },
            );
        });
        let ctx = observed.lock().expect("mutex").clone().expect("set");
        assert_eq!(ctx.trace_id, TraceId(T_HEX.into()));
        assert_eq!(ctx.parent_span_id, Some(SpanId(PARENT_HEX.into())));
        // The layer mints a fresh per-span id so concurrent IPC
        // dispatches sharing a `TraceId` still have distinguishable
        // `span_id`s in the log.
        assert_eq!(ctx.span_id.as_str().len(), 16);
        assert!(ctx.span_id.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn current_trace_ctx_reads_active_span_extensions() {
        // The CLI seeds a root `with_trace_id` then makes an IPC
        // call; `Client::call` reads the active id back via
        // `current_trace_ctx` so the daemon's log line ends up with
        // the same `trace_id` as the CLI's. A regression that didn't
        // walk parent spans would surface here when the test's
        // inner `info_span!` (no `TraceCtx`) shadows the outer one.
        let subscriber = Registry::default().with(TraceCtxLayer::new());
        let dispatch = tracing::Dispatch::new(subscriber);
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None::<TraceId>));
        let observed_clone = observed.clone();
        tracing::dispatcher::with_default(&dispatch, || {
            with_trace_id(TraceId(T_HEX.into()), None, || {
                // Open an unrelated inner span to prove the helper
                // walks up to the nearest `TraceCtx`-carrying span.
                let inner = tracing::info_span!("inner_call");
                inner.in_scope(|| {
                    if let Some(tc) = current_trace_ctx() {
                        *observed_clone.lock().expect("mutex") = Some(tc.trace_id);
                    }
                });
            });
        });
        assert_eq!(
            *observed.lock().expect("mutex"),
            Some(TraceId(T_HEX.into()))
        );
    }

    #[test]
    fn current_trace_ctx_is_none_without_seeded_scope() {
        // Outside any `with_trace_id`, the helper returns `None`.
        // `Client::call` falls through to minting a fresh id in
        // this branch.
        let subscriber = Registry::default().with(TraceCtxLayer::new());
        let dispatch = tracing::Dispatch::new(subscriber);
        let observed: Option<TraceCtx> =
            tracing::dispatcher::with_default(&dispatch, current_trace_ctx);
        assert_eq!(observed, None);
    }

    #[test]
    fn with_trace_id_none_parent_yields_none_in_ctx() {
        // CLI root call: no parent span. `None` must propagate
        // through the visitor — an empty-string parent_span_id on
        // the recorded field would otherwise round-trip as
        // `Some(SpanId(""))`, which is invalid for the wire.
        let subscriber = Registry::default().with(TraceCtxLayer::new());
        let dispatch = tracing::Dispatch::new(subscriber);
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None::<TraceCtx>));
        let observed_clone = observed.clone();
        tracing::dispatcher::with_default(&dispatch, || {
            with_trace_id(TraceId(T_HEX.into()), None, || {
                let current = tracing::Span::current();
                current.with_subscriber(|(id, dispatch)| {
                    let reg = dispatch
                        .downcast_ref::<Registry>()
                        .expect("registry available");
                    let span = reg.span(id).expect("current span lives in the registry");
                    let ctx = span
                        .extensions()
                        .get::<TraceCtx>()
                        .cloned()
                        .expect("TraceCtx present");
                    *observed_clone.lock().expect("mutex") = Some(ctx);
                });
            });
        });
        let ctx = observed.lock().expect("mutex").clone().expect("set");
        assert_eq!(ctx.trace_id, TraceId(T_HEX.into()));
        assert_eq!(ctx.parent_span_id, None);
    }
}
