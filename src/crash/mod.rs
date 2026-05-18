//! `sy crash` — list and show panic records and native coredumps.
//! SPEC §4.6 "Crash records" / arch-observability Step 6.
//!
//! Two sources are merged into a single time-sorted listing:
//!
//! - Rust panic records under `$XDG_STATE_HOME/sy/crash/*.json`,
//!   written by [`sy_core::obs::panic`].
//! - Native cores via `coredumpctl list --json=pretty --since=-1day`
//!   (Fedora default). Missing `coredumpctl` is non-fatal — the
//!   listing degrades to JSONL-only.
//!
//! ## `sy crash list --json` schema (v1)
//!
//! ```json
//! [
//!   {
//!     "ts": "<rfc3339 with millis>",
//!     "pid": <u32>,
//!     "source": "panic" | "coredump",
//!     "payload_preview": "<truncated panic message, or signal/comm>"
//!   }
//! ]
//! ```
//!
//! Entries are sorted by `ts` ascending. `payload_preview` is at most
//! [`PREVIEW_MAX`] bytes so a one-shot `sy crash list` doesn't dump
//! megabytes of backtrace into the terminal.
//!
//! Exit codes (SPEC §4.7): 0 success, 1 generic I/O error, 4 the
//! requested `show <ts>` is not found.

pub mod show;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;

/// Truncate `payload_preview` to this many bytes so list output stays
/// terminal-friendly. Mirrors how `journalctl --no-pager` clamps
/// MESSAGE for tabular renderings.
pub const PREVIEW_MAX: usize = 160;

/// `sy crash show <unknown-ts>` exit (SPEC §4.7 "not ready / not
/// found"). Re-declared here so this module is self-contained;
/// `ipc_cli` owns the same constant for the IPC surface.
pub const EXIT_NOT_FOUND: i32 = 4;

/// clap surface for `sy crash`. `list` and `show <ts>` map to the two
/// behaviours operators need: a tabular overview and a single-record
/// drill-down.
#[derive(Debug, Subcommand)]
pub enum CrashCmd {
    /// List recent crash records (Rust panics + native cores merged
    /// by timestamp).
    List {
        /// Emit the SPEC §4.6 JSON array on stdout instead of the
        /// human table.
        #[arg(long)]
        json: bool,
    },
    /// Show one record by RFC3339 timestamp (the `ts` field from
    /// `sy crash list`). Exits 4 if the timestamp doesn't match any
    /// known record.
    Show {
        /// RFC3339 timestamp from `sy crash list`.
        ts: String,
        /// Emit the full record as JSON instead of the human view.
        #[arg(long)]
        json: bool,
    },
}

/// One row of `sy crash list`. Wire-shape is the v1 schema documented
/// in the module docstring; `payload_preview` is the only free-form
/// field and is bounded by [`PREVIEW_MAX`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CrashSummary {
    pub ts: String,
    pub pid: u32,
    pub source: &'static str,
    pub payload_preview: String,
}

/// Walk `$XDG_STATE_HOME/sy/crash/` for our JSONL records and shell
/// out to `coredumpctl` for the native side. Returns the merged,
/// time-sorted list.
pub fn list() -> Result<Vec<CrashSummary>> {
    let dir = sy_core::obs::panic::crash_dir();
    let coredumpctl = run_coredumpctl_list();
    list_with_inputs(&dir, coredumpctl.as_deref())
}

/// Pure variant used by tests. The `coredumpctl_output` parameter is
/// `Some(raw_json)` when `coredumpctl list --json=pretty` was
/// available, `None` when it failed or wasn't installed.
pub fn list_with_inputs(
    jsonl_dir: &Path,
    coredumpctl_output: Option<&[u8]>,
) -> Result<Vec<CrashSummary>> {
    let mut out = Vec::new();
    out.extend(read_panic_records(jsonl_dir));
    if let Some(raw) = coredumpctl_output {
        out.extend(parse_coredumpctl(raw));
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(out)
}

/// Read every `*.json` under `dir` as a panic record. Files that
/// don't parse or lack the required fields are skipped silently —
/// crash inventory is best-effort by design.
fn read_panic_records(dir: &Path) -> Vec<CrashSummary> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(summary) = read_one_panic(&path) {
            out.push(summary);
        }
    }
    out
}

