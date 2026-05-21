//! Integration test for `sy power show` end-to-end (Step 35 of
//! `specs/roadmaps/sy-power`, Phase RV finale).
//!
//! Seeds 60 seconds of audit entries directly on disk (mirroring the
//! daemon's `Logger::append` path; we cannot link `sy::power::log::*`
//! from an integration test because the crate has no `lib.rs`), then
//! invokes `sy power show --since=2m --out=<tmp>/report.pdf
//! --no-open --allow-thin` and asserts the resulting file starts with
//! the `%PDF-` magic.
//!
//! `--allow-thin` is mandatory because 60 entries are well below the
//! Step 35 `MIN_ENTRIES_FOR_THICK_REPORT` floor (24 h ≈ 86 400
//! entries); the gate is the right default for production but would
//! otherwise wall this test off behind a 24-h sleep.
//!
//! XDG_STATE_HOME is per-process state, so a serialising mutex would
//! be needed if a second test in this file ever lands.

use std::process::Command;

/// Lower bound on a non-trivial PDF body. Mirrors the unit-test cap
/// in `src/power/report/render.rs`: 1.5 KB easily clears the catalog
/// + page tree + 7 pages of text overhead.
const MIN_PDF_BYTES: usize = 1_500;

/// Step 35 DoD: a 2-minute window of seeded NDJSON entries → a
/// well-formed PDF on disk under `--out`. The CLI must exit 0; the
/// file must exist and start with the `%PDF-` magic.
#[test]
fn power_show_writes_well_formed_pdf() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_root = tmp.path().join("state");
    let power_dir = state_root.join("sy").join("power");
    std::fs::create_dir_all(&power_dir).expect("create power dir");

    // 60 entries spaced 1 s apart ending at "now"; lands inside the
    // 2 m default window and matches the daemon's 1 Hz append rate.
    let now = chrono::Utc::now();
    let zero_features: Vec<f32> = vec![0.0; 12];
    for i in (0..60).rev() {
        let ts = now - chrono::Duration::seconds(i);
        let day_path = power_dir.join(format!("telemetry-{}.ndjson", ts.date_naive()));
        let entry = serde_json::json!({
            "schema": "sy.power.audit/v1",
            "snapshot": {
                "schema": "sy.power.snapshot/v1",
                "ts": ts.to_rfc3339(),
                "features": zero_features,
                "raw": {
                    "package_power_w": 8.0,
                    "activity_label": "Browse",
                },
                "snapshot_hash": "0".repeat(64),
            },
            "applied_arm": "browse",
            "shield_state": "COOL_AC",
            "reason_chain": [],
            "ranked_actions": [["browse", 0.5]],
            "conservative_alpha": 0.05,
        });
        let mut line = serde_json::to_string(&entry).expect("encode entry");
        line.push('\n');
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&day_path)
            .expect("open day file");
        f.write_all(line.as_bytes()).expect("write line");
    }

    let pdf_path = tmp.path().join("report.pdf");
    let bin = env!("CARGO_BIN_EXE_sy");
    let out = Command::new(bin)
        .args([
            "power",
            "show",
            "--since=2m",
            "--no-open",
            "--allow-thin",
            "--out",
        ])
        .arg(&pdf_path)
        .env("XDG_STATE_HOME", &state_root)
        .output()
        .expect("spawn sy power show");
    assert!(
        out.status.success(),
        "sy power show exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        pdf_path.exists(),
        "expected PDF at {} after sy power show",
        pdf_path.display(),
    );
    let bytes = std::fs::read(&pdf_path).expect("read pdf");
    assert!(
        bytes.starts_with(b"%PDF-"),
        "PDF must start with magic bytes, got first 8: {:?}",
        &bytes[..bytes.len().min(8)],
    );
    assert!(
        bytes.len() > MIN_PDF_BYTES,
        "PDF must be > {MIN_PDF_BYTES} bytes, got {}",
        bytes.len(),
    );
}
