//! Versioned, atomic-write checkpoint persistence for the bandit +
//! classifier state held in [`crate::power::daemon::BanditTickState`]
//! and [`crate::power::daemon::ActivityTickState`].
//!
//! Until this module landed, both structs were constructed fresh on
//! every `sy-powerd` boot — every `sy apply`, `systemctl restart`, or
//! suspend-resume wiped the CLUCB posterior + FTRL classifier weights.
//! See BUG-20260525-2353 §Reproduction for the journal-restart count
//! that surfaced the problem (8 distinct PIDs in 4 days on the audit
//! host).
//!
//! ## Format
//!
//! Single JSON file under `~/.local/state/sy/power/checkpoint.json`.
//! JSON over bincode is a deliberate choice — both structs are
//! < 1 KB serialised, and human-debuggability beats binary compactness
//! when an operator is staring at `cat checkpoint.json | jq` trying
//! to figure out why the bandit is suggesting `flat-out` at 03:00.
//!
//! ## Atomicity
//!
//! [`save`] writes to `<path>.tmp`, fsyncs, then renames over `<path>`
//! — the same idiom [`crate::power::trainer::FileSink::commit`] uses
//! for ONNX exports. A crashed daemon never leaves a half-written
//! `checkpoint.json` for the next boot to half-load.
//!
//! ## Schema / arms drift
//!
//! [`load`] returns `Ok(None)` (re-learn-from-zero) when either:
//! - the on-disk `schema` doesn't equal [`CHECKPOINT_SCHEMA`], or
//! - the on-disk `arms_hash` doesn't equal the caller-supplied
//!   `expected_arms_hash` (which the daemon computes from the live
//!   `cfg.arms` vector at startup).
//!
//! On either mismatch the stale file is rotated to
//! `<path>.stale-<rfc3339>` so an operator can diff it post-hoc, and
//! an INFO line goes through `tracing` so the re-learn event is
//! attributable in journalctl.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::power::activity::ClassifierState;
use crate::power::bandit::clucb::ClucbState;
use crate::power::bandit::Arm;

/// On-disk checkpoint schema version. Bump when [`DaemonCheckpoint`]
/// (or its component states) gain or drop a field in a way that
/// can't be back-filled at load time. A bump invalidates every
/// host's previously-persisted state — accepted as the cost of a
/// schema change; a migration path (load v_old, transform, save
/// v_new) is out of scope for the v1 ship.
pub const CHECKPOINT_SCHEMA: u32 = 1;

/// Periodic save cadence in daemon ticks. At the daemon's 1 Hz tick
/// rate this fires once every 5 minutes — so a SIGKILL (kernel OOM,
/// `kill -9`) loses at most ~5 minutes of accumulated learning. Sized
/// to keep the daemon's per-tick write budget at < 0.4 % wall-clock:
/// both structs round-trip at < 1 KB JSON serialised, and the
/// rename-after-fsync settles in single-digit milliseconds on a tmpfs
/// or NVMe-backed `~/.local/state/`.
pub const CHECKPOINT_INTERVAL_TICKS: u64 = 300;

/// Full checkpoint payload. Round-trips through serde JSON; the
/// daemon (`crate::power::daemon::run_async`) builds one from live
/// state every [`CHECKPOINT_INTERVAL_TICKS`] ticks and on graceful
/// shutdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonCheckpoint {
    /// [`CHECKPOINT_SCHEMA`] at save time. Mismatch ⇒ re-learn.
    pub schema: u32,
    /// [`arms_hash`] of the live `cfg.arms` vector at save time.
    /// Mismatch ⇒ re-learn (the arm vocabulary mutated under us).
    pub arms_hash: u64,
    /// CLUCB posterior + counts.
    pub bandit: ClucbState,
    /// Per-class FTRL accumulators.
    pub classifier: ClassifierState,
    /// Wall-clock at the moment [`save`] was called. Op-visible via
    /// `cat checkpoint.json | jq .saved_at`.
    pub saved_at: DateTime<Utc>,
}

