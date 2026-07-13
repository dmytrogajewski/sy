//! NDJSON audit-log writer + daily rotation + size cap (Step 9).
//!
//! The audit log is the source of truth that every later step in the
//! `sy-power` roadmap depends on:
//!
//! * Step 10's daemon calls [`Logger::append`] once per 1 Hz tick.
//! * Step 12's `sy power log --since=…` reads back from these files.
//! * Step 23's `sy power explain` replays decisions from the same
//!   stream.
//! * Step 25's offline trainer consumes them as a training corpus.
//!
//! Format invariants (enforced by tests):
//!
//! * One JSON object per line, no pretty printing.
//! * Every line carries `"schema": "sy.power.audit/v1"` (regular
//!   entries via [`AuditEntry::schema`], rotation markers via the
//!   inline marker payload).
//! * Files are named `telemetry-YYYY-MM-DD.ndjson` under
//!   `~/.local/state/sy/power/`, one per UTC day, plus overflow
//!   segments `telemetry-YYYY-MM-DD.1.ndjson`, `.2`, … once the base
//!   file reaches the size cap.
//! * 7-day retention; older files (base *and* overflow segments) are
//!   deleted by [`Logger::rotate_retention`].
//! * Per-segment size cap ([`DEFAULT_MAX_SIZE_BYTES`]); when a segment
//!   fills, its tail receives a single `"marker":"rotated:size_cap"`
//!   line and the writer rolls to the next overflow segment so
//!   collection continues. The cap event is logged (WARN) once per UTC
//!   day, not once per tick.
//! * Refuses to write when the mountpoint's free space is below 1 GiB
//!   (`Err(LogError::OutOfSpace)`); the daemon surfaces this as a
//!   shield-state degradation in a later step.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::power::clock::Clock;
use crate::power::snapshot::Snapshot;

/// Versioned schema id stamped on every line of the audit log
/// (regular entries and rotation markers alike). Bumping this is a
/// breaking change to the audit replay (Step 23) and the offline
/// trainer (Step 25).
pub const SCHEMA_ID: &str = "sy.power.audit/v1";

/// Default hard cap per day. SPEC §4 "Migration & Compatibility".
///
/// Sized so 24 h × 1 Hz of audit entries fits with realistic headroom
/// for bursty reason chains and `top3` payloads. At ~700–1000 B/entry
/// (post BUG-20260522-0037 NPU reason trim), 86,400 entries occupy
/// ~58–82 MiB; 200 MiB clears the `cli::MIN_ENTRIES_FOR_THICK_REPORT`
/// threshold without the file capping mid-day. The previous 50 MiB
/// value capped the daily file at ~40K entries, making the 24-h
/// thick-report threshold unreachable.
pub const DEFAULT_MAX_SIZE_BYTES: u64 = 200 * 1024 * 1024;

/// Default retention horizon. SPEC §4 "Migration & Compatibility".
pub const DEFAULT_RETENTION_DAYS: u32 = 7;

/// Refuse to write when the mountpoint has less than this much free
/// space. SPEC §4 "Migration & Compatibility" — the daemon must never
/// be the process that fills the user's disk.
pub const MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

/// Default for the [`AuditEntry::schema`] field on deserialization.
/// The field is `&'static str` (pinned constant); on the read path
/// it is reconstituted from this default rather than borrowed from
/// the wire format, which is how the same struct can be both
/// `Serialize` and `Deserialize` without flipping `schema` to `String`.
fn default_audit_schema() -> &'static str {
    SCHEMA_ID
}

/// Deserialise `ranked_actions` tolerating a JSON `null` score.
/// A manual pin records the pinned arm's score as `f32::INFINITY`
/// (`daemon::one_tick`'s `pin:<arm>` branch); `serde_json` emits `null`
/// for any non-finite float, and the default `(String, f32)`
/// deserialiser rejects `null → f32`. Without this shim every pinned
/// audit line fails to parse — the tail reader silently skips it
/// (`sy power log --since=<short>` returns "no entries") and the IPC
/// `last_audit` decode fails (`sy power status --json` reports a healthy
/// daemon as "unreachable"). Each `null` score reconstitutes as
/// `f32::NAN`, mirroring the H1 feature-array shim in
/// [`crate::power::snapshot`]. See BUG-20260712-1137.
fn deserialize_ranked_null_as_nan<'de, D>(deserializer: D) -> Result<Vec<(String, f32)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<(String, Option<f32>)> = serde::Deserialize::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|(name, score)| (name, score.unwrap_or(f32::NAN)))
        .collect())
}

/// One audit-log line. Fields beyond `schema` + `snapshot` are
/// `Option<_>` / empty `Vec` in R1 so the schema is forward-compatible
/// with R2's shield (Step 17), R2's actuation (Step 19), and R3's
/// bandit reason chain (Step 22) without a v2 bump.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Always [`SCHEMA_ID`]. Field is required by readers (Step 12 +
    /// Step 23 + Step 25); a missing or mismatched schema is treated
    /// as a corrupt line. The pinned `&'static str` is reconstituted
    /// from [`default_audit_schema`] on deserialize.
    #[serde(default = "default_audit_schema", skip_deserializing)]
    pub schema: &'static str,
    /// The 1 Hz snapshot the decision (or no-op) was made against.
    pub snapshot: Snapshot,
    /// Arm that the daemon actually applied this tick. `None` in R1
    /// (no actuation); populated in R2 by Step 19.
    pub applied_arm: Option<String>,
    /// Shield-DFA state at decision time. `None` in R1 (no shield);
    /// populated in R2 by Step 17.
    pub shield_state: Option<String>,
    /// Bandit / shield / rules reason chain. Empty in R1; populated
    /// in R3 by Step 22.
    pub reason_chain: Vec<String>,
    /// Top-3 (arm name, UCB score) tuples from `bandit::Clucb::propose_ranked`.
    /// Populated in R3 (Step 22); empty for older NDJSON lines so the
    /// schema stays forward-compatible. The Step 22 `sy power status` +
    /// Step 23 `sy power explain` consume this field directly.
    #[serde(default, deserialize_with = "deserialize_ranked_null_as_nan")]
    pub ranked_actions: Vec<(String, f32)>,
    /// CLUCB conservative-α margin in force when this entry was written.
    /// Mirrors `cfg.bandit.alpha` at decision time. Surfaced on
    /// `sy power status`'s `bandit.conservative_alpha` slot.
    #[serde(default)]
    pub conservative_alpha: f32,
}

