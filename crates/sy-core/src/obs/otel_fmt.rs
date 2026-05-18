//! OpenTelemetry-shaped JSON log formatter (SPEC §4.6 / arch-
//! observability Step 3).
//!
//! Implements [`FormatEvent`] for the rolling JSONL appender so each
//! line conforms to the OpenTelemetry Logs Data Model and the future
//! `--otlp` exporter can be one extra `Layer` (SPEC §3.2 K6).
//!
//! Line shape (newline-delimited; one JSON object per line):
//! ```json
//! {
//!   "v": 1,
//!   "ts": "2026-05-16T23:00:00.123Z",
//!   "severity_text": "INFO",
//!   "severity_number": 9,
//!   "target": "sy::aiplane::supervisor",
//!   "span": "embed",
//!   "trace_id": "",
//!   "span_id": "",
//!   "resource": {"service.name": "sy-aiplane"},
//!   "attributes": {"workload": "embed", "latency_ms": 18.4},
//!   "body": "workload completed"
//! }
//! ```
//!
//! `trace_id` / `span_id` are filled from the nearest enclosing
//! `obs::with_trace_id` span's [`crate::obs::trace_ctx::TraceCtx`]
//! extension (Step 4); they stay empty strings when no scope has
//! been seeded so the line shape is identical inside or outside
//! a traced dispatch.
//!
//! Severity-number mapping per OpenTelemetry Logs Data Model:
//! TRACE=1, DEBUG=5, INFO=9, WARN=13, ERROR=17.
//!
//! Manual verification recipe (cannot run in unit tests):
//! `journalctl --user -u 'sy-*' -o json | jq` returns parseable
//! records on the rice. The journald layer's
//! `with_syslog_identifier("sy-<name>")` tagging in
//! [`super::init`] is what makes `-u 'sy-*'` filter correctly; this
//! file does not touch journald.

use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

use super::trace_ctx::TraceCtx;

/// OTel Logs Data Model severity numbers. Kept as named constants so
/// the level-to-number mapping is auditable without hunting through
/// the match arm.
const SEVERITY_TRACE: u8 = 1;
const SEVERITY_DEBUG: u8 = 5;
const SEVERITY_INFO: u8 = 9;
const SEVERITY_WARN: u8 = 13;
const SEVERITY_ERROR: u8 = 17;

/// JSON-schema version stamped on every log line so downstream
/// readers (`sy crash`, future OTLP shipper) can fast-path on the
/// shape.
const SCHEMA_VERSION: u64 = 1;

/// Reserved field name `tracing` synthesises from the format-args
/// passed to `info!("…", …)`. Routed to the `body` slot rather than
/// `attributes` to match SPEC §4.6.
const MESSAGE_FIELD: &str = "message";

/// `FormatEvent` impl that emits OTel-shaped lines. Constructed once
/// per Layer; cheap to clone (the service name is reference-counted).
#[derive(Clone, Debug)]
pub struct OtelFormatter {
    service_name: Arc<str>,
}

impl OtelFormatter {
    /// Build a formatter that stamps `resource.service.name` on every
    /// line. `service_name` is typically `"sy-aiplane"` /
    /// `"sy-knowledge"` / etc. — the daemon basename passed into
    /// `Mode::Daemon { name }`. CLI mode passes the empty string;
    /// the formatter still produces a valid line, just with
    /// `resource.service.name: ""`.
    pub fn new(service_name: impl Into<Arc<str>>) -> Self {
        Self {
            service_name: service_name.into(),
        }
    }
}

