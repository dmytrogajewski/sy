//! 14-day onboarding gate per SPEC §2 + §4.
//!
//! Apple's Optimized Battery Charging requires "≥14 days of data and
//! learned routines" before any ML decision lands. We mirror that: the
//! daemon's bandit is held at the rules-baseline for the configured
//! window. `SY_POWER_ONBOARDING_DAYS` (Step 1, plumbed through
//! [`crate::power::config::OnboardingConfig`]) shortens the window for
//! dev / bench.
//!
//! [`compute_onboarding_status`] is the single source of truth. It
//! resolves the onboarding anchor via [`resolve_anchor`]:
//!
//! 1. **Persisted anchor wins.** When the caller supplies a
//!    `first_telemetry_at` read from `checkpoint.json`, that instant
//!    is the anchor — full stop. This is the S3 deadlock fix: without
//!    it, `days_collected` was derived from the OLDEST *surviving*
//!    NDJSON entry, but the 7-day retention sweep deletes older
//!    telemetry, so the count plateaued around 7 while `target_days =
//!    14` — the day-14 gate was structurally unreachable
//!    (BUG-20260712-0139).
//! 2. **Fallback: oldest surviving NDJSON entry.** When no anchor is
//!    persisted yet (fresh host, or first run after the S3 fix), it
//!    sorts the `telemetry-YYYY-MM-DD.ndjson` files lexicographically
//!    (= chronological), opens the OLDEST file's first line,
//!    deserialises it as `AuditEntry`, and reads `snapshot.ts`. The
//!    daemon persists this derived anchor back into `checkpoint.json`
//!    so it stops sliding.
//! 3. **Fallback: oldest file mtime.** Used only when the oldest file
//!    is empty or its first line is corrupt. The historical mtime-only
//!    path was wrong on its own: rotation and daemon restarts both
//!    bump the mtime to "today" while the entry's `ts` correctly
//!    carries the day the line was written.
//!
//! When no anchor can be resolved (no telemetry exists yet),
//! `days_collected = 0` and `ready_at = now + days_cfg days` so the
//! operator sees a sensible countdown on `sy power status --json`.
//!
//! The function is pure with respect to its arguments — `state_root`
//! is a path the caller supplies, `clock` is a `&dyn Clock`. Tests
//! pass tempdirs + a `MockClock` so
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
/// `persisted_anchor` is the `first_telemetry_at` value read from
/// `checkpoint.json` (via [`crate::power::checkpoint::read_anchor`]),
/// or `None` when none has been persisted. See [`resolve_anchor`] for
/// the precedence. Negative `(now - anchor)` deltas (clock skew) clamp
/// to zero so `days_collected` never goes negative.
pub fn compute_onboarding_status(
    state_root: &Path,
    clock: &dyn Clock,
    days_cfg: u32,
    persisted_anchor: Option<DateTime<Utc>>,
) -> OnboardingStatus {
    let now = clock.now();
    let window = Duration::days(days_cfg as i64);
    let Some(anchor) = resolve_anchor(state_root, persisted_anchor) else {
        return OnboardingStatus {
            active: true,
            days_collected: 0,
            ready_at: now + window,
        };
    };
    let elapsed = (now - anchor).max(Duration::zero());
    let days_collected = elapsed.num_days().max(0) as u32;
    OnboardingStatus {
        active: days_collected < days_cfg,
        days_collected,
        ready_at: anchor + window,
    }
}

/// Resolve the onboarding anchor. Precedence (S3 / BUG-20260712-0139):
///
/// 1. `persisted_anchor` (`first_telemetry_at` from `checkpoint.json`)
///    when present — the retention-proof frozen anchor.
/// 2. Else the oldest surviving NDJSON entry's `snapshot.ts`.
/// 3. Else that file's mtime.
///
/// Returns `None` only when no anchor is persisted AND no telemetry
/// exists yet — the fresh-host, pre-first-write state. When it returns
/// a value derived from NDJSON (cases 2/3), the daemon persists that
/// value into `checkpoint.json` so subsequent ticks and restarts read
/// it as case 1 and it stops sliding under the retention sweep.
pub fn resolve_anchor(
    state_root: &Path,
    persisted_anchor: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    persisted_anchor.or_else(|| {
        oldest_ndjson_entry_ts(state_root).or_else(|| oldest_ndjson_mtime_fallback(state_root))
    })
}