impl AuditEntry {
    /// Construct an R1-shape entry (no actuation, no shield, no
    /// bandit). Step 19 retired R1 from production — the daemon now
    /// builds entries with `applied_arm` / `shield_state` /
    /// `reason_chain` populated. The constructor stays as a
    /// test-only helper because `log.rs` + `cli.rs` regression tests
    /// rely on it to seed fixture entries that only exercise the
    /// snapshot field.
    #[cfg(test)]
    pub fn r1(snapshot: Snapshot) -> Self {
        Self {
            schema: SCHEMA_ID,
            snapshot,
            applied_arm: None,
            shield_state: None,
            reason_chain: Vec::new(),
            ranked_actions: Vec::new(),
            conservative_alpha: 0.0,
        }
    }

    /// Construct an R3-shape entry as written by the Step 22 daemon
    /// tick. Carries the bandit's top-3 ranked actions and the
    /// conservative-α margin in force at decision time. Older
    /// constructors (`r1`, ad-hoc struct literals in `cli`/`status`
    /// tests) keep working because both new fields are
    /// `#[serde(default)]`.
    pub fn r3(
        snapshot: Snapshot,
        applied_arm: String,
        shield_state: String,
        reason_chain: Vec<String>,
        ranked_actions: Vec<(String, f32)>,
        conservative_alpha: f32,
    ) -> Self {
        Self {
            schema: SCHEMA_ID,
            snapshot,
            applied_arm: Some(applied_arm),
            shield_state: Some(shield_state),
            reason_chain,
            ranked_actions,
            conservative_alpha,
        }
    }
}

/// Reasons [`Logger::append`] can refuse a write. Kept separate from
/// `anyhow::Error` so callers can dispatch on `OutOfSpace` (the
/// daemon surfaces it as `shield_state = BATTERY_LOW`-equivalent in
/// Step 17). The former `SizeCap` variant was retired by
/// BUG-20260712-*: a full segment now rolls over to an overflow file
/// rather than refusing the write, so `append` no longer returns a
/// cap error.
#[derive(Debug)]
pub enum LogError {
    /// Mountpoint has less than [`MIN_FREE_BYTES`] free. Nothing was
    /// written; the daemon should reduce its tick cadence or stop.
    OutOfSpace { free_bytes: u64 },
    /// I/O failure on disk: open / write / metadata. The string is
    /// the source error rendered via `to_string()` so the daemon's
    /// `tracing::warn!` stays one-line.
    Io(String),
    /// `serde_json::to_string` failed. In practice unreachable —
    /// [`Snapshot`] is a fixed shape with no `Serialize` impls that
    /// can error — but we surface it instead of `unwrap()`-ing so the
    /// daemon never panics on a bad line.
    Serialize(String),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfSpace { free_bytes } => write!(
                f,
                "audit log refusing to write: free space {free_bytes} < {MIN_FREE_BYTES} bytes"
            ),
            Self::Io(e) => write!(f, "audit log I/O error: {e}"),
            Self::Serialize(e) => write!(f, "audit log serialise error: {e}"),
        }
    }
}

impl std::error::Error for LogError {}

/// Free-space probe abstraction. Production wires [`StatvfsProbe`];
/// tests wire `MockFreeSpace` so the disk-full path is hermetic.
pub trait FreeSpaceProbe: Send + Sync {
    /// Bytes available to a non-root writer at `path`. Returns 0 if
    /// the probe fails — that maps cleanly onto "refuse the write".
    fn free_bytes(&self, path: &Path) -> u64;
}

/// Production probe — wraps `libc::statvfs`. Returns
/// `f_bavail * f_frsize`; on `statvfs` failure returns 0 so the
/// caller refuses the write (fail-closed).
#[derive(Debug, Default)]
pub struct StatvfsProbe;

impl FreeSpaceProbe for StatvfsProbe {
    fn free_bytes(&self, path: &Path) -> u64 {
        let c_path = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => return 0,
        };
        // SAFETY: `c_path` outlives the call; `stat` is a stack-local
        // `libc::statvfs` initialised to zero, and `statvfs` writes
        // every field it returns nonzero for. Per `man 3 statvfs` the
        // return is 0 on success, -1 on error.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if rc != 0 {
            return 0;
        }
        (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64)
    }
}

/// Mutable per-instance bookkeeping. Lives behind a [`Mutex`] so the
/// daemon (Step 10 wires this onto a tokio task that may be moved
/// across threads) can share a single `Logger`.
#[derive(Debug, Default)]
struct LoggerState {
    /// Day of the most-recently-emitted size-cap marker. When the
    /// clock crosses midnight this resets implicitly — the new day's
    /// path doesn't match, so the cap doesn't carry over.
    capped_day: Option<NaiveDate>,
}

/// NDJSON audit-log writer. Per-instance; shared across the daemon's
/// 1 Hz tick. Owns the path root, the size cap, the retention
/// horizon, and the free-space probe.
pub struct Logger {
    root: PathBuf,
    max_size_bytes: u64,
    retention_days: u32,
    free_space_probe: Box<dyn FreeSpaceProbe>,
    state: Mutex<LoggerState>,
}