impl<S, N> FormatEvent<S, N> for OtelFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        let (severity_text, severity_number) = severity_for(*metadata.level());

        let mut visitor = OtelVisitor::default();
        event.record(&mut visitor);

        // `parent_span()` is the event's contextual parent: the
        // innermost `#[instrument]`-style span at the call site.
        // SPEC §4.6 / the roadmap want the leaf-most span name here.
        let span_name: String = ctx
            .parent_span()
            .map(|s| s.name().to_string())
            .unwrap_or_default();

        // arch-observability Step 4: walk up the event's span scope
        // looking for the nearest `TraceCtx` extension. The Layer
        // (`super::trace_ctx::TraceCtxLayer`) stamps the extension
        // when `with_trace_id` opens its wrapping span; if no scope
        // carries one, the slots stay empty (Step 3 placeholder).
        let (trace_id, span_id) = trace_ctx_from_event(ctx);

        let mut resource = Map::new();
        resource.insert(
            "service.name".to_string(),
            Value::String(self.service_name.as_ref().to_string()),
        );

        let mut line = Map::new();
        line.insert("v".to_string(), Value::from(SCHEMA_VERSION));
        line.insert("ts".to_string(), Value::String(rfc3339_millis_utc_now()));
        line.insert(
            "severity_text".to_string(),
            Value::String(severity_text.to_string()),
        );
        line.insert("severity_number".to_string(), Value::from(severity_number));
        line.insert(
            "target".to_string(),
            Value::String(metadata.target().to_string()),
        );
        line.insert("span".to_string(), Value::String(span_name));
        line.insert("trace_id".to_string(), Value::String(trace_id));
        line.insert("span_id".to_string(), Value::String(span_id));
        line.insert("resource".to_string(), Value::Object(resource));
        line.insert("attributes".to_string(), Value::Object(visitor.attributes));
        line.insert("body".to_string(), Value::String(visitor.body));

        // `Value`'s Display impl uses `serde_json::to_string`, which
        // never embeds a newline, so the `\n` we append is the only
        // record separator — i.e. line-delimited JSON.
        let value = Value::Object(line);
        writeln!(writer, "{value}")
    }
}

/// Walk up the event's contextual scope and return the
/// `(trace_id, span_id)` of the nearest enclosing span that carries a
/// `TraceCtx`. Empty strings when no scope has been seeded — the
/// formatter stays usable outside an `obs::with_trace_id` closure.
fn trace_ctx_from_event<S, N>(ctx: &FmtContext<'_, S, N>) -> (String, String)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    let Some(scope) = ctx.event_scope() else {
        return (String::new(), String::new());
    };
    // `scope` walks innermost → outermost by default; the first
    // span we find with a `TraceCtx` extension is the closest
    // enclosing `with_trace_id` invocation.
    for span in scope {
        if let Some(tc) = span.extensions().get::<TraceCtx>() {
            return (
                tc.trace_id.as_str().to_string(),
                tc.span_id.as_str().to_string(),
            );
        }
    }
    (String::new(), String::new())
}

/// Map a `tracing::Level` to the SPEC §4.6 / OTel Logs Data Model
/// pair `(severity_text, severity_number)`.
fn severity_for(level: Level) -> (&'static str, u8) {
    match level {
        Level::TRACE => ("TRACE", SEVERITY_TRACE),
        Level::DEBUG => ("DEBUG", SEVERITY_DEBUG),
        Level::INFO => ("INFO", SEVERITY_INFO),
        Level::WARN => ("WARN", SEVERITY_WARN),
        Level::ERROR => ("ERROR", SEVERITY_ERROR),
    }
}

/// RFC3339 timestamp truncated to millisecond precision, in UTC.
/// Single source of truth for the `ts` field so the formatter and
/// any future helpers (e.g. crash records) stay aligned.
fn rfc3339_millis_utc_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Field visitor that splits the synthetic `message` field out into
/// the `body` slot and routes everything else into `attributes`.
/// Uses `Debug` for non-primitive types to match the default tracing
/// JSON formatter's behaviour (and so we stay readable when an
/// `Error` propagates through `error = ?err`).
#[derive(Default)]
struct OtelVisitor {
    body: String,
    attributes: Map<String, Value>,
}

impl OtelVisitor {
    fn insert(&mut self, field: &Field, value: Value) {
        if field.name() == MESSAGE_FIELD {
            if let Value::String(s) = value {
                self.body = s;
            } else {
                self.body = value.to_string();
            }
        } else {
            self.attributes.insert(field.name().to_string(), value);
        }
    }
}