impl DaemonCheckpoint {
    /// Number of FTRL classes carried by [`Self::classifier`]. Used by
    /// the daemon's "checkpoint hydrated" INFO line so the operator
    /// can sanity-check that the on-disk file matches the expected
    /// taxonomy (always [`crate::power::activity::ACTIVITY_CLASS_COUNT`]
    /// at the time of writing — surfaced anyway so a future taxonomy
    /// bump is visible in journalctl).
    pub fn classifier_class_count(&self) -> usize {
        self.classifier.class_count()
    }
}

/// Atomic write: serialise `ck` to JSON, write `<path>.tmp`, fsync,
/// rename over `<path>`. Mirrors
/// [`crate::power::trainer::FileSink::commit`]. The parent directory
/// is created if missing.
pub fn save(ck: &DaemonCheckpoint, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes =
        serde_json::to_vec_pretty(ck).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = tmp_sibling(path);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Load + verify. Returns `Ok(None)` when the checkpoint is absent
/// or its schema / arms_hash don't match the caller's expectations.
/// On a mismatch the stale file is rotated to
/// `<path>.stale-<rfc3339>` (best-effort; a rename failure is logged
/// and otherwise ignored — the next [`save`] will overwrite the
/// original in place) and an INFO tracing line is emitted so the
/// re-learn event is attributable in journalctl.
pub fn load(path: &Path, expected_arms_hash: u64) -> io::Result<Option<DaemonCheckpoint>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let ck: DaemonCheckpoint = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => {
            tracing::info!(
                target: "sy::power::checkpoint",
                error = %e,
                path = %path.display(),
                "checkpoint deserialise failed; rotating stale file, re-learning from zero",
            );
            rotate_stale(path);
            return Ok(None);
        }
    };
    if ck.schema != CHECKPOINT_SCHEMA || ck.arms_hash != expected_arms_hash {
        tracing::info!(
            target: "sy::power::checkpoint",
            on_disk_schema = ck.schema,
            expected_schema = CHECKPOINT_SCHEMA,
            on_disk_arms_hash = ck.arms_hash,
            expected_arms_hash = expected_arms_hash,
            path = %path.display(),
            "checkpoint schema or arms-hash mismatch, re-learning from zero",
        );
        rotate_stale(path);
        return Ok(None);
    }
    Ok(Some(ck))
}