impl Logger {
    /// Production constructor: 50 MiB cap, 7-day retention,
    /// statvfs-backed probe.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            retention_days: DEFAULT_RETENTION_DAYS,
            free_space_probe: Box::new(StatvfsProbe),
            state: Mutex::new(LoggerState::default()),
        }
    }

    /// Crate-internal constructor with explicit knobs. Test-only:
    /// tests use it to override the size cap (keeping disk I/O under
    /// a KiB) and to inject a [`MockFreeSpace`] probe so the
    /// disk-full path is hermetic.
    #[cfg(test)]
    pub(crate) fn with_overrides(
        root: PathBuf,
        max_size_bytes: u64,
        retention_days: u32,
        probe: Box<dyn FreeSpaceProbe>,
    ) -> Self {
        Self {
            root,
            max_size_bytes,
            retention_days,
            free_space_probe: probe,
            state: Mutex::new(LoggerState::default()),
        }
    }

    /// Retention horizon in days. Read by the daemon's S3 startup
    /// guard rail ([`crate::power::onboarding::retention_guard`]) so a
    /// retention window shorter than the onboarding window surfaces a
    /// loud WARN instead of silently starving the onboarding gate.
    pub(crate) fn retention_days(&self) -> u32 {
        self.retention_days
    }

    /// Path the entry for `day` lands at. Public-in-crate so Step 12's
    /// tail reader can enumerate without re-deriving the format
    /// string.
    pub(crate) fn day_path(&self, day: NaiveDate) -> PathBuf {
        self.root.join(format!("telemetry-{day}.ndjson"))
    }

    /// Append one entry to today's file (UTC). See module docstring
    /// for the full state machine. Idempotent w.r.t. the on-disk
    /// layout: if the directory is missing, it is created with
    /// `0700` perms (matches `~/.local/state/sy/power/` convention).
    pub fn append(&self, entry: &AuditEntry, clock: &dyn Clock) -> Result<(), LogError> {
        let free = self.free_space_probe.free_bytes(&self.root);
        if free < MIN_FREE_BYTES {
            return Err(LogError::OutOfSpace { free_bytes: free });
        }
        fs::create_dir_all(&self.root).map_err(|e| LogError::Io(e.to_string()))?;

        let now = clock.now();
        let day = now.date_naive();

        let mut line =
            serde_json::to_string(entry).map_err(|e| LogError::Serialize(e.to_string()))?;
        line.push('\n');

        // BUG-20260712-*: when the day's active segment reaches the size
        // cap we no longer refuse the write (which silently starved the
        // trainer corpus for the rest of the day and spammed a per-tick
        // WARN). Instead we roll over to an overflow segment
        // (`telemetry-YYYY-MM-DD.1.ndjson`, `.2`, …) so collection
        // continues, record the cap event once per day, and rely on the
        // retention sweep (which now also parses overflow segments) to
        // keep total disk bounded.
        let path = self.active_segment_path(day, line.len() as u64, now)?;

        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| LogError::Io(e.to_string()))?;
        f.write_all(line.as_bytes())
            .map_err(|e| LogError::Io(e.to_string()))?;
        Ok(())
    }

    /// Path of the day's segment `seg`. Segment 0 is the base file
    /// (`telemetry-YYYY-MM-DD.ndjson`); overflow segments carry a
    /// numeric suffix (`telemetry-YYYY-MM-DD.1.ndjson`, `.2`, …).
    fn segment_path(&self, day: NaiveDate, seg: u32) -> PathBuf {
        if seg == 0 {
            self.day_path(day)
        } else {
            self.root.join(format!("telemetry-{day}.{seg}.ndjson"))
        }
    }

    /// Highest contiguous overflow-segment index that already exists on
    /// disk for `day` (0 when only the base file — or nothing — exists).
    /// Segments are created contiguously, so a probe-until-gap loop is
    /// exact and cheaper than a full `read_dir`.
    fn highest_existing_segment(&self, day: NaiveDate) -> u32 {
        let mut seg = 0_u32;
        while self.segment_path(day, seg + 1).exists() {
            seg += 1;
        }
        seg
    }

    /// Resolve the segment file this append should land in, rolling
    /// past any segment that would exceed [`Logger::max_size_bytes`].
    /// A fresh (zero-length) segment always accepts the line — a single
    /// entry is never split, and this guarantees termination even if one
    /// serialised entry is larger than the whole cap.
    fn active_segment_path(
        &self,
        day: NaiveDate,
        line_len: u64,
        now: DateTime<Utc>,
    ) -> Result<PathBuf, LogError> {
        let mut seg = self.highest_existing_segment(day);
        loop {
            let path = self.segment_path(day, seg);
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size == 0 || size + line_len <= self.max_size_bytes {
                return Ok(path);
            }
            // Segment full → record the cap event (once per day) and
            // roll forward so collection continues on the next segment.
            self.record_size_cap(&path, day, now)?;
            seg += 1;
        }
    }

    /// Note that the day's telemetry hit the size cap: write a single
    /// `rotated:size_cap` marker into the full segment and emit exactly
    /// one WARN — both deduped to once per day via
    /// [`LoggerState::capped_day`] so a persistently-capping day rolls
    /// through many overflow segments without spamming the journal.
    fn record_size_cap(
        &self,
        path: &Path,
        day: NaiveDate,
        now: DateTime<Utc>,
    ) -> Result<(), LogError> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| LogError::Io(format!("logger state mutex poisoned: {e}")))?;
        if state.capped_day == Some(day) {
            return Ok(());
        }
        // Cross-restart dedupe (BUG-20260522-0037): if the previous
        // process already wrote a marker for this day, do not write a
        // second one. The marker is the last meaningful line in a
        // capped file, so it always lives within the trailing few
        // hundred bytes — peek there instead of slurping the whole
        // file.
        if file_tail_has_size_cap_marker(path) {
            state.capped_day = Some(day);
            return Ok(());
        }
        let marker = serde_json::json!({
            "schema": SCHEMA_ID,
            "marker": "rotated:size_cap",
            "ts": now.to_rfc3339(),
        });
        let mut line = marker.to_string();
        line.push('\n');
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| LogError::Io(e.to_string()))?;
        f.write_all(line.as_bytes())
            .map_err(|e| LogError::Io(e.to_string()))?;
        state.capped_day = Some(day);
        // Loud-once: the operator sees the cap the day it happens, but
        // the per-tick 1 Hz roll never repeats it.
        tracing::warn!(
            target: "sy::power::log",
            day = %day,
            cap_bytes = self.max_size_bytes,
            "audit log daily size cap reached; rolling to overflow segment (telemetry collection continues)",
        );
        Ok(())
    }

    /// Read the audit log in reverse chronological order, returning
    /// every entry whose `snapshot.ts` falls inside the
    /// `[clock.now() - since, clock.now()]` window. Newest first.
    ///
    /// Files are sorted by their `YYYY-MM-DD` stem (descending); each
    /// file is read whole and split into lines, then lines are
    /// reverse-iterated so the newest line in each file is visited
    /// first. Lines that fail JSON parsing (e.g. the
    /// `rotated:size_cap` marker emitted by [`Logger::append`]) are
    /// silently skipped — readers contract is "best-effort tail",
    /// not "fail on the first malformed byte". Iteration stops early
    /// once a file's *newest* entry is older than the cutoff, since
    /// older files cannot contain anything newer.
    pub fn tail(&self, since: Duration, clock: &dyn Clock) -> Result<Vec<AuditEntry>, LogError> {
        let now = clock.now();
        let chrono_since = chrono::Duration::from_std(since)
            .map_err(|e| LogError::Io(format!("since out of range: {e}")))?;
        let cutoff = now - chrono_since;
        let mut paths = match self.collect_day_paths() {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(LogError::Io(e.to_string())),
        };
        paths.sort_by_key(|p| std::cmp::Reverse((p.0, p.1)));
        let mut out = Vec::new();
        for (_day, _seg, path) in paths {
            let contents = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => return Err(LogError::Io(e.to_string())),
            };
            let mut file_has_any_in_window = false;
            for line in contents.lines().rev() {
                let Ok(entry) = serde_json::from_str::<AuditEntry>(line) else {
                    continue;
                };
                if entry.snapshot.ts < cutoff {
                    continue;
                }
                file_has_any_in_window = true;
                out.push(entry);
            }
            // Optimisation: if nothing in this file landed in the
            // window, older files can't either (sorted descending).
            if !file_has_any_in_window {
                break;
            }
        }
        Ok(out)
    }

    /// Read the audit log in reverse chronological order, returning at
    /// most the `n` most-recent entries irrespective of how old they
    /// are. Step 23's `sy power explain` is count-bounded (the SPEC §4
    /// `--last=N` flag), whereas Step 12's [`Logger::tail`] is
    /// time-bounded. The two share `collect_day_paths` + the same
    /// best-effort line-skip semantics. `n == 0` short-circuits without
    /// touching disk.
    pub fn tail_count(&self, n: usize, _clock: &dyn Clock) -> Result<Vec<AuditEntry>, LogError> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut paths = match self.collect_day_paths() {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(LogError::Io(e.to_string())),
        };
        paths.sort_by_key(|p| std::cmp::Reverse((p.0, p.1)));
        let mut out = Vec::with_capacity(n);
        for (_day, _seg, path) in paths {
            let contents = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => return Err(LogError::Io(e.to_string())),
            };
            for line in contents.lines().rev() {
                let Ok(entry) = serde_json::from_str::<AuditEntry>(line) else {
                    continue;
                };
                out.push(entry);
                if out.len() >= n {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// Enumerate `telemetry-YYYY-MM-DD[.N].ndjson` files under `root`,
    /// pairing each path with the parsed `(NaiveDate, segment)`. The
    /// segment lets [`Logger::tail`] / [`Logger::tail_count`] order
    /// same-day overflow files newest-first (higher segment == newer).
    fn collect_day_paths(&self) -> std::io::Result<Vec<(NaiveDate, u32, PathBuf)>> {
        let entries = fs::read_dir(&self.root)?;
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some((day, seg)) = parse_day_segment_from_path(&path) {
                out.push((day, seg, path));
            }
        }
        Ok(out)
    }

    /// Delete `telemetry-YYYY-MM-DD.ndjson` files older than
    /// [`Logger::retention_days`] (relative to `clock.now()`).
    /// Idempotent: a second call after the first is a no-op.
    pub fn rotate_retention(&self, clock: &dyn Clock) -> Result<(), LogError> {
        let today = clock.now().date_naive();
        let cutoff = today - chrono::Duration::days(self.retention_days as i64);
        let entries = match fs::read_dir(&self.root) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(LogError::Io(e.to_string())),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(day) = parse_day_from_path(&path) else {
                continue;
            };
            if day < cutoff {
                fs::remove_file(&path).map_err(|e| LogError::Io(e.to_string()))?;
            }
        }
        Ok(())
    }
}

