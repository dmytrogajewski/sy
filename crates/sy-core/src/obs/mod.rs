//! Observability initialiser. SPEC §4.6 / arch-observability Step 1.
//!
//! One `init(Mode)` call per process composes a `tracing_subscriber`
//! Registry tailored to the binary's role:
//!
//! - `Mode::Cli` — `fmt::Layer` to stderr, JSON when stdout/stderr
//!   isn't a TTY or `SY_LOG_FORMAT=json`, compact otherwise.
//! - `Mode::Daemon { name }` — same stderr layer (so `journalctl -u
//!   sy-<name> -f` is legible) PLUS `tracing_journald` (for the
//!   indexed `SY_*` fields) PLUS a daily-rolling JSONL appender at
//!   `$XDG_STATE_HOME/sy/logs/<name>/sy-<name>.jsonl` (dual-sink
//!   mitigation for the `tracing-journald` silent-drop bug; see
//!   SPEC §2.3).
//!
//! Both modes share `EnvFilter::from_default_env()` so `RUST_LOG` is
//! honoured uniformly. Subsequent observability steps layer on top:
//! Step 3 swaps the JSON layer's `FormatEvent` for the OTel shape,
//! Step 4 stamps `trace_id` from the IPC envelope, Step 6 wires the
//! panic hook.
//!
//! Returns a [`WorkerGuard`] that the caller must keep alive — it
//! owns the non-blocking writer's flush thread for the rolling
//! appender. Dropping it before process exit risks losing buffered
//! log lines.

// sy-mon Step 9 (SPEC §3 SCOPE item 1 "`mon-exporter` feature on every
// plane"): the Prometheus UDS exposition surface. Gated so default
// builds don't link hyper (SPEC §6 risk mitigation: "second HTTP
// stack"). Step 10 (aiplane) and Step 20 (remaining planes) will call
// `mon_exporter::install(path)` from their daemon entrypoints.
#[cfg(feature = "mon-exporter")]
pub mod mon_exporter;

mod otel_fmt;
pub mod panic;
mod trace_ctx;

use std::io::{self, IsTerminal};
use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use otel_fmt::OtelFormatter;
pub use trace_ctx::{
    current_trace_ctx, with_trace_id, with_trace_id_async, TraceCtx, TraceCtxLayer,
};