fn read_one_panic(path: &Path) -> Option<CrashSummary> {
    let raw = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let ts = v.get("ts").and_then(|s| s.as_str())?.to_string();
    let pid = v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0) as u32;
    let payload = v.get("payload").and_then(|s| s.as_str()).unwrap_or("");
    Some(CrashSummary {
        ts,
        pid,
        source: "panic",
        payload_preview: truncate(payload),
    })
}

/// `coredumpctl list --json=pretty` returns an array of objects. We
/// pull `_TIMESTAMP` (µs since epoch) → RFC3339, `_PID`, and a
/// short `<COMM>: <SIGNAL>` preview. Older `coredumpctl` versions or
/// missing fields degrade to an empty slice rather than crashing.
fn parse_coredumpctl(raw: &[u8]) -> Vec<CrashSummary> {
    let v: serde_json::Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter().filter_map(parse_coredumpctl_entry).collect()
}

fn parse_coredumpctl_entry(entry: &serde_json::Value) -> Option<CrashSummary> {
    let ts_us = entry
        .get("_SOURCE_REALTIME_TIMESTAMP")
        .or_else(|| entry.get("_TIMESTAMP"))
        .and_then(|t| t.as_u64().or_else(|| t.as_str()?.parse::<u64>().ok()))?;
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(ts_us as i64)?;
    let ts = dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let pid = entry
        .get("_PID")
        .and_then(|p| p.as_u64().or_else(|| p.as_str()?.parse::<u64>().ok()))
        .unwrap_or(0) as u32;
    let comm = entry
        .get("_COMM")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");
    let signal = entry
        .get("COREDUMP_SIGNAL_NAME")
        .or_else(|| entry.get("COREDUMP_SIGNAL"))
        .and_then(|s| s.as_str())
        .unwrap_or("?");
    Some(CrashSummary {
        ts,
        pid,
        source: "coredump",
        payload_preview: truncate(&format!("{comm}: {signal}")),
    })
}

fn truncate(s: &str) -> String {
    if s.len() <= PREVIEW_MAX {
        return s.to_string();
    }
    // Byte-safe truncation at the last char boundary ≤ PREVIEW_MAX.
    let mut end = PREVIEW_MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push('…');
    out
}

