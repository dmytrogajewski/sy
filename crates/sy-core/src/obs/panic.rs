//! Panic hook + crash JSONL writer. SPEC §4.6 "Crash records" /
//! arch-observability Step 6.
//!
//! `install_panic_hook` registers a `std::panic::set_hook` that:
//! 1. Emits a `tracing::error!` with `target = "sy::panic"` and
//!    structured fields (`payload`, `location`, `thread`, `pid`,
//!    `trace_id`).
//! 2. Writes a single-line JSON crash record to
//!    `$XDG_STATE_HOME/sy/crash/<rfc3339-nanos>-<pid>.json`.
//!
//! The hook is `std::panic::PanicHookInfo`-driven so it works on every
//! thread that panics, not just `main`. Filename uses nanos precision
//! to avoid collisions if two panics fire in the same millisecond.
//!
//! `tracing-error::SpanTrace` capture is deliberately not pulled in at
//! this step — the dep is heavy and `ErrorLayer` isn't installed in
//! the Registry. The record's `span_trace` field is always `null`; a
//! future step can flip the dep on and populate it without breaking
//! consumers (the field is documented as optional).
//!
//! Safety: panics inside the hook are catastrophic (they re-enter the
//! panic machinery, often hanging the process). Every fallible
//! operation here is wrapped in `let _ = …` — a failed write is
//! preferable to a recursive panic.

use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};

use crate::trace::TraceId;

/// SPEC §4.6 record-schema version. Bumped on breaking changes.
const RECORD_VERSION: u32 = 1;

/// Build the crash-record JSON for a panic. Public-test surface: the
/// unit test calls this directly with a synthetic `PanicHookInfo` to
/// avoid forking a child process inside `cargo test`.
pub(crate) fn build_record(
    info: &PanicHookInfo<'_>,
    pid: u32,
    thread: &str,
    trace_id: Option<&TraceId>,
) -> serde_json::Value {
    let payload = panic_payload(info);
    let location = info
        .location()
        .map(|l| format!("{}:{}", l.file(), l.line()))
        .unwrap_or_else(|| "unknown".to_string());
    serde_json::json!({
        "v": RECORD_VERSION,
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "pid": pid,
        "thread": thread,
        "payload": payload,
        "location": location,
        "trace_id": trace_id.map(|t| t.as_str()).unwrap_or(""),
        // tracing-error is not yet wired into the Registry; the slot
        // is reserved so a later step can populate it without bumping
        // `v`. Consumers (`sy crash show`) must tolerate `null`.
        "span_trace": serde_json::Value::Null,
    })
}

/// Pull the panic message out of `PanicHookInfo`. Mirrors the
/// downcasts the default hook does — `&'static str` first, then
/// `String`, falling back to a placeholder.
fn panic_payload(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "Box<dyn Any>".to_string()
}

/// Write a crash record to `dir/<ts>-<pid>.json`. Returns the path
/// written or `None` on any I/O failure — the panic hook must never
/// itself panic, so this swallows errors.
pub(crate) fn write_crash_record(dir: &Path, record: &serde_json::Value) -> Option<PathBuf> {
    let _ = std::fs::create_dir_all(dir);
    let ts = record.get("ts").and_then(|v| v.as_str()).unwrap_or("ts");
    let pid = record.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
    // `:` is fs-safe on Linux but the RFC3339 timestamp also carries
    // a `+` for the tz offset; strip neither — keep the filename
    // identical to the record's `ts` for greppability.
    let nanos = chrono::Utc::now().timestamp_subsec_nanos();
    let name = format!("{ts}-{pid}-{nanos:09}.json");
    let path = dir.join(name);
    let bytes = match serde_json::to_vec(record) {
        Ok(b) => b,
        Err(_) => return None,
    };
    if std::fs::write(&path, bytes).is_err() {
        return None;
    }
    Some(path)
}

/// `$XDG_STATE_HOME/sy/crash`, falling back to `~/.local/state/sy/crash`.
/// Mirrors `state_logs_dir` in `obs::mod` so logs and crash records
/// share the same root.
pub fn crash_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_STATE_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("sy/crash");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/sy/crash");
    }
    PathBuf::from("sy/crash")
}