impl Visit for OtelVisitor {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, Value::from(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, Value::String(value.to_string()));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, Value::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.insert(field, Value::String(format!("{value:?}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use tracing::instrument;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::layer::SubscriberExt;

    const SERVICE_NAME: &str = "sy-aiplane";

    /// `MakeWriter` that appends every formatted line to an
    /// `Arc<Mutex<Vec<u8>>>` so tests can read the bytes back without
    /// touching the filesystem.
    #[derive(Clone)]
    struct BufWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl BufWriter {
        fn new() -> Self {
            Self {
                buf: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn lines(&self) -> Vec<String> {
            let bytes = self.buf.lock().expect("buf mutex").clone();
            let text = String::from_utf8(bytes).expect("utf8");
            text.lines().map(str::to_string).collect()
        }
    }

    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().expect("buf mutex").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn subscriber_with(writer: BufWriter, service: &str) -> tracing::Dispatch {
        let layer = tracing_subscriber::fmt::layer()
            .event_format(OtelFormatter::new(service.to_string()))
            .with_writer(writer);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::Dispatch::new(subscriber)
    }

    fn first_line_as_object(writer: &BufWriter) -> Map<String, Value> {
        let lines = writer.lines();
        assert!(!lines.is_empty(), "no log lines emitted");
        let parsed: Value = serde_json::from_str(&lines[0]).expect("first line is valid JSON");
        match parsed {
            Value::Object(map) => map,
            other => panic!("expected JSON object, got {other:?}"),
        }
    }

    #[test]
    fn info_event_matches_otel_shape() {
        let writer = BufWriter::new();
        let dispatch = subscriber_with(writer.clone(), SERVICE_NAME);
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(workload = "embed", batch = 32, "hello otel");
        });

        let obj = first_line_as_object(&writer);
        for key in [
            "v",
            "ts",
            "severity_text",
            "severity_number",
            "target",
            "span",
            "trace_id",
            "span_id",
            "resource",
            "attributes",
            "body",
        ] {
            assert!(obj.contains_key(key), "missing field: {key}");
        }
        assert_eq!(obj["v"], Value::from(SCHEMA_VERSION));
        assert_eq!(obj["severity_text"], Value::String("INFO".into()));
        assert_eq!(obj["severity_number"], Value::from(SEVERITY_INFO));
        assert_eq!(obj["body"], Value::String("hello otel".into()));
        assert_eq!(obj["trace_id"], Value::String(String::new()));
        assert_eq!(obj["span_id"], Value::String(String::new()));
        let attrs = obj["attributes"].as_object().expect("attributes object");
        assert_eq!(attrs["workload"], Value::String("embed".into()));
        assert_eq!(attrs["batch"], Value::from(32));
        assert!(
            !attrs.contains_key(MESSAGE_FIELD),
            "message must not leak into attributes; it belongs in body"
        );
    }

    #[test]
    fn error_event_severity_number_is_17() {
        let writer = BufWriter::new();
        let dispatch = subscriber_with(writer.clone(), SERVICE_NAME);
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::error!("boom");
        });

        let obj = first_line_as_object(&writer);
        assert_eq!(obj["severity_text"], Value::String("ERROR".into()));
        assert_eq!(obj["severity_number"], Value::from(SEVERITY_ERROR));
    }

    #[test]
    fn span_field_carries_innermost_span_name() {
        #[instrument]
        fn embed_batch(batch_size: u32) {
            tracing::info!(rows = batch_size, "inside span");
        }

        let writer = BufWriter::new();
        let dispatch = subscriber_with(writer.clone(), SERVICE_NAME);
        tracing::dispatcher::with_default(&dispatch, || {
            embed_batch(7);
        });

        let obj = first_line_as_object(&writer);
        // Step 4 populates `span_id` only when a `TraceCtxLayer`
        // observes a `with_trace_id` wrapping span. A bare
        // `#[instrument]`-style span — like this test — keeps the
        // slot empty so consumers that never opt into trace
        // propagation see a stable shape.
        assert_eq!(
            obj["span"],
            Value::String("embed_batch".into()),
            "span name must be the innermost #[instrument]ed fn"
        );
        assert_eq!(
            obj["span_id"],
            Value::String(String::new()),
            "span_id stays empty without a `with_trace_id` ancestor"
        );
    }

    #[test]
    fn resource_carries_daemon_name() {
        let writer = BufWriter::new();
        let dispatch = subscriber_with(writer.clone(), "sy-knowledge");
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!("ping");
        });

        let obj = first_line_as_object(&writer);
        let resource = obj["resource"].as_object().expect("resource object");
        assert_eq!(
            resource["service.name"],
            Value::String("sy-knowledge".into()),
            "resource.service.name carries the Mode::Daemon name"
        );
    }
}