/// Shell out to `coredumpctl list --json=pretty --since=-1day`.
/// Returns `Some(stdout)` on success, `None` if the binary is
/// missing or the call failed (the listing degrades to JSONL-only).
fn run_coredumpctl_list() -> Option<Vec<u8>> {
    let out = Command::new("coredumpctl")
        .args(["list", "--json=pretty", "--since", "-1day"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

/// `main.rs` entry: dispatch the clap subcommand. Exits with the
/// SPEC §4.7 code on the way out.
pub fn dispatch(cmd: CrashCmd) -> Result<()> {
    match cmd {
        CrashCmd::List { json } => run_list(json),
        CrashCmd::Show { ts, json } => show::run(ts, json),
    }
}

fn run_list(as_json: bool) -> Result<()> {
    let entries = list()?;
    if as_json {
        let s = serde_json::to_string_pretty(&entries)?;
        println!("{s}");
    } else if entries.is_empty() {
        println!("no recent crashes");
    } else {
        for e in &entries {
            println!(
                "{}  pid={:<7} {:<8} {}",
                e.ts, e.pid, e.source, e.payload_preview
            );
        }
    }
    Ok(())
}

/// Walk `dir` and return the path of the panic record whose embedded
/// `ts` field equals `ts`. Used by `sy crash show`.
pub fn find_panic_by_ts(dir: &Path, ts: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if v.get("ts").and_then(|s| s.as_str()) == Some(ts) {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const TS_A: &str = "2026-05-17T10:00:00.000Z";
    const TS_B: &str = "2026-05-17T11:30:00.000Z";
    const TS_C: &str = "2026-05-17T12:00:00.000Z";

    fn write_panic(dir: &Path, ts: &str, pid: u32, payload: &str) {
        std::fs::create_dir_all(dir).expect("mkdir");
        let rec = serde_json::json!({
            "v": 1,
            "ts": ts,
            "pid": pid,
            "thread": "main",
            "payload": payload,
            "location": "src/x.rs:1",
            "trace_id": "",
            "span_trace": null,
        });
        let path = dir.join(format!("{ts}-{pid}.json"));
        std::fs::write(&path, serde_json::to_vec(&rec).expect("serialise")).expect("write");
    }

    #[test]
    fn list_merges_jsonl_and_coredumpctl() {
        // Two JSONL records on disk + a coredumpctl array with one
        // entry. Expect a merged, time-sorted listing with the
        // correct sources tagged.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path().join("sy/crash");
        write_panic(&dir, TS_A, 100, "boom-a");
        write_panic(&dir, TS_C, 300, "boom-c");
        // Use chrono to compute the µs-since-epoch that re-renders to
        // `TS_B` — hand-computing the seconds is fragile (we did it
        // wrong once already; the unit test caught it).
        let parsed: chrono::DateTime<chrono::Utc> = chrono::DateTime::parse_from_rfc3339(TS_B)
            .expect("parse TS_B")
            .with_timezone(&chrono::Utc);
        let ts_us: u64 = parsed
            .timestamp_micros()
            .try_into()
            .expect("ts_us fits in u64");
        let coredumpctl = serde_json::json!([
            {
                "_SOURCE_REALTIME_TIMESTAMP": ts_us,
                "_PID": 222,
                "_COMM": "sy-aiplane",
                "COREDUMP_SIGNAL_NAME": "SIGSEGV"
            }
        ]);
        let raw = serde_json::to_vec(&coredumpctl).expect("serialise");
        let entries = list_with_inputs(&dir, Some(&raw)).expect("list");
        assert_eq!(entries.len(), 3, "expected 3 entries, got {entries:?}");
        assert_eq!(entries[0].ts, TS_A);
        assert_eq!(entries[0].source, "panic");
        assert_eq!(entries[1].ts, TS_B);
        assert_eq!(entries[1].source, "coredump");
        assert!(entries[1].payload_preview.contains("SIGSEGV"));
        assert_eq!(entries[2].ts, TS_C);
        assert_eq!(entries[2].source, "panic");
    }

    #[test]
    fn list_handles_missing_coredumpctl() {
        // `None` coredumpctl input must degrade cleanly to JSONL-only.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path().join("sy/crash");
        write_panic(&dir, TS_A, 100, "boom-a");
        let entries = list_with_inputs(&dir, None).expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "panic");
    }

    #[test]
    fn truncate_clamps_long_payloads() {
        let long = "x".repeat(PREVIEW_MAX + 50);
        let out = truncate(&long);
        assert!(
            out.chars().count() <= PREVIEW_MAX + 1,
            "truncated payload should be ≤ PREVIEW_MAX chars + the ellipsis"
        );
        assert!(out.ends_with('…'));
        // Short input is returned verbatim.
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn find_panic_by_ts_returns_matching_file() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path().join("sy/crash");
        write_panic(&dir, TS_A, 100, "boom-a");
        write_panic(&dir, TS_B, 200, "boom-b");
        let hit = find_panic_by_ts(&dir, TS_B).expect("found");
        let raw = std::fs::read_to_string(hit).expect("read");
        assert!(raw.contains("boom-b"));
        assert!(find_panic_by_ts(&dir, "1999-01-01T00:00:00.000Z").is_none());
    }
}