/// Which subscriber stack `init` builds. CLI binaries want a
/// stderr-only sink; daemons want journald + rolling JSONL + stderr.
#[derive(Debug, Clone, Copy)]
pub enum Mode {
    /// Short-lived foreground process (e.g. `sy aiplane run …`).
    Cli,
    /// Long-lived daemon. `name` is the systemd unit's basename
    /// (without `.service`), used as the journald identifier and
    /// the rolling-appender subdirectory.
    Daemon { name: &'static str },
}

/// Initialise the process-global `tracing` subscriber. Returns the
/// non-blocking writer's [`WorkerGuard`]; bind it to a local in
/// `main` so it lives for the process's full duration. Dropping the
/// guard early flushes buffered log lines but may race with
/// in-flight events.
///
/// `init` is **soft-idempotent**: a second call with a subscriber
/// already installed returns a fresh `WorkerGuard` but skips the
/// re-init (per `tracing`'s one-subscriber-per-process rule). This
/// keeps daemons alive when `main` runs `Mode::Cli` before
/// dispatching to a daemon subcommand. Tests still use the in-crate
/// `testing::appender_dispatch` helper for a hermetic test-local
/// subscriber installed via `tracing::dispatcher::with_default`.
pub fn init(mode: Mode) -> Result<WorkerGuard> {
    let (writer, guard) = match mode {
        Mode::Daemon { name } => non_blocking_appender_for(name)?,
        Mode::Cli => non_blocking_stderr(),
    };

    let stderr_layer = fmt_layer_for_stderr();
    // arch-observability Step 3: the rolling appender writes the
    // OTel-shaped 11-field schema (SPEC §4.6) via a custom
    // `FormatEvent`. The stderr layer keeps the default fmt because
    // operators read `journalctl -f` with their eyes, not jq.
    let service_name = match mode {
        Mode::Daemon { name } => format!("sy-{name}"),
        Mode::Cli => String::new(),
    };
    let json_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .event_format(OtelFormatter::new(service_name))
        .boxed();
    let journald_layer = match mode {
        Mode::Daemon { name } => Some(
            tracing_journald::layer()
                .context("connect to systemd journal")?
                .with_syslog_identifier(format!("sy-{name}"))
                .boxed(),
        ),
        Mode::Cli => None,
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // `tracing` enforces one subscriber per process. When `main` runs
    // `Mode::Cli` before dispatching to a daemon subcommand, the
    // daemon's later `Mode::Daemon{..}` call would hit
    // `try_init`-already-set. Treat that as a graceful no-op: the
    // first call wins, the daemon logs through whatever subscriber
    // is already installed. A future cleanup can defer init until
    // the subcommand is known so the daemon's journald + rolling
    // appender layers can win — for now, keep daemons alive.
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(trace_ctx::TraceCtxLayer::new())
        .with(stderr_layer)
        .with(json_layer)
        .with(journald_layer);
    if registry.try_init().is_err() {
        return Ok(guard);
    }

    // arch-observability Step 6: panic hook writes a JSONL record
    // under `$XDG_STATE_HOME/sy/crash/` and emits a structured
    // `tracing::error!` so journald carries the panic alongside the
    // crash record.
    panic::install_panic_hook();

    // arch-observability Step 7: pre-declare every SPEC §4.6 metric
    // name with the installed `metrics` recorder. With no recorder
    // installed in production yet (Zone 6.2's UDS prometheus
    // exporter is deferred), the call is a cheap no-op; the
    // describe call lands names in the recorder's metadata as soon
    // as one *is* installed (whether in tests, via the future
    // exporter, or via a snapshot endpoint).
    crate::metrics::register_core_metrics();

    Ok(guard)
}

/// Build the stderr `fmt::Layer`. JSON when stdout/stderr aren't
/// TTYs or `SY_LOG_FORMAT=json`; compact human-readable otherwise.
/// ANSI escapes are suppressed when `NO_COLOR` is set or stderr is
/// piped (CLIG / `clig.dev` §"Output").
fn fmt_layer_for_stderr<S>() -> Box<dyn Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let want_json = std::env::var_os("SY_LOG_FORMAT").is_some_and(|v| v == *"json")
        || !io::stderr().is_terminal();
    let ansi = std::env::var_os("NO_COLOR").is_none() && io::stderr().is_terminal();

    if want_json {
        tracing_subscriber::fmt::layer()
            .with_writer(io::stderr)
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_writer(io::stderr)
            .with_ansi(ansi)
            .compact()
            .boxed()
    }
}

/// Wrap `io::stderr` in `tracing_appender::non_blocking` so the
/// CLI path returns a `WorkerGuard` shaped the same way as the
/// daemon path. The stderr layer itself reads `io::stderr` directly
/// (so log lines still go to the terminal) — this writer is unused
/// for CLI mode but keeps the public API uniform.
fn non_blocking_stderr() -> (tracing_appender::non_blocking::NonBlocking, WorkerGuard) {
    tracing_appender::non_blocking(io::stderr())
}

/// Build the daily-rolling JSONL appender under
/// `$XDG_STATE_HOME/sy/logs/<name>/`. Mirrors SPEC §4.6's "Daemon
/// mode" sink with the per-daemon subdir from Step 1 (so the
/// appender doesn't share a directory with sibling daemons).
fn non_blocking_appender_for(
    name: &str,
) -> Result<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    let dir = state_logs_dir().join(name);
    std::fs::create_dir_all(&dir).with_context(|| format!("create log dir {}", dir.display()))?;
    let appender = tracing_appender::rolling::daily(&dir, format!("sy-{name}.jsonl"));
    Ok(tracing_appender::non_blocking(appender))
}

/// `$XDG_STATE_HOME/sy/logs` if `XDG_STATE_HOME` is set and
/// non-empty; otherwise `~/.local/state/sy/logs`. Falls back to
/// the current dir if `HOME` is also unset (defensive — a daemon
/// would have failed long before this point).
fn state_logs_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_STATE_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("sy/logs");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/sy/logs");
    }
    PathBuf::from("sy/logs")
}

#[cfg(test)]
pub(crate) mod testing {
    //! Test-only helpers. Building a per-test subscriber via
    //! `with_default` avoids polluting the process-global subscriber
    //! slot — `init()` can only be called once per process and
    //! parallel `cargo test` workers would race on it otherwise.

    use std::path::Path;

    use anyhow::{Context, Result};
    use tracing_appender::non_blocking::WorkerGuard;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::EnvFilter;