/// Guard rail (S3): the NDJSON retention horizon must be at least the
/// onboarding window, else the retention sweep deletes telemetry
/// before the onboarding gate can trip. The persisted
/// `first_telemetry_at` anchor makes the deadlock non-fatal — the
/// anchor survives the sweep — but a retention shorter than the window
/// is still a config smell worth a loud startup line. Returns
/// `Some(message)` when `retention_days < onboarding_days`, else
/// `None`. The daemon logs the message at `warn`.
pub fn retention_guard(retention_days: u32, onboarding_days: u32) -> Option<String> {
    (retention_days < onboarding_days).then(|| {
        format!(
            "telemetry retention ({retention_days}d) is shorter than the onboarding window \
             ({onboarding_days}d): the retention sweep deletes older telemetry before the \
             onboarding gate can trip. The persisted first_telemetry_at anchor keeps \
             days_collected honest, but set retention_days >= onboarding.days to keep raw \
             telemetry available for the full onboarding period."
        )
    })
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
    use chrono::TimeZone;
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
        let status = compute_onboarding_status(root, &clock, DAYS_CFG, None);
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
        let status = compute_onboarding_status(root, &clock, DAYS_CFG, None);
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
        let status = compute_onboarding_status(td.path(), &clock, DAYS_CFG, None);
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
        let status = compute_onboarding_status(root, &clock, DAYS_CFG, None);
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
        let status = compute_onboarding_status(root, &clock, DAYS_CFG, None);
        assert_eq!(
            status.days_collected, OBSERVED_DAYS as u32,
            "mtime fallback must drive days_collected when first line is missing",
        );
    }

    /// Fallback-path rotation safety: with NO persisted anchor and the
    /// retention sweep (Step 9) having deleted the original file, the
    /// oldest surviving file is younger than the daemon's first run.
    /// `compute_onboarding_status` must still return a finite,
    /// non-negative number by anchoring on the surviving oldest entry.
    /// With a single surviving file dated 2 days ago, `days_collected
    /// == 2`. This is exactly the plateau the S3 persisted anchor
    /// prevents once one is written (see
    /// `persisted_anchor_survives_ndjson_deletion`).
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
        let status = compute_onboarding_status(root, &clock, DAYS_CFG, None);
        assert_eq!(
            status.days_collected, OBSERVED_DAYS as u32,
            "post-rotation: days_collected anchors on the surviving oldest entry",
        );
        assert!(
            status.active,
            "two days of telemetry stays inside the window"
        );
    }

    /// S3 core regression (BUG-20260712-0139): a persisted
    /// `first_telemetry_at` anchor drives `days_collected` even after
    /// the retention sweep has deleted every telemetry file older than
    /// the anchor. Simulate the deadlock: anchor is 10 days old, but
    /// the only surviving NDJSON entry is 2 days old (retention_days =
    /// 7 ate the rest). Without the anchor `days_collected` would read
    /// 2 (the plateau); WITH it, `days_collected == 10`.
    #[test]
    fn persisted_anchor_survives_ndjson_deletion() {
        const DAYS_CFG: u32 = 14;
        const ANCHOR_AGE: i64 = 10;
        const SURVIVING_AGE: i64 = 2;
        let td = TempDir::new().expect("tempdir");
        let root = td.path();
        let now = Utc::now();
        // Only a young file survives the sweep; the anchor predates it.
        write_ndjson_with_ts(root, "2026-07-10", now - Duration::days(SURVIVING_AGE));
        let anchor = now - Duration::days(ANCHOR_AGE);
        let clock = MockClock::new(now);
        let status = compute_onboarding_status(root, &clock, DAYS_CFG, Some(anchor));
        assert_eq!(
            status.days_collected, ANCHOR_AGE as u32,
            "persisted anchor must drive days_collected past the retention plateau",
        );
        assert!(status.active, "still inside the 14d window at day 10");
    }

    /// S3: the day-14 gate is now reachable. With a persisted anchor 14
    /// days old and the 14-day window, `active` flips to `false` — the
    /// structural deadlock is gone. Mirrors the deletion-proof anchor:
    /// no surviving NDJSON is needed at all.
    #[test]
    fn gate_trips_with_persisted_anchor_14_days_old() {
        const DAYS_CFG: u32 = 14;
        const ANCHOR_AGE: i64 = 14;
        let td = TempDir::new().expect("tempdir");
        let now = Utc::now();
        let anchor = now - Duration::days(ANCHOR_AGE);
        let clock = MockClock::new(now);
        let status = compute_onboarding_status(td.path(), &clock, DAYS_CFG, Some(anchor));
        assert_eq!(status.days_collected, ANCHOR_AGE as u32);
        assert!(
            !status.active,
            "14 days collected ≥ 14d window ⇒ onboarding complete",
        );
    }

    /// S3: [`resolve_anchor`] precedence — a persisted anchor wins over
    /// the oldest surviving NDJSON entry, and its absence falls through
    /// to the NDJSON-derived anchor (which the daemon then persists).
    #[test]
    fn resolve_anchor_prefers_persisted_over_ndjson() {
        let td = TempDir::new().expect("tempdir");
        let root = td.path();
        let ndjson_ts = Utc.with_ymd_and_hms(2026, 7, 7, 0, 0, 0).single().unwrap();
        write_ndjson_with_ts(root, "2026-07-07", ndjson_ts);
        let persisted = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).single().unwrap();
        assert_eq!(
            resolve_anchor(root, Some(persisted)),
            Some(persisted),
            "persisted anchor must win",
        );
        assert_eq!(
            resolve_anchor(root, None),
            Some(ndjson_ts),
            "absent persisted anchor falls through to the oldest NDJSON ts",
        );
    }

    /// S3 guard rail: [`retention_guard`] flags a retention horizon
    /// shorter than the onboarding window (the deadlock's structural
    /// precondition) and stays silent when retention >= window.
    #[test]
    fn retention_guard_flags_short_retention() {
        assert!(
            retention_guard(7, 14).is_some(),
            "7d retention < 14d window must warn",
        );
        assert!(
            retention_guard(14, 14).is_none(),
            "retention == window is fine",
        );
        assert!(
            retention_guard(30, 14).is_none(),
            "retention > window is fine",
        );
    }

    /// BUG-20260723-2210 follow-up: the production defaults must not
    /// trip the guard rail — a stock install should never boot into
    /// the "retention sweep starves the onboarding window" WARN the
    /// live host logged on every IPC tick.
    #[test]
    fn default_retention_covers_default_onboarding_window() {
        assert!(
            retention_guard(
                crate::power::log::DEFAULT_RETENTION_DAYS,
                crate::power::config::DEFAULT_ONBOARDING_DAYS,
            )
            .is_none(),
            "default retention must cover the default onboarding window",
        );
    }
}
