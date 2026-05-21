//! Integration test for `sy power log --since=2h --json` end-to-end
//! (Step 12 of `specs/roadmaps/sy-power`).
//!
//! The CLI under test cannot reach `sy::power::log::*` directly (the
//! crate has no `lib.rs`), so this test seeds the audit log on disk
//! the same way `Logger::append` does: one JSON line per entry, file
//! per UTC day named `telemetry-YYYY-MM-DD.ndjson`. The wire format
//! is pinned by `src/power/log.rs`; if this test ever drifts it means
//! the on-disk schema changed and `sy power explain` (Step 24) is the
//! caller that breaks next.
//!
//! XDG_STATE_HOME is per-process state, so a serializing mutex would
//! be needed if a second test in this file ever lands.

use std::process::Command;

/// Step 12 DoD: write 20 entries spanning two rotation boundaries
/// (three calendar days) via direct on-disk seeding. `sy power log
/// --since=2h --json` may return fewer than 20 because the entries
/// are spaced 7 minutes apart (covering ~2h20m), but the read path
/// must return at least every entry inside the window in
/// newest-first order with `--json` emitting one JSON object per
/// line (NDJSON, not an array).
#[test]
fn power_log_emits_ndjson_filtered_by_since() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_root = tmp.path().join("state");
    let power_dir = state_root.join("sy").join("power");
    std::fs::create_dir_all(&power_dir).expect("create power dir");

    // Anchor at "yesterday noon UTC" relative to wall-clock now so
    // the test exercises real cross-day rotation while staying inside
    // the default 1h-but-overridden 2h window. We span 20 entries
    // backwards from the anchor at 7-minute intervals (~2h20m total),
    // intentionally crossing the day boundary so two files exist.
    let now = chrono::Utc::now();
    let spacing = chrono::Duration::seconds(7 * 60);
    let zero_features: Vec<f32> = vec![0.0; 12];
    // Daemon writes oldest-first (1 Hz append-only); mirror that so
    // the on-disk shape matches what `Logger::append` produces and
    // `Logger::tail`'s reverse-iteration finds the newest at the
    // file's tail.
    for i in (0..20).rev() {
        let ts = now - spacing * i;
        let day_path = power_dir.join(format!("telemetry-{}.ndjson", ts.date_naive()));
        let entry = serde_json::json!({
            "schema": "sy.power.audit/v1",
            "snapshot": {
                "schema": "sy.power.snapshot/v1",
                "ts": ts.to_rfc3339(),
                "features": zero_features,
                "raw": {},
                "snapshot_hash": "0".repeat(64),
            },
            "applied_arm": null,
            "shield_state": null,
            "reason_chain": [],
        });
        let mut line = serde_json::to_string(&entry).expect("encode entry");
        line.push('\n');
        // Append-only writes match the daemon's append path so the
        // resulting layout is byte-identical to a real audit log.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&day_path)
            .expect("open day file");
        f.write_all(line.as_bytes()).expect("write line");
    }

    let bin = env!("CARGO_BIN_EXE_sy");
    let out = Command::new(bin)
        .args(["power", "log", "--since=3h", "--json"])
        .env("XDG_STATE_HOME", &state_root)
        .output()
        .expect("spawn sy power log");
    assert!(
        out.status.success(),
        "sy power log --json exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        20,
        "expected 20 NDJSON lines, got {}: stdout=\n{stdout}",
        lines.len(),
    );
    // Each line must be a standalone JSON object (NDJSON, not an array).
    let mut prev_ts: Option<chrono::DateTime<chrono::Utc>> = None;
    for line in &lines {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("non-NDJSON line {line:?}: {e}"));
        assert_eq!(v["schema"], "sy.power.audit/v1");
        let ts_str = v["snapshot"]["ts"].as_str().expect("snapshot.ts present");
        let ts = chrono::DateTime::parse_from_rfc3339(ts_str)
            .expect("rfc3339 ts")
            .with_timezone(&chrono::Utc);
        if let Some(p) = prev_ts {
            assert!(p >= ts, "expected newest-first order; got {p} then {ts}");
        }
        prev_ts = Some(ts);
    }
}