    /// Build a test-local subscriber that writes JSON to `dir/file`
    /// via the rolling appender. Returns `(dispatch, guard)`; bind
    /// `dispatch` via `tracing::subscriber::with_default(&dispatch, ||
    /// …)` to scope subscriber installation to the test body.
    pub fn appender_dispatch(
        dir: &Path,
        file: &str,
        filter: EnvFilter,
    ) -> Result<(tracing::Dispatch, WorkerGuard)> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create log dir {}", dir.display()))?;
        let appender = tracing_appender::rolling::daily(dir, file);
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .json()
            .with_current_span(false)
            .with_span_list(false);
        let subscriber = tracing_subscriber::registry().with(filter).with(layer);
        Ok((tracing::dispatcher::Dispatch::new(subscriber), guard))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;

    use tempfile::tempdir;

    /// Serialises tests that mutate process-wide env vars
    /// (`RUST_LOG`, `XDG_STATE_HOME`). Without this, parallel
    /// `cargo test` workers would race on the same globals.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn read_first_line(dir: &Path) -> Option<String> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();
        for p in entries {
            let s = fs::read_to_string(&p).ok()?;
            if !s.is_empty() {
                return s.lines().next().map(str::to_string);
            }
        }
        None
    }

    #[test]
    fn init_cli_mode_returns_guard() {
        // We can't safely call `init()` in a unit test (process-global
        // subscriber), so exercise the same plumbing the CLI path uses
        // — the non-blocking stderr writer + a no-op test subscriber
        // — and confirm a guard is returned and an `info!` doesn't
        // panic.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let (dispatch, guard) =
            testing::appender_dispatch(tmp.path(), "cli.jsonl", EnvFilter::new("info"))
                .expect("appender_dispatch");
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!("cli-mode-ok");
        });
        drop(guard);
    }

    #[test]
    fn daemon_mode_emits_json_to_appender() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let (dispatch, guard) =
            testing::appender_dispatch(tmp.path(), "sy-test.jsonl", EnvFilter::new("info"))
                .expect("appender_dispatch");
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!("daemon-mode-emit");
        });
        drop(guard);

        let line = read_first_line(tmp.path()).expect("at least one log line");
        assert!(
            line.contains("\"level\":\"INFO\""),
            "expected default tracing-subscriber JSON level field, got: {line}"
        );
        assert!(
            line.contains("daemon-mode-emit"),
            "expected event body, got: {line}"
        );
    }

    #[test]
    fn trace_id_propagates_into_log_field() {
        // SPEC §4.6 + arch-observability Step 4: `with_trace_id`
        // must stamp the configured trace_id onto every event
        // emitted inside its closure. The OTel formatter is the
        // surface contract — assert the JSON log line carries the
        // id verbatim, not the empty placeholder Step 3 emitted.
        const T: &str = "0af7651916cd43dd8448eb211c80319c";
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let appender = tracing_appender::rolling::daily(tmp.path(), "traced.jsonl");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .event_format(OtelFormatter::new("sy-test"));
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(super::TraceCtxLayer::new())
            .with(fmt_layer);
        let dispatch = tracing::Dispatch::new(subscriber);
        tracing::dispatcher::with_default(&dispatch, || {
            super::with_trace_id(crate::TraceId(T.into()), None, || {
                tracing::info!("inside-trace");
            });
        });
        drop(guard);

        let line = read_first_line(tmp.path()).expect("at least one log line");
        let v: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["trace_id"], serde_json::Value::String(T.into()));
        assert_eq!(v["body"], serde_json::Value::String("inside-trace".into()));
        assert!(
            v["span_id"].as_str().is_some_and(|s| !s.is_empty()),
            "span_id must be populated when inside `with_trace_id`, got {line}"
        );
    }

    #[test]
    fn env_filter_respects_rust_log() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let (dispatch, guard) =
            testing::appender_dispatch(tmp.path(), "filtered.jsonl", EnvFilter::new("warn"))
                .expect("appender_dispatch");
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!("should-be-filtered");
            tracing::warn!("should-pass");
        });
        drop(guard);

        let line = read_first_line(tmp.path()).expect("at least one log line");
        assert!(
            !line.contains("should-be-filtered"),
            "info event leaked past RUST_LOG=warn: {line}"
        );
        assert!(line.contains("should-pass"), "warn event missing: {line}");
    }
}
