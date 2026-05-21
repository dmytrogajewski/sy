//! 14-day onboarding gate per SPEC §2 + §4.
//!
//! Apple's Optimized Battery Charging requires "≥14 days of data and
//! learned routines" before any ML decision lands. We mirror that: the
//! daemon's bandit is held at the rules-baseline for the configured
//! window. `SY_POWER_ONBOARDING_DAYS` (Step 1, plumbed through
//! [`crate::power::config::OnboardingConfig`]) shortens the window for
//! dev / bench.
//!
//! [`compute_onboarding_status`] is the single source of truth: it
//! sorts the `telemetry-YYYY-MM-DD.ndjson` files under the state-root
//! lexicographically (= chronological), opens the OLDEST file's first
//! line, deserialises it as `AuditEntry`, and reads `snapshot.ts` as
//! the onboarding anchor. The mtime of the oldest file is used only
//! when the primary path can't read a `ts` — the file is empty or its
//! first line is corrupt. The historical mtime-only path was wrong:
//! rotation and daemon restarts both bump the mtime to "today" while
//! the entry's `ts` correctly carries the day the line was written.
//! Step P1-2 (`sy-power-production`) closes the gap. When no NDJSON
//! exists yet, `days_collected = 0` and `ready_at = now + days_cfg
//! days` so the operator sees a sensible countdown on
//! `sy power status --json`.
//!
//! The function is pure with respect to its arguments — `state_root`
//! is a path the caller supplies, `clock` is a `&dyn Clock`. Tests
//! pass tempdirs + a [`MockClock`](crate::power::clock::MockClock) so
//! every assertion is hermetic.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::power::clock::Clock;
use crate::power::log::AuditEntry;

/// SPEC §4 `sy.power.status/v1` `onboarding` block. Surfaced verbatim
/// on `sy power status --json`; the waybar power tile reads the same
/// shape to render the "ML kicks in in X days" countdown promised by
/// SPEC §3 anti-goal #4 ("no black-box decisions").
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OnboardingStatus {
    /// `true` while the daemon is still inside the rules-only window.
    /// The bandit propose path is gated on this; while `active`, every
    /// tick applies the rules baseline and `model.version_sha` is
    /// pinned at `"rules-baseline"`.
    pub active: bool,
    /// Whole days observed since the oldest NDJSON file was created.
    /// `0` when no telemetry exists yet — the count starts ticking
    /// the day the daemon first writes an audit line.
    pub days_collected: u32,
    /// Wall-clock instant at which `active` flips to `false`. Computed
    /// as `oldest_mtime + days_cfg`; when no telemetry exists,
    /// `now + days_cfg` so the operator sees a real future date.
    pub ready_at: DateTime<Utc>,
}

/// Compute the onboarding status from the state-root + the configured
/// window. The state-root is the directory the audit logger writes
/// `telemetry-YYYY-MM-DD.ndjson` files into (per
/// [`crate::power::power_state_dir_for_daemon`]).
///
/// Primary signal (Step P1-2): the oldest file by filename is opened,
/// the first line is deserialised as `AuditEntry`, and `snapshot.ts`
/// is taken as the anchor. Fallback: when the first line is missing
/// or fails to parse, the file's mtime is used. Negative `(now -
/// anchor)` deltas (clock skew) clamp to zero so `days_collected`
/// never goes negative.
pub fn compute_onboarding_status(
    state_root: &Path,
    clock: &dyn Clock,
    days_cfg: u32,
) -> OnboardingStatus {
    let now = clock.now();
    let window = Duration::days(days_cfg as i64);
    let oldest =
        oldest_ndjson_entry_ts(state_root).or_else(|| oldest_ndjson_mtime_fallback(state_root));
    let Some(oldest) = oldest else {
        return OnboardingStatus {
            active: true,
            days_collected: 0,
            ready_at: now + window,
        };
    };
    let elapsed = (now - oldest).max(Duration::zero());
    let days_collected = elapsed.num_days().max(0) as u32;
    OnboardingStatus {
        active: days_collected < days_cfg,
        days_collected,
        ready_at: oldest + window,
    }
}

/// Primary onboarding anchor (Step P1-2): walk
/// `telemetry-YYYY-MM-DD.ndjson` files under `state_root`, sort by
/// filename ascending (= chronological — `%Y-%m-%d` is lexicographic),
/// open the OLDEST file, read its FIRST line, deserialise as
/// `AuditEntry`, and return `entry.snapshot.ts`. Returns `None` when
/// the oldest file is missing, empty, or the first line fails to
/// deserialise — the caller falls back to the file-mtime anchor.
fn oldest_ndjson_entry_ts(state_root: &Path) -> Option<DateTime<Utc>> {
    let oldest_path = oldest_ndjson_path(state_root)?;
    let f = File::open(&oldest_path).ok()?;
    let first_line = BufReader::new(f).lines().next()?.ok()?;
    let entry: AuditEntry = serde_json::from_str(&first_line).ok()?;
    Some(entry.snapshot.ts)
}