/// Install a process-global panic hook (idempotent against repeat
/// installs — `std::panic::set_hook` replaces the previous one). The
/// hook logs via `tracing::error!` and writes a JSONL record under
/// [`crash_dir`]. Call this once, after `obs::init`, in every binary.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Always run the default hook first so stderr still gets the
        // standard "thread 'main' panicked at …" line.
        default(info);
        let pid = std::process::id();
        let thread_handle = std::thread::current();
        let thread_name = thread_handle.name().unwrap_or("unnamed");
        let trace_ctx = crate::obs::current_trace_ctx();
        let trace_id = trace_ctx.as_ref().map(|c| &c.trace_id);
        let record = build_record(info, pid, thread_name, trace_id);
        tracing::error!(
            target: "sy::panic",
            payload = %record["payload"].as_str().unwrap_or(""),
            location = %record["location"].as_str().unwrap_or(""),
            thread = %thread_name,
            pid = pid,
            "panicked",
        );
        let _ = write_crash_record(&crash_dir(), &record);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use tempfile::tempdir;

    /// `XDG_STATE_HOME` is process-global; serialise mutations with
    /// the same `ENV_LOCK` pattern Step 1 uses in `obs/mod.rs`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const TEST_PID: u32 = 4242;
    const TEST_THREAD: &str = "panic-test";
    const TEST_TRACE: &str = "0af7651916cd43dd8448eb211c80319c";

    #[test]
    fn build_record_carries_required_fields() {
        // The record schema is the SPEC §4.6 contract: `v`, `ts`,
        // `pid`, `thread`, `payload`, `location`, `trace_id`,
        // `span_trace`. Lock the shape down so future drift breaks
        // here rather than in `sy crash show`.
        // `PanicHookInfo` is `#[non_exhaustive]` and can't be built
        // directly — the test fires a real panic inside
        // `catch_unwind` to obtain one via the panic hook plumbing.
        // The hook is process-global; serialise with `ENV_LOCK` so
        // the test doesn't race the sibling `panic_hook_writes_...`
        // test.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let trace = TraceId(TEST_TRACE.into());
        // Drive `build_record` through a real panic so the
        // `PanicHookInfo` is genuine.
        let captured = std::sync::Arc::new(Mutex::new(None::<serde_json::Value>));
        let captured_clone = captured.clone();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let rec = build_record(info, TEST_PID, TEST_THREAD, Some(&trace));
            *captured_clone.lock().expect("mutex") = Some(rec);
        }));
        let _ = std::panic::catch_unwind(|| panic!("boom"));
        std::panic::set_hook(prev);
        let rec = captured.lock().expect("mutex").take().expect("captured");
        assert_eq!(rec["v"], 1);
        assert_eq!(rec["pid"], TEST_PID);
        assert_eq!(rec["thread"], TEST_THREAD);
        assert_eq!(rec["payload"], "boom");
        assert_eq!(rec["trace_id"], TEST_TRACE);
        assert!(rec["location"].as_str().is_some_and(|l| l.contains(':')));
        assert!(rec["ts"].as_str().is_some());
        assert_eq!(rec["span_trace"], serde_json::Value::Null);
    }

    #[test]
    fn write_crash_record_creates_file_in_isolated_dir() {
        // Hermetic: tempdir stands in for `$XDG_STATE_HOME/sy/crash`.
        // After write, exactly one `.json` file appears, parses as
        // JSON, and re-reads the record we just wrote.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path().join("crash");
        let record = serde_json::json!({
            "v": 1,
            "ts": "2026-05-17T12:34:56.789Z",
            "pid": TEST_PID,
            "thread": TEST_THREAD,
            "payload": "boom",
            "location": "src/x.rs:7",
            "trace_id": TEST_TRACE,
            "span_trace": null,
        });
        let path = write_crash_record(&dir, &record).expect("path");
        assert!(path.starts_with(&dir));
        assert!(path.extension().is_some_and(|e| e == "json"));
        let raw = std::fs::read_to_string(&path).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(parsed["payload"], "boom");
        assert_eq!(parsed["pid"], TEST_PID);
        // Single-line JSON, not pretty-printed array — the filename
        // is the index, the file contains one record.
        assert_eq!(raw.lines().count(), 1);
    }

    #[test]
    fn panic_hook_writes_jsonl_under_xdg_state_home() {
        // Install the production hook, point `XDG_STATE_HOME` at a
        // tempdir, force a panic (via `catch_unwind` so the test
        // doesn't itself abort), and assert a `.json` file appears.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let prev_xdg = std::env::var_os("XDG_STATE_HOME");
        // Safety: ENV_LOCK serialises the env mutation; restore on
        // the way out.
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let prev_hook = std::panic::take_hook();
        install_panic_hook();
        let _ = std::panic::catch_unwind(|| panic!("forced-panic-for-hook-test"));
        std::panic::set_hook(prev_hook);

        let dir = tmp.path().join("sy/crash");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("crash dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !entries.is_empty(),
            "expected at least one crash record under {}",
            dir.display()
        );
        let path = entries[0].path();
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(
            raw.contains("forced-panic-for-hook-test"),
            "crash record missing payload: {raw}"
        );

        match prev_xdg {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }
}