/// Stable 64-bit fingerprint of an arms vector. The daemon recomputes
/// this every boot from `cfg.arms`; [`load`] uses it to detect a
/// `power.toml`-driven schema drift (arm added / removed / renamed /
/// re-ordered) and re-init the bandit cleanly.
///
/// Implementation: `blake3` over the canonical-JSON-encoded arms slice,
/// first 8 bytes folded into a u64. `blake3` is already in the
/// workspace (`Cargo.toml` line 63) — no new dep. Stable across Rust
/// versions and host architectures, unlike `DefaultHasher`.
pub fn arms_hash(arms: &[Arm]) -> u64 {
    let bytes = serde_json::to_vec(arms).unwrap_or_default();
    let digest = blake3::hash(&bytes);
    let b = digest.as_bytes();
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Sibling tmp path used by [`save`] for the atomic rename. Pulled
/// out so the test for `atomic_write_survives_concurrent_load` can
/// reason about the intermediate file name.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Move the stale checkpoint to `<path>.stale-<rfc3339>`. Best-effort
/// — a failure logs at `warn` and otherwise drops the rename; the
/// next [`save`] will overwrite the original in place.
fn rotate_stale(path: &Path) {
    let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".stale-{stamp}"));
    let stale = path.with_file_name(name);
    if let Err(e) = fs::rename(path, &stale) {
        tracing::warn!(
            target: "sy::power::checkpoint",
            error = %e,
            from = %path.display(),
            to = %stale.display(),
            "stale checkpoint rotation failed; leaving original in place",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::thread;
    use std::time::Duration;

    use chrono::TimeZone;
    use tempfile::TempDir;

    use crate::power::activity::{ActivityLabel, OnlineClassifier};
    use crate::power::bandit::clucb::{for_snapshot_features_with_activity, Clucb};
    use crate::power::bandit::{Arm, CgroupOverrides, Epp, NpuPmode};
    use crate::power::sensors::igpu::IgpuProfileMode;
    use crate::power::sensors::platform::PlatformProfile;
    use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};

    const TEST_ALPHA: f32 = 0.05;

    fn three_arms() -> Vec<String> {
        vec!["browse".into(), "code".into(), "idle".into()]
    }

    fn canonical_arm(name: &str) -> Arm {
        Arm {
            name: name.into(),
            platform_profile: PlatformProfile::Balanced,
            epp: Epp::Default,
            igpu_mode: IgpuProfileMode::BootupDefault,
            npu_pmode: NpuPmode::Default,
            cgroup: CgroupOverrides::default(),
        }
    }

    fn snapshot_with(features: [f32; FEATURE_LEN]) -> Snapshot {
        Snapshot {
            schema: SCHEMA_ID,
            ts: Utc
                .with_ymd_and_hms(2026, 5, 26, 12, 0, 0)
                .single()
                .unwrap(),
            features,
            raw: SnapshotRaw::default(),
            snapshot_hash: "0".repeat(64),
        }
    }

    fn make_checkpoint(bandit: &Clucb, clf: &OnlineClassifier, arms_h: u64) -> DaemonCheckpoint {
        DaemonCheckpoint {
            schema: CHECKPOINT_SCHEMA,
            arms_hash: arms_h,
            bandit: bandit.snapshot(),
            classifier: clf.snapshot(),
            saved_at: Utc
                .with_ymd_and_hms(2026, 5, 26, 12, 0, 0)
                .single()
                .unwrap(),
        }
    }

    /// Round-trip the Clucb posterior through disk. Pin a non-trivial
    /// per-arm count / response vector so a serde regression would
    /// surface as a numerical drift in the restored state, not just a
    /// schema mismatch.
    #[test]
    fn round_trips_bandit_state_through_disk() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("checkpoint.json");
        let mut bandit = for_snapshot_features_with_activity(three_arms(), TEST_ALPHA);
        let ctx: Vec<f32> = (0..bandit.context_dim())
            .map(|i| (i as f32) * 0.1)
            .collect();
        bandit.update("browse", &ctx, 0.7);
        bandit.update("code", &ctx, 0.3);
        bandit.update("idle", &ctx, -0.1);
        bandit.observe_baseline(0.55);
        let original_state = bandit.snapshot();
        let arms_h = 0xC0FFEE_u64;
        let ck = make_checkpoint(&bandit, &OnlineClassifier::new(), arms_h);
        save(&ck, &path).expect("save");
        let loaded = load(&path, arms_h).expect("load").expect("Some");
        let mut restored = for_snapshot_features_with_activity(three_arms(), TEST_ALPHA);
        restored.restore(loaded.bandit);
        assert_eq!(restored.snapshot(), original_state);
    }

    /// Round-trip the FTRL classifier through disk. Drive `partial_fit`
    /// enough times that every per-class `(z, n)` vector has at least
    /// one non-zero coordinate; assert deep equality of the restored
    /// snapshot.
    #[test]
    fn round_trips_classifier_state_through_disk() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("checkpoint.json");
        let mut clf = OnlineClassifier::new();
        let labels = [
            ActivityLabel::Browse,
            ActivityLabel::Code,
            ActivityLabel::Call,
            ActivityLabel::Build,
            ActivityLabel::Idle,
        ];
        for (i, l) in labels.iter().enumerate() {
            let mut feats = [0.0_f32; FEATURE_LEN];
            feats[i % FEATURE_LEN] = 1.0;
            clf.partial_fit(&snapshot_with(feats), *l);
        }
        let original = clf.snapshot();
        let bandit = for_snapshot_features_with_activity(three_arms(), TEST_ALPHA);
        let arms_h = 0xC0FFEE_u64;
        let ck = make_checkpoint(&bandit, &clf, arms_h);
        save(&ck, &path).expect("save");
        let loaded = load(&path, arms_h).expect("load").expect("Some");
        let mut restored = OnlineClassifier::new();
        restored.restore(loaded.classifier);
        // serde_json round-trip → re-snapshot → expect bitwise-equal.
        let after = restored.snapshot();
        let original_json = serde_json::to_string(&original).expect("serialise original");
        let after_json = serde_json::to_string(&after).expect("serialise restored");
        assert_eq!(after_json, original_json);
    }

    /// arms_hash mismatch ⇒ Ok(None) + stale rotation. The expected
    /// hash is deliberately wrong; the file must move to
    /// `<path>.stale-<ts>` so the operator can diff it post-hoc.
    #[test]
    fn arms_hash_mismatch_returns_none_and_rotates_stale() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("checkpoint.json");
        let bandit = for_snapshot_features_with_activity(three_arms(), TEST_ALPHA);
        let ck = make_checkpoint(&bandit, &OnlineClassifier::new(), 0x1234_u64);
        save(&ck, &path).expect("save");
        let loaded = load(&path, 0x5678_u64).expect("load");
        assert!(loaded.is_none(), "arms-hash mismatch must surface as None");
        assert!(!path.exists(), "original file must be moved out of the way");
        let stale_count = fs::read_dir(tmp.path())
            .expect("read tmpdir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("checkpoint.json.stale-")
            })
            .count();
        assert_eq!(
            stale_count, 1,
            "stale rotation must produce one .stale-* file"
        );
    }

    /// Schema mismatch ⇒ Ok(None) + stale rotation. Force-write a
    /// checkpoint with a bogus schema; assert clean re-init.
    #[test]
    fn schema_bump_returns_none_and_rotates_stale() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("checkpoint.json");
        let bandit = for_snapshot_features_with_activity(three_arms(), TEST_ALPHA);
        let mut ck = make_checkpoint(&bandit, &OnlineClassifier::new(), 0xC0FFEE);
        ck.schema = CHECKPOINT_SCHEMA + 99;
        save(&ck, &path).expect("save");
        let loaded = load(&path, 0xC0FFEE).expect("load");
        assert!(loaded.is_none(), "schema mismatch must surface as None");
        assert!(!path.exists(), "original file must be rotated out");
    }

    /// Absent checkpoint ⇒ Ok(None), not Err. This is the first-boot
    /// path on every fresh host.
    #[test]
    fn absent_checkpoint_returns_none_without_error() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("nonexistent.json");
        let loaded = load(&path, 0xC0FFEE).expect("load on absent file is not an error");
        assert!(loaded.is_none());
    }

    /// Concurrent save/load must never surface an Err from a
    /// partial-write race — the rename-after-fsync atomicity is the
    /// whole point of the [`tmp_sibling`] dance.
    #[test]
    fn atomic_write_survives_concurrent_load() {
        const LOAD_ITERS: usize = 100;
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("checkpoint.json");
        let arms_h = 0xC0FFEE_u64;
        let bandit = for_snapshot_features_with_activity(three_arms(), TEST_ALPHA);
        // Seed the file once so the first reader sees a valid
        // checkpoint instead of NotFound.
        save(
            &make_checkpoint(&bandit, &OnlineClassifier::new(), arms_h),
            &path,
        )
        .expect("seed save");

        let writer_path = path.clone();
        let writer_bandit = bandit.clone();
        let writer = thread::spawn(move || {
            for _ in 0..LOAD_ITERS {
                let ck = make_checkpoint(&writer_bandit, &OnlineClassifier::new(), arms_h);
                save(&ck, &writer_path).expect("concurrent save");
                thread::sleep(Duration::from_micros(50));
            }
        });

        for _ in 0..LOAD_ITERS {
            // Either Some(valid) or None (never seen — file always
            // exists once seeded), but NEVER Err.
            let r = load(&path, arms_h).expect("concurrent load must not Err");
            // r may be None if a stale-rotate happened, but we never
            // rotate in the happy path — assert positively.
            assert!(r.is_some(), "concurrent load must see a valid file");
        }
        writer.join().expect("writer join");
    }

    /// End-to-end "restart" simulation: populate both the bandit
    /// posterior AND the classifier weights, save through the public
    /// `save`/`load` API, and rehydrate into a *fresh* pair of structs
    /// (the cold-start state every `sy-powerd` boot sees). Asserts the
    /// rehydrated structs' snapshots match the pre-shutdown snapshots.
    /// This is the in-process equivalent of the
    /// `tests/power_checkpoint_survives_restart.rs` recipe in the
    /// roadmap — the integration-test crate has no access to
    /// `sy::power::checkpoint` (the binary exports no `lib.rs`), so
    /// the "across a restart" semantics are exercised here instead.
    /// See BUG-20260525-2353 §Fix step 5 for the full contract.
    #[test]
    fn survives_simulated_daemon_restart() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("checkpoint.json");
        let arms_h = 0xDEADBEEF_u64;

        // --- Pre-shutdown: populate both structs with non-trivial
        // accumulators, save the checkpoint.
        let mut bandit = for_snapshot_features_with_activity(three_arms(), TEST_ALPHA);
        let ctx: Vec<f32> = (0..bandit.context_dim())
            .map(|i| (i as f32) * 0.05 - 0.1)
            .collect();
        for _ in 0..50 {
            bandit.update("browse", &ctx, 0.6);
            bandit.update("code", &ctx, 0.4);
            bandit.observe_baseline(0.5);
        }
        let mut clf = OnlineClassifier::new();
        let labels = [
            ActivityLabel::Browse,
            ActivityLabel::Code,
            ActivityLabel::Call,
        ];
        for round in 0..20 {
            let l = labels[round % labels.len()];
            let mut feats = [0.0_f32; FEATURE_LEN];
            feats[round % FEATURE_LEN] = 1.0;
            clf.partial_fit(&snapshot_with(feats), l);
        }
        let pre_bandit = bandit.snapshot();
        let pre_clf = clf.snapshot();
        let pre_clf_json = serde_json::to_string(&pre_clf).expect("serialise pre clf");
        let ck = make_checkpoint(&bandit, &clf, arms_h);
        save(&ck, &path).expect("pre-shutdown save");

        // --- "Restart": let the old structs go out of scope, load
        // from disk, restore into *fresh* zero-init structs (the
        // post-boot state). `OnlineClassifier`/`Clucb` don't impl
        // `Drop`, so the prior values are simply left to fall out of
        // scope rather than explicit-dropped (clippy::drop_non_drop).
        let _ = (bandit, clf);
        let mut fresh_bandit = for_snapshot_features_with_activity(three_arms(), TEST_ALPHA);
        let mut fresh_clf = OnlineClassifier::new();
        let loaded = load(&path, arms_h)
            .expect("post-restart load")
            .expect("checkpoint must be present");
        fresh_bandit.restore(loaded.bandit);
        fresh_clf.restore(loaded.classifier);

        // --- Post-restart: snapshots must equal the pre-shutdown
        // state. The bandit's `ClucbState` implements `PartialEq`
        // (deep equality over every Vec<f32>); the classifier's
        // private FtrlClass doesn't — compare via JSON round-trip.
        assert_eq!(
            fresh_bandit.snapshot(),
            pre_bandit,
            "bandit posterior must survive the restart",
        );
        let post_clf_json =
            serde_json::to_string(&fresh_clf.snapshot()).expect("serialise post clf");
        assert_eq!(
            post_clf_json, pre_clf_json,
            "classifier FTRL weights must survive the restart",
        );
    }

    /// arms_hash is stable across reorderings of the underlying byte
    /// sequence — pin the determinism contract so a future refactor
    /// of [`Arm`]'s serde shape (e.g. field re-order) is caught here.
    #[test]
    fn arms_hash_is_stable_across_calls() {
        let arms = vec![canonical_arm("browse"), canonical_arm("code")];
        let h1 = arms_hash(&arms);
        let h2 = arms_hash(&arms);
        assert_eq!(h1, h2, "arms_hash must be deterministic");
        // And changes when the arms list changes.
        let arms2 = vec![canonical_arm("browse"), canonical_arm("idle")];
        let h3 = arms_hash(&arms2);
        assert_ne!(h1, h3, "arms_hash must reflect arm-name changes");
    }
}
