//! `sy crash show <ts>` — drill into a single crash record. SPEC §4.6
//! / arch-observability Step 6.
//!
//! Lookup strategy:
//!
//! 1. If `ts` matches the `ts` field of a JSONL record under
//!    [`sy_core::obs::panic::crash_dir`], render that record.
//! 2. Otherwise, shell out to `coredumpctl info <ts>` and print its
//!    output verbatim. A non-zero exit there ⇒ exit 4 ("not found").
//!
//! Exit codes (SPEC §4.7): 0 ok, 1 generic error, 4 not found.

use std::process::Command;

use anyhow::Result;

use super::{find_panic_by_ts, EXIT_NOT_FOUND};

/// Render one crash record by its `ts`. Drives the `sy crash show`
/// dispatch arm; on "not found" it calls `std::process::exit(4)` so
/// callers don't need to translate `Result` → exit code.
pub fn run(ts: String, as_json: bool) -> Result<()> {
    let dir = sy_core::obs::panic::crash_dir();
    if let Some(path) = find_panic_by_ts(&dir, &ts) {
        let raw = std::fs::read_to_string(&path)?;
        if as_json {
            // The on-disk record is already canonical JSON; emit it
            // pretty-printed so jq | less is legible.
            let v: serde_json::Value = serde_json::from_str(&raw)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            print_human_panic(&raw)?;
        }
        return Ok(());
    }
    // Fall through to coredumpctl. `coredumpctl info <ts>` accepts a
    // timestamp directly. Missing binary or non-zero exit ⇒ not found.
    if let Some(output) = try_coredumpctl_info(&ts) {
        print!("{output}");
        return Ok(());
    }
    eprintln!("error: no crash record matching ts={ts}");
    std::process::exit(EXIT_NOT_FOUND);
}

fn print_human_panic(raw: &str) -> Result<()> {
    let v: serde_json::Value = serde_json::from_str(raw)?;
    let ts = v.get("ts").and_then(|s| s.as_str()).unwrap_or("?");
    let pid = v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0);
    let thread = v.get("thread").and_then(|s| s.as_str()).unwrap_or("?");
    let payload = v.get("payload").and_then(|s| s.as_str()).unwrap_or("?");
    let location = v.get("location").and_then(|s| s.as_str()).unwrap_or("?");
    let trace_id = v.get("trace_id").and_then(|s| s.as_str()).unwrap_or("");
    println!("ts:       {ts}");
    println!("pid:      {pid}");
    println!("thread:   {thread}");
    println!("location: {location}");
    if !trace_id.is_empty() {
        println!("trace_id: {trace_id}");
    }
    println!("payload:  {payload}");
    Ok(())
}

fn try_coredumpctl_info(ts: &str) -> Option<String> {
    let out = Command::new("coredumpctl")
        .args(["info", ts])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const UNKNOWN_TS: &str = "9999-01-01T00:00:00.000Z";

    /// Test-internal variant of `run`: doesn't shell out to
    /// `coredumpctl` (the test host might have cores!), and returns
    /// an exit-code rather than calling `std::process::exit`. The
    /// production code path is the contract: panic-not-found → 4.
    fn lookup_exit_code(dir: &std::path::Path, ts: &str) -> i32 {
        if find_panic_by_ts(dir, ts).is_some() {
            0
        } else {
            EXIT_NOT_FOUND
        }
    }

    #[test]
    fn show_unknown_ts_exits_4() {
        // SPEC §4.7: exit 4 is "not ready / not found". A `show <ts>`
        // for a ts that matches no record on disk maps here.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path().join("sy/crash");
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert_eq!(lookup_exit_code(&dir, UNKNOWN_TS), EXIT_NOT_FOUND);
    }

    #[test]
    fn show_known_ts_returns_zero() {
        // Sanity-companion to `show_unknown_ts_exits_4`: a record we
        // wrote ourselves resolves to a zero exit.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempdir().expect("tempdir");
        let dir = tmp.path().join("sy/crash");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let rec = serde_json::json!({
            "v": 1,
            "ts": "2026-05-17T10:00:00.000Z",
            "pid": 42,
            "thread": "main",
            "payload": "boom",
            "location": "src/x.rs:7",
            "trace_id": "",
            "span_trace": null,
        });
        std::fs::write(
            dir.join("rec.json"),
            serde_json::to_vec(&rec).expect("serialise"),
        )
        .expect("write");
        assert_eq!(lookup_exit_code(&dir, "2026-05-17T10:00:00.000Z"), 0);
    }
}