/// Lowest `telemetry-YYYY-MM-DD.ndjson` filename under `state_root`,
/// sorted lexicographically (= chronological). Returns `None` when
/// the directory is missing or contains no telemetry file. Helper
/// shared by [`oldest_ndjson_entry_ts`] and
/// [`oldest_ndjson_mtime_fallback`] so both agree on which file is
/// "oldest" — the entry-ts primary path and the mtime fallback must
/// always inspect the same file, else a corrupt newer file would
/// poison the fallback.
fn oldest_ndjson_path(state_root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(state_root).ok()?;
    let mut names: Vec<PathBuf> = Vec::new();
    for ent in entries.flatten() {
        let name = ent.file_name();
        let Some(s) = name.to_str() else { continue };
        if !s.starts_with("telemetry-") || !s.ends_with(".ndjson") {
            continue;
        }
        names.push(ent.path());
    }
    names.sort();
    names.into_iter().next()
}

/// Mtime fallback (Step P1-2): when the oldest NDJSON file is empty
/// or carries a first line that fails to deserialise as `AuditEntry`,
/// the caller falls back to the file's mtime as the onboarding
/// anchor. Inspects the same "oldest by filename" file as the primary
/// path so the two stay in sync. Returns `None` when the directory is
/// missing or no telemetry file exists.
fn oldest_ndjson_mtime_fallback(state_root: &Path) -> Option<DateTime<Utc>> {
    let path = oldest_ndjson_path(state_root)?;
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    Some(modified.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::clock::MockClock;
    use std::fs;
    use std::time::SystemTime;
    use tempfile::TempDir;

    /// Lay down one NDJSON file under `root`. The mtime is "now" by
    /// construction (real filesystem write); the test then constructs
    /// a `MockClock` `days_ahead` in the future to simulate the
    /// `days_collected` window without touching the filesystem mtime.
    fn seed_ndjson(root: &Path, day: &str) {
        let path = root.join(format!("telemetry-{day}.ndjson"));
        fs::write(&path, b"{}\n").expect("write ndjson");
    }

    /// Read back the freshly-written NDJSON's mtime as the anchor
    /// for the mock clock — the test then advances the clock from
    /// that anchor instead of trying to set the file mtime (which
    /// would require `filetime`, a new cargo dep that's banned).
    fn file_mtime(root: &Path, day: &str) -> DateTime<Utc> {
        let path = root.join(format!("telemetry-{day}.ndjson"));
        let modified: SystemTime = fs::metadata(&path)
            .expect("stat ndjson")
            .modified()
            .expect("mtime supported");
        modified.into()
    }

    /// SPEC §3 default — 14-day rules-only window; with the oldest
    /// NDJSON 3 days old, `days_collected = 3` and `active = true`.
    #[test]
    fn active_for_first_14_days() {
        const DAYS_CFG: u32 = 14;
        const OBSERVED_DAYS: i64 = 3;
        let td = TempDir::new().expect("tempdir");
        let root = td.path();
        seed_ndjson(root, "2026-05-17");
        let anchor = file_mtime(root, "2026-05-17");
        let clock = MockClock::new(anchor + Duration::days(OBSERVED_DAYS));
        let status = compute_onboarding_status(root, &clock, DAYS_CFG);
        assert!(status.active, "still active inside 14d window");
        assert_eq!(status.days_collected, OBSERVED_DAYS as u32);
        let expected_ready = anchor + Duration::days(DAYS_CFG as i64);
        assert_eq!(status.ready_at, expected_ready);
    }

    /// `SY_POWER_ONBOARDING_DAYS=3` shortens the window — 4 days of
    /// telemetry flips `active` to `false`. The env override is
    /// applied by `PowerConfig::load`; this test exercises only
    /// `compute_onboarding_status`, so it passes `days_cfg = 3`
    /// directly to mirror what `cfg.onboarding.days` would carry.
    #[test]
    fn env_override_shortens_window() {
        const DAYS_CFG: u32 = 3;
        const OBSERVED_DAYS: i64 = 4;
        let td = TempDir::new().expect("tempdir");
        let root = td.path();
        seed_ndjson(root, "2026-05-16");
        let anchor = file_mtime(root, "2026-05-16");
        let clock = MockClock::new(anchor + Duration::days(OBSERVED_DAYS));
        let status = compute_onboarding_status(root, &clock, DAYS_CFG);
        assert!(!status.active, "4d collected ≥ 3d cfg ⇒ inactive");
        assert_eq!(status.days_collected, OBSERVED_DAYS as u32);
    }

    /// When no NDJSON exists yet, `days_collected = 0` and `active =
    /// true` — the operator sees a future `ready_at` countdown.
    #[test]
    fn empty_state_root_starts_window_now() {
        const DAYS_CFG: u32 = 14;
        let td = TempDir::new().expect("tempdir");
        let anchor = Utc::now();
        let clock = MockClock::new(anchor);
        let status = compute_onboarding_status(td.path(), &clock, DAYS_CFG);
        assert!(status.active);
        assert_eq!(status.days_collected, 0);
        assert_eq!(status.ready_at, anchor + Duration::days(DAYS_CFG as i64));
    }

    /// Build a valid first-line `AuditEntry` whose `snapshot.ts` is
    /// `ts`. Used to seed onboarding fixtures that exercise the
    /// primary "read oldest entry's ts" path (P1-2). The rest of the
    /// audit-entry surface is left at its default shape — Step P1-2
    /// reads `snapshot.ts` only.
    fn write_ndjson_with_ts(root: &Path, day: &str, ts: DateTime<Utc>) {
        use crate::power::log::AuditEntry;
        use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
        let snap = Snapshot {
            schema: SCHEMA_ID,
            ts,
            features: [0.0_f32; FEATURE_LEN],
            raw: SnapshotRaw::default(),
            snapshot_hash: "0".repeat(64),
        };
        let entry = AuditEntry::r1(snap);
        let line = serde_json::to_string(&entry).expect("serialize audit entry");
        let path = root.join(format!("telemetry-{day}.ndjson"));
        fs::write(&path, format!("{line}\n")).expect("write ndjson");
    }

    /// Step P1-2 primary path: `days_collected` is computed from the
    /// OLDEST NDJSON entry's `snapshot.ts`, NOT from the file's mtime.
    /// Seed two files (3 days ago + today); clock is "now". Assert
    /// `days_collected == 3`, even though both files' mtimes are
    /// "today" (the test wrote them seconds ago).
    #[test]
    fn reads_days_collected_from_oldest_ndjson_entry_ts() {
        const DAYS_CFG: u32 = 14;
        const OBSERVED_DAYS: i64 = 3;
        let td = TempDir::new().expect("tempdir");
        let root = td.path();
        let now = Utc::now();
        let three_days_ago = now - Duration::days(OBSERVED_DAYS);
        write_ndjson_with_ts(root, "2026-05-18", three_days_ago);
        write_ndjson_with_ts(root, "2026-05-21", now);
        let clock = MockClock::new(now);
        let status = compute_onboarding_status(root, &clock, DAYS_CFG);
        assert_eq!(
            status.days_collected, OBSERVED_DAYS as u32,
            "days_collected must read oldest entry's snapshot.ts",
        );
        assert!(status.active, "still inside 14d window");
    }

    /// Step P1-2 fallback path: when the oldest NDJSON file is empty
    /// (no first line to deserialise), `compute_onboarding_status`
    /// falls back to the file's mtime as the onboarding anchor. The
    /// fixture writes an empty file, reads back its mtime as the
    /// anchor, and advances the mock clock 5 days from that anchor —
    /// asserting `days_collected == 5`.
    #[test]
    fn falls_back_to_mtime_when_oldest_file_empty() {
        const DAYS_CFG: u32 = 14;
        const OBSERVED_DAYS: i64 = 5;
        let td = TempDir::new().expect("tempdir");
        let root = td.path();
        let path = root.join("telemetry-2026-05-16.ndjson");
        fs::write(&path, b"").expect("write empty ndjson");
        let anchor = file_mtime(root, "2026-05-16");
        let clock = MockClock::new(anchor + Duration::days(OBSERVED_DAYS));
        let status = compute_onboarding_status(root, &clock, DAYS_CFG);
        assert_eq!(
            status.days_collected, OBSERVED_DAYS as u32,
            "mtime fallback must drive days_collected when first line is missing",
        );
    }

    /// Step P1-2 rotation safety: when the retention sweep (Step 9)
    /// deletes the original file and the oldest surviving file is
    /// younger than the daemon's first run, `compute_onboarding_status`
    /// must still return a finite, non-negative number. With a single
    /// surviving file dated 2 days ago, `days_collected == 2`. The
    /// roadmap risks-note (Step P1-2) documents that this plateau at
    /// retention_days is the intended onboarding-gate bound.
    #[test]
    fn handles_rotated_state_where_oldest_file_younger_than_first_run() {
        const DAYS_CFG: u32 = 14;
        const OBSERVED_DAYS: i64 = 2;
        let td = TempDir::new().expect("tempdir");
        let root = td.path();
        let now = Utc::now();
        let two_days_ago = now - Duration::days(OBSERVED_DAYS);
        write_ndjson_with_ts(root, "2026-05-19", two_days_ago);
        let clock = MockClock::new(now);
        let status = compute_onboarding_status(root, &clock, DAYS_CFG);
        assert_eq!(
            status.days_collected, OBSERVED_DAYS as u32,
            "post-rotation: days_collected anchors on the surviving oldest entry",
        );
        assert!(
            status.active,
            "two days of telemetry stays inside the window"
        );
    }
}