/// Default `--since` window for `sy power log` when the flag is
/// omitted. Matches the SPEC §4 example (`sy power log [--since=1h]`).
pub const DEFAULT_TAIL_WINDOW: Duration = Duration::from_secs(3600);

/// Parse a short duration string of the form `<number><suffix>` where
/// suffix is `s`, `m`, `h`, or `d`. Returns `None` on any malformed
/// input — the CLI layer maps that to a `usage` exit (2) per CLIG.
///
/// We hand-roll this rather than pull in `humantime` because the
/// audit-log read path only ever needs these four suffixes; adding a
/// dep for four tokens would violate AGENTS.md "search before
/// implementing". The `d` suffix is required by SPEC §RV.2's
/// `sy power show --since=7d` example and by `DEFAULT_SHOW_SINCE`.
pub fn parse_since(spec: &str) -> Option<Duration> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let (num, suffix) = spec.split_at(spec.len().saturating_sub(1));
    let n: u64 = num.parse().ok()?;
    let secs_per_unit: u64 = match suffix {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    n.checked_mul(secs_per_unit).map(Duration::from_secs)
}

/// How many trailing bytes of a capped file we scan when looking for
/// an existing `rotated:size_cap` marker. The marker is the last line
/// written before the file stops accepting entries, so it always lives
/// in the tail — 1 KiB is comfortably above the marker's own length
/// (~100 B) plus one preceding audit entry (~700 B).
const MARKER_TAIL_PROBE_BYTES: u64 = 1024;

/// Return `true` if the trailing [`MARKER_TAIL_PROBE_BYTES`] of `path`
/// already contain a `"marker":"rotated:size_cap"` line. Used by
/// [`Logger::record_size_cap`] for cross-restart dedupe — see
/// BUG-20260522-0037. Best-effort: any I/O error is treated as "no
/// marker present" so the caller falls back to its normal write path.
fn file_tail_has_size_cap_marker(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let Ok(len) = f.seek(SeekFrom::End(0)) else {
        return false;
    };
    let start = len.saturating_sub(MARKER_TAIL_PROBE_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut tail = Vec::with_capacity(MARKER_TAIL_PROBE_BYTES as usize);
    if f.take(MARKER_TAIL_PROBE_BYTES)
        .read_to_end(&mut tail)
        .is_err()
    {
        return false;
    }
    // The marker line is JSON without internal newlines, so a raw
    // substring scan is sufficient — no need to parse.
    twoway_contains(&tail, br#""marker":"rotated:size_cap""#)
}

/// Substring search over byte slices. Tiny standalone helper to avoid
/// pulling in `memchr` / `bstr` for a single call site.
fn twoway_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Pull the `YYYY-MM-DD` date out of a `telemetry-<date>[.N].ndjson`
/// path. Returns `None` for any other filename so the retention sweep
/// doesn't touch files it didn't create. Overflow segments share the
/// day of their base file, so the retention sweep deletes them on the
/// same schedule.
fn parse_day_from_path(path: &Path) -> Option<NaiveDate> {
    parse_day_segment_from_path(path).map(|(day, _seg)| day)
}

/// Parse `(day, segment)` out of a `telemetry-<date>[.N].ndjson` path.
/// The base file is segment 0; `telemetry-<date>.1.ndjson` is segment 1
/// and so on. Returns `None` for any filename `Logger` didn't create.
fn parse_day_segment_from_path(path: &Path) -> Option<(NaiveDate, u32)> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("telemetry-")?;
    let stem = rest.strip_suffix(".ndjson")?;
    // The date part contains no '.', so a trailing `.<n>` unambiguously
    // marks an overflow segment.
    if let Some((date_part, seg_part)) = stem.rsplit_once('.') {
        if let (Ok(day), Ok(seg)) = (
            NaiveDate::parse_from_str(date_part, "%Y-%m-%d"),
            seg_part.parse::<u32>(),
        ) {
            return Some((day, seg));
        }
    }
    Some((NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok()?, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::clock::MockClock;
    use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN};
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;

    /// Free-space probe that always reports `value` bytes — used to
    /// drive the `OutOfSpace` path deterministically without a real
    /// disk. `AtomicU64` so a single instance can be flipped between
    /// "plenty of room" and "disk full" inside one test.
    struct MockFreeSpace(AtomicU64);

    impl MockFreeSpace {
        fn new(b: u64) -> Self {
            Self(AtomicU64::new(b))
        }
    }

    impl FreeSpaceProbe for MockFreeSpace {
        fn free_bytes(&self, _path: &Path) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// 10 GiB — comfortably above [`MIN_FREE_BYTES`] so the disk-full
    /// gate doesn't fire in tests that aren't about free space.
    const PLENTY: u64 = 10 * 1024 * 1024 * 1024;

    fn fixed_snapshot(ts: DateTime<Utc>) -> Snapshot {
        Snapshot {
            schema: crate::power::snapshot::SCHEMA_ID,
            ts,
            features: [0.0_f32; FEATURE_LEN],
            raw: SnapshotRaw::default(),
            snapshot_hash: "0".repeat(64),
        }
    }

    fn at(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, hh, mm, ss).single().unwrap()
    }

    fn read_lines(path: &Path) -> Vec<String> {
        let s = std::fs::read_to_string(path).unwrap_or_default();
        s.lines().map(|l| l.to_string()).collect()
    }

    /// Roadmap DoD: `OutOfSpace` is returned, no file is written,
    /// and the directory is not even created (the probe gate is the
    /// first thing the writer checks).
    #[test]
    fn refuses_when_free_space_below_1gb() {
        let tmp = TempDir::new().unwrap();
        let logger = Logger::with_overrides(
            tmp.path().join("power"),
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(MIN_FREE_BYTES - 1)),
        );
        let clock = MockClock::new(at(2026, 5, 19, 12, 0, 0));
        let entry = AuditEntry::r1(fixed_snapshot(clock.now()));

        let err = logger.append(&entry, &clock).unwrap_err();
        assert!(
            matches!(err, LogError::OutOfSpace { .. }),
            "expected OutOfSpace, got {err:?}",
        );
        assert!(
            !tmp.path().join("power").exists(),
            "directory must not be created when free-space gate refuses",
        );
    }

    /// Roadmap DoD: a midnight boundary crossing closes one file and
    /// opens the next. Both files are readable; each contains one
    /// line.
    #[test]
    fn rotates_at_midnight_boundary() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        let logger = Logger::with_overrides(
            root.clone(),
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(PLENTY)),
        );
        let clock = MockClock::new(at(2026, 5, 19, 23, 59, 59));
        logger
            .append(&AuditEntry::r1(fixed_snapshot(clock.now())), &clock)
            .unwrap();
        // Cross midnight.
        clock.tick(Duration::from_secs(2));
        logger
            .append(&AuditEntry::r1(fixed_snapshot(clock.now())), &clock)
            .unwrap();

        let day1 = root.join("telemetry-2026-05-19.ndjson");
        let day2 = root.join("telemetry-2026-05-20.ndjson");
        assert_eq!(
            read_lines(&day1).len(),
            1,
            "day 1 file should hold one entry"
        );
        assert_eq!(
            read_lines(&day2).len(),
            1,
            "day 2 file should hold one entry"
        );
    }

    /// Roadmap DoD: pre-populate 9 dated files. After
    /// `rotate_retention`, files older than 7 days are gone; files
    /// within the 7-day window are kept.
    #[test]
    fn deletes_files_older_than_7_days() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        fs::create_dir_all(&root).unwrap();
        let today = at(2026, 5, 19, 12, 0, 0).date_naive();
        let mut kept = Vec::new();
        let mut gone = Vec::new();
        for offset in 0..9_i64 {
            let day = today - chrono::Duration::days(offset);
            let p = root.join(format!("telemetry-{day}.ndjson"));
            fs::write(&p, "{}\n").unwrap();
            // "Older than 7 days" is strictly more than 7 days:
            // cutoff = today − 7, delete iff `day < cutoff`. Offsets
            // 0..=7 are within the window (kept); offset 8+ falls
            // outside (deleted).
            if offset <= DEFAULT_RETENTION_DAYS as i64 {
                kept.push(p);
            } else {
                gone.push(p);
            }
        }
        let logger = Logger::with_overrides(
            root.clone(),
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(PLENTY)),
        );
        let clock = MockClock::new(at(2026, 5, 19, 12, 0, 0));
        logger.rotate_retention(&clock).unwrap();
        for p in kept {
            assert!(p.exists(), "expected retained: {}", p.display());
        }
        for p in gone {
            assert!(!p.exists(), "expected deleted: {}", p.display());
        }
        // Idempotent: a second run is a no-op.
        logger.rotate_retention(&clock).unwrap();
    }

    /// BUG-20260712-*: Problem A. Overflow segments
    /// (`telemetry-YYYY-MM-DD.N.ndjson`) must be swept by
    /// `rotate_retention` on the same schedule as their base file, so a
    /// capping day 8+ days ago cannot leak disk. An overflow segment for
    /// a day *inside* the window is kept.
    #[test]
    fn retention_sweeps_overflow_segments() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        fs::create_dir_all(&root).unwrap();
        let today = at(2026, 5, 19, 12, 0, 0).date_naive();
        // 8 days ago is outside the 7-day window → base + overflow gone.
        let old = today - chrono::Duration::days(8);
        let old_base = root.join(format!("telemetry-{old}.ndjson"));
        let old_ovf = root.join(format!("telemetry-{old}.1.ndjson"));
        // Today is inside the window → base + overflow kept.
        let new_base = root.join(format!("telemetry-{today}.ndjson"));
        let new_ovf = root.join(format!("telemetry-{today}.2.ndjson"));
        for p in [&old_base, &old_ovf, &new_base, &new_ovf] {
            fs::write(p, "{}\n").unwrap();
        }
        let logger = Logger::with_overrides(
            root.clone(),
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(PLENTY)),
        );
        let clock = MockClock::new(at(2026, 5, 19, 12, 0, 0));
        logger.rotate_retention(&clock).unwrap();
        assert!(!old_base.exists(), "old base file must be swept");
        assert!(!old_ovf.exists(), "old overflow segment must be swept");
        assert!(new_base.exists(), "in-window base file must be kept");
        assert!(new_ovf.exists(), "in-window overflow segment must be kept");
    }

    /// BUG-20260712-*: Problem A. With a small `max_size_bytes`, the
    /// append that crosses the cap writes a single `rotated:size_cap`
    /// marker into the base file, rolls to an overflow segment
    /// (`telemetry-…​.1.ndjson`), and **continues** writing there —
    /// `append` returns `Ok`, never a cap error, so collection never
    /// silently stops for the rest of the day. The cap event is logged
    /// exactly once even though many entries roll past it.
    #[test]
    #[tracing_test::traced_test]
    fn size_cap_rolls_to_overflow_and_warns_once() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        // 1 KiB cap is well above one serialised entry (~600 B) but
        // well below two — every entry after the first rolls a segment.
        const CAP: u64 = 1024;
        let logger = Logger::with_overrides(
            root.clone(),
            CAP,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(PLENTY)),
        );
        let clock = MockClock::new(at(2026, 5, 19, 12, 0, 0));
        let entry = AuditEntry::r1(fixed_snapshot(clock.now()));
        let base = root.join("telemetry-2026-05-19.ndjson");
        let overflow1 = root.join("telemetry-2026-05-19.1.ndjson");

        // First append fits in the base file.
        logger.append(&entry, &clock).unwrap();
        assert!(fs::metadata(&base).unwrap().len() > 0);

        // Second append crosses the cap → marker in base, rolls to
        // overflow, still Ok (collection continues).
        logger.append(&entry, &clock).unwrap();
        let base_lines = read_lines(&base);
        assert!(
            base_lines
                .iter()
                .any(|l| l.contains("\"marker\":\"rotated:size_cap\"")),
            "expected size-cap marker in base file, got: {base_lines:?}",
        );
        assert!(
            base_lines.iter().any(|l| l.contains(SCHEMA_ID)),
            "marker must carry schema id, got: {base_lines:?}",
        );
        // Pre-cap entry survives (roll is not a truncate).
        assert!(
            base_lines
                .iter()
                .any(|l| l.contains("sy.power.audit/v1") && l.contains("snapshot")),
            "pre-cap entry must survive the roll, got: {base_lines:?}",
        );
        // The rolled entry landed in the overflow segment.
        assert!(overflow1.exists(), "overflow segment must be created");
        assert_eq!(
            read_lines(&overflow1)
                .iter()
                .filter(|l| l.contains("snapshot"))
                .count(),
            1,
            "rolled entry must land in the overflow segment",
        );

        // A third append continues rolling (Ok, no second marker).
        logger.append(&entry, &clock).unwrap();
        let marker_count = read_lines(&base)
            .iter()
            .filter(|l| l.contains("\"marker\":\"rotated:size_cap\""))
            .count();
        assert_eq!(marker_count, 1, "marker must be written exactly once per day");

        // The cap WARN fires exactly once for the day despite two rolls.
        logs_assert(|lines: &[&str]| {
            let n = lines
                .iter()
                .filter(|l| l.contains("audit log daily size cap reached"))
                .count();
            if n == 1 {
                Ok(())
            } else {
                Err(format!("expected exactly one cap WARN, got {n}"))
            }
        });
    }

    /// BUG-20260522-0037: each daemon restart used to write a fresh
    /// `rotated:size_cap` marker because `state.capped_day` is
    /// in-memory only. Simulate a restart by dropping the first
    /// `Logger` and building a second one on the same root; the file
    /// must still contain exactly one marker after the second writer
    /// hits the cap.
    #[test]
    fn size_cap_marker_dedupes_across_logger_restarts() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        const CAP: u64 = 1024;
        let clock = MockClock::new(at(2026, 5, 19, 12, 0, 0));
        let entry = AuditEntry::r1(fixed_snapshot(clock.now()));
        let path = root.join("telemetry-2026-05-19.ndjson");

        // First "process": land one entry, then hit the cap so the
        // marker is written and the writer rolls to overflow.
        {
            let logger = Logger::with_overrides(
                root.clone(),
                CAP,
                DEFAULT_RETENTION_DAYS,
                Box::new(MockFreeSpace::new(PLENTY)),
            );
            logger.append(&entry, &clock).unwrap();
            logger.append(&entry, &clock).unwrap();
        }

        // Second "process": same on-disk state, fresh in-memory
        // `capped_day`. Further appends must keep succeeding (rolling
        // through overflow segments) without writing a second marker.
        {
            let logger = Logger::with_overrides(
                root.clone(),
                CAP,
                DEFAULT_RETENTION_DAYS,
                Box::new(MockFreeSpace::new(PLENTY)),
            );
            for _ in 0..3 {
                logger.append(&entry, &clock).unwrap();
            }
        }

        let marker_count = read_lines(&path)
            .iter()
            .filter(|l| l.contains("\"marker\":\"rotated:size_cap\""))
            .count();
        assert_eq!(
            marker_count, 1,
            "exactly one marker must survive a daemon restart after the cap is hit"
        );
    }

    /// Production constructor smoke-test: [`Logger::new`] yields the
    /// documented defaults and the [`StatvfsProbe`] reports a
    /// nonzero value on `/tmp` (a real mountpoint). Keeps both
    /// production-only symbols exercised from the test build so they
    /// don't drift into dead code before Step 10's daemon wires
    /// `Logger::new` from `cli::daemon`.
    #[test]
    fn production_constructor_defaults_and_statvfs_alive() {
        let tmp = TempDir::new().unwrap();
        let logger = Logger::new(tmp.path().join("power"));
        assert_eq!(logger.max_size_bytes, DEFAULT_MAX_SIZE_BYTES);
        assert_eq!(logger.retention_days, DEFAULT_RETENTION_DAYS);
        // statvfs on /tmp (or any real path) returns a nonzero value
        // on a working filesystem; if it ever returns 0 here, the
        // disk is full and the rest of the suite would fail anyway.
        let probe = StatvfsProbe;
        assert!(probe.free_bytes(Path::new("/tmp")) > 0);
    }

    /// BUG-20260522-0037: regression guard against the 50 MiB cap that
    /// made `sy power show` stuck at 65,538 entries. If a future
    /// refactor shrinks the daily cap below 24 h × 1 Hz worth of
    /// audit entries at the observed worst-case post-trim size
    /// (~1 KiB/entry), the thick-report threshold becomes
    /// mathematically unreachable again. Evaluated in a `const` block
    /// so the check fires at compile time, not at test run time.
    #[test]
    fn default_max_size_fits_one_day_at_1hz() {
        const _: () = {
            const MIN_BYTES_FOR_24H_AT_1HZ: u64 = 24 * 3600 * 1024;
            assert!(
                DEFAULT_MAX_SIZE_BYTES >= MIN_BYTES_FOR_24H_AT_1HZ,
                "DEFAULT_MAX_SIZE_BYTES below 24 h × 1 Hz × 1 KiB; \
                 daily file would cap before sy power show reaches \
                 the 86,400-entry thick-report threshold",
            );
        };
    }

    /// `parse_since` accepts every suffix the CLI advertises (`s`,
    /// `m`, `h`, `d`) and rejects anything else, so a bad `--since=`
    /// argument flips the CLI to a usage error rather than silently
    /// tailing the default window. The `d` suffix matters because
    /// `DEFAULT_SHOW_SINCE` and the SPEC §RV.2 example both call out
    /// `--since=7d`.
    #[test]
    fn parse_since_accepts_documented_suffixes_and_rejects_garbage() {
        assert_eq!(parse_since("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_since("15m"), Some(Duration::from_secs(15 * 60)));
        assert_eq!(parse_since("2h"), Some(Duration::from_secs(2 * 3600)));
        assert_eq!(parse_since("1d"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_since("7d"), Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_since(" 1h "), Some(Duration::from_secs(3600)));
        assert_eq!(parse_since(""), None);
        assert_eq!(parse_since("abc"), None);
        // No overflow on absurdly large counts.
        assert_eq!(parse_since("99999999999999999999h"), None);
    }

    /// Step 12 DoD: `Logger::tail(since, &clock)` returns only entries
    /// whose `snapshot.ts` falls inside the `[now - since, now]`
    /// window, in newest-first order. Write 5 entries at -5m, -3m,
    /// -1m, -30s, -5s; tailing the last 2 minutes returns the 3 most
    /// recent.
    #[test]
    fn tail_filters_by_since() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        let logger = Logger::with_overrides(
            root,
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(PLENTY)),
        );
        let now = at(2026, 5, 19, 12, 0, 0);
        let offsets_s: [i64; 5] = [-300, -180, -60, -30, -5];
        for off in offsets_s {
            let ts = now + chrono::Duration::seconds(off);
            let clock = MockClock::new(ts);
            logger
                .append(&AuditEntry::r1(fixed_snapshot(ts)), &clock)
                .unwrap();
        }
        let read_clock = MockClock::new(now);
        let entries = logger.tail(Duration::from_secs(120), &read_clock).unwrap();
        assert_eq!(entries.len(), 3, "expected 3 entries inside 120 s window");
        // Newest first: -5s, -30s, -60s.
        assert_eq!(entries[0].snapshot.ts, now + chrono::Duration::seconds(-5));
        assert_eq!(entries[2].snapshot.ts, now + chrono::Duration::seconds(-60));
    }

    /// BUG-20260712-1137: a manual pin records `f32::INFINITY` as the
    /// ranked score, which `serde_json` serialises as `null`. The tail
    /// reader must parse the line back (`null → NaN`) instead of
    /// silently skipping it, otherwise `sy power log --since=<short>`
    /// returns "no entries" while a pin is active.
    #[test]
    fn tail_reads_pinned_entries_with_nonfinite_score() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        let logger = Logger::with_overrides(
            root,
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(PLENTY)),
        );
        let now = at(2026, 5, 19, 12, 0, 0);
        let entry = AuditEntry::r3(
            fixed_snapshot(now),
            "flat-out".to_string(),
            "COOL_AC".to_string(),
            vec!["pin:flat-out".to_string()],
            vec![("flat-out".to_string(), f32::INFINITY)],
            0.05,
        );
        logger.append(&entry, &MockClock::new(now)).unwrap();
        let read_clock = MockClock::new(now);
        let entries = logger.tail(Duration::from_secs(30), &read_clock).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "pinned entry must be tailed back, not skipped as unparseable",
        );
        assert_eq!(entries[0].ranked_actions.len(), 1);
        assert_eq!(entries[0].ranked_actions[0].0, "flat-out");
        assert!(
            entries[0].ranked_actions[0].1.is_nan(),
            "null score reconstitutes as NaN",
        );
    }

    /// BUG-20260712-1137: the on-wire form of a non-finite score is
    /// `null`; the roundtrip through serde must survive it. Guards the
    /// IPC `status --json` decode path (same `AuditEntry` deserialiser).
    #[test]
    fn audit_entry_with_nonfinite_score_round_trips() {
        let now = at(2026, 5, 19, 12, 0, 0);
        let entry = AuditEntry::r3(
            fixed_snapshot(now),
            "flat-out".to_string(),
            "COOL_AC".to_string(),
            vec!["pin:flat-out".to_string()],
            vec![("flat-out".to_string(), f32::INFINITY)],
            0.05,
        );
        let line = serde_json::to_string(&entry).unwrap();
        assert!(
            line.contains("[\"flat-out\",null]"),
            "non-finite score must serialise as null: {line}",
        );
        let back: AuditEntry = serde_json::from_str(&line).expect("pinned line must deserialize");
        assert!(back.ranked_actions[0].1.is_nan());
    }

    /// Step 12 DoD: `Logger::tail` walks every dated file under the
    /// root, not just today's, and returns them in newest-first order
    /// across rotation boundaries. Write 3 entries to "yesterday" and
    /// 3 to "today"; `tail(48h)` returns all 6 with today's entries
    /// preceding yesterday's.
    #[test]
    fn tail_handles_rotated_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        let logger = Logger::with_overrides(
            root,
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(PLENTY)),
        );
        let today_base = at(2026, 5, 19, 12, 0, 0);
        let yest_base = at(2026, 5, 18, 12, 0, 0);
        for off in [0, 30, 60] {
            let ts = yest_base + chrono::Duration::seconds(off);
            logger
                .append(&AuditEntry::r1(fixed_snapshot(ts)), &MockClock::new(ts))
                .unwrap();
        }
        for off in [0, 30, 60] {
            let ts = today_base + chrono::Duration::seconds(off);
            logger
                .append(&AuditEntry::r1(fixed_snapshot(ts)), &MockClock::new(ts))
                .unwrap();
        }
        let read_clock = MockClock::new(today_base + chrono::Duration::seconds(120));
        let entries = logger
            .tail(Duration::from_secs(48 * 3600), &read_clock)
            .unwrap();
        assert_eq!(entries.len(), 6, "all 6 entries across two days returned");
        // Today's three precede yesterday's three.
        for w in entries.windows(2) {
            assert!(
                w[0].snapshot.ts >= w[1].snapshot.ts,
                "entries must be sorted newest-first: {:?} vs {:?}",
                w[0].snapshot.ts,
                w[1].snapshot.ts,
            );
        }
        assert_eq!(
            entries[0].snapshot.ts,
            today_base + chrono::Duration::seconds(60)
        );
        assert_eq!(entries[5].snapshot.ts, yest_base);
    }

    /// Step 23 DoD: `Logger::tail_count(n, &clock)` returns the most
    /// recent N entries in newest-first order, irrespective of any
    /// time window. With 5 entries written across two days, asking for
    /// `tail_count(3)` returns the 3 newest; asking for `tail_count(0)`
    /// returns an empty vector without I/O fallthrough.
    #[test]
    fn tail_count_returns_most_recent_n_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        let logger = Logger::with_overrides(
            root,
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(PLENTY)),
        );
        let base = at(2026, 5, 19, 12, 0, 0);
        for off in [-300_i64, -180, -60, -30, -5] {
            let ts = base + chrono::Duration::seconds(off);
            logger
                .append(&AuditEntry::r1(fixed_snapshot(ts)), &MockClock::new(ts))
                .unwrap();
        }
        let read_clock = MockClock::new(base);
        let entries = logger.tail_count(3, &read_clock).unwrap();
        assert_eq!(entries.len(), 3, "expected 3 most-recent entries");
        assert_eq!(entries[0].snapshot.ts, base + chrono::Duration::seconds(-5));
        assert_eq!(
            entries[2].snapshot.ts,
            base + chrono::Duration::seconds(-60)
        );
        let empty = logger.tail_count(0, &read_clock).unwrap();
        assert!(empty.is_empty(), "n=0 must short-circuit to empty");
    }

    /// Step 12 DoD: an `AuditEntry` serializes to a single JSON line
    /// and deserializes back byte-equal on the feature vec — the
    /// invariant that lets `Logger::tail` reconstruct the typed entry
    /// from the on-disk NDJSON without a parallel `AuditEntryRaw`
    /// schema.
    #[test]
    fn audit_entry_round_trips_through_serde() {
        let ts = at(2026, 5, 19, 12, 0, 0);
        let mut snap = fixed_snapshot(ts);
        // Distinct features so byte-equal is a real assertion, not a
        // zero-init tautology.
        for (i, slot) in snap.features.iter_mut().enumerate() {
            *slot = i as f32 + 0.5;
        }
        let entry = AuditEntry::r1(snap);
        let line = serde_json::to_string(&entry).expect("serialize entry");
        let back: AuditEntry = serde_json::from_str(&line).expect("deserialize entry");
        assert_eq!(back.schema, SCHEMA_ID);
        assert_eq!(back.snapshot.schema, crate::power::snapshot::SCHEMA_ID);
        for i in 0..FEATURE_LEN {
            assert_eq!(
                back.snapshot.features[i].to_bits(),
                entry.snapshot.features[i].to_bits(),
                "feature[{i}] survived round-trip",
            );
        }
        assert_eq!(back.snapshot.snapshot_hash, entry.snapshot.snapshot_hash);
        assert_eq!(back.snapshot.ts, entry.snapshot.ts);
    }

    /// Roadmap DoD: NDJSON is one JSON object per line — no pretty
    /// printing, no array wrapper. Each line decodes back into a
    /// JSON object carrying the v1 schema id.
    #[test]
    fn ndjson_one_json_object_per_line() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("power");
        let logger = Logger::with_overrides(
            root.clone(),
            DEFAULT_MAX_SIZE_BYTES,
            DEFAULT_RETENTION_DAYS,
            Box::new(MockFreeSpace::new(PLENTY)),
        );
        let clock = MockClock::new(at(2026, 5, 19, 12, 0, 0));
        for _ in 0..3 {
            logger
                .append(&AuditEntry::r1(fixed_snapshot(clock.now())), &clock)
                .unwrap();
        }
        let path = root.join("telemetry-2026-05-19.ndjson");
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 3);
        for l in &lines {
            assert!(!l.contains('\n'), "line contains embedded newline: {l:?}",);
            let v: serde_json::Value = serde_json::from_str(l).expect("each line is valid JSON");
            assert_eq!(v["schema"], SCHEMA_ID);
        }
    }
}
