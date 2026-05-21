//! Self-supervised label extractor (Step 28 of the `sy-power`
//! roadmap, SPEC §3 "Self-supervised labels").
//!
//! Three labelled-signal sources feed the [`activity::OnlineClassifier`]:
//!
//! 1. **Manual override → positive `+1.0`.** When the user pins a
//!    profile with `sy power profile <name>`, the daemon's reason
//!    chain (Step 22) carries `"pin:<arm>"`. The arm maps onto an
//!    [`crate::power::activity::ActivityLabel`] via the same
//!    arm→activity mapping the offline trainer uses
//!    ([`crate::power::trainer`] module head). This path is what
//!    Step 28 ships.
//! 2. **Throttling event → coarse negative `-0.5`.** Future work
//!    (Step 31+ adds the drift detector + throttle observer).
//! 3. **Battery-drain residual vs TOD prediction → signed.** Future
//!    work; the residual stream is produced by the GRU forecaster's
//!    drift hook in Step 31+.
//!
//! Today [`extract_label`] only implements path (1). Paths (2) and
//! (3) return `None` so the caller keeps making progress; the
//! signatures are stable so later steps can extend them without a
//! breaking change.

use crate::power::activity::ActivityLabel;
use crate::power::log::AuditEntry;

/// Confidence weight returned alongside the label. The classifier
/// only uses the sign today (`partial_fit` treats it as 0/1), but
/// the magnitude is preserved so the future drift-residual path
/// (SPEC §3, Step 31+) can pass through a calibrated weight.
pub type LabelConfidence = f32;

/// Reason-chain prefix the daemon emits for a manual pin (see
/// `daemon::one_tick`'s `pin:<name>` source label).
const PIN_REASON_PREFIX: &str = "pin:";

/// Positive label weight for a manual override. SPEC §3:
/// "manual override = positive". Magnitude is +1.0 — the strongest
/// supervision signal available, because the user just told the
/// daemon explicitly what they want.
pub const MANUAL_OVERRIDE_WEIGHT: LabelConfidence = 1.0;

/// Project a canonical arm name onto an [`ActivityLabel`] using the
/// same taxonomy as [`crate::power::trainer`]'s `arm_to_class_idx`:
///
/// - `idle` / `whisper` → [`ActivityLabel::Idle`]
/// - `browse`           → [`ActivityLabel::Browse`]
/// - `call`             → [`ActivityLabel::Call`]
/// - `code`             → [`ActivityLabel::Code`]
/// - `build` / `flat-out` / `npu-burst` → [`ActivityLabel::Build`]
///
/// Returns `None` for unknown arm names so the caller surfaces "no
/// label" rather than silently miscategorising.
pub fn arm_to_label(arm: &str) -> Option<ActivityLabel> {
    match arm {
        "idle" | "whisper" => Some(ActivityLabel::Idle),
        "browse" => Some(ActivityLabel::Browse),
        "call" => Some(ActivityLabel::Call),
        "code" => Some(ActivityLabel::Code),
        "build" | "flat-out" | "npu-burst" => Some(ActivityLabel::Build),
        _ => None,
    }
}

/// Inspect an audit entry's reason chain for a manual pin
/// (`"pin:<arm>"`); if present, project the arm name onto an
/// [`ActivityLabel`] and return `(label, +1.0)`. Returns `None` for
/// every other audit entry shape — throttling / drain-residual paths
/// land here in Step 31+.
pub fn extract_label(entry: &AuditEntry) -> Option<(ActivityLabel, LabelConfidence)> {
    for reason in &entry.reason_chain {
        if let Some(arm) = reason.strip_prefix(PIN_REASON_PREFIX) {
            return arm_to_label(arm).map(|l| (l, MANUAL_OVERRIDE_WEIGHT));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::log::AuditEntry;
    use crate::power::snapshot::{Snapshot, SnapshotRaw, FEATURE_LEN, SCHEMA_ID};
    use chrono::{TimeZone, Utc};

    fn empty_snapshot() -> Snapshot {
        Snapshot {
            schema: SCHEMA_ID,
            ts: Utc
                .with_ymd_and_hms(2026, 5, 19, 12, 0, 0)
                .single()
                .unwrap(),
            features: [0.0; FEATURE_LEN],
            raw: SnapshotRaw::default(),
            snapshot_hash: "0".repeat(64),
        }
    }

    /// Step 28 DoD: an `AuditEntry` whose reason chain mentions
    /// `"pin:build"` (manual override, build arm) emits
    /// `Some((Build, +1.0))`. This is the canonical positive-label
    /// path SPEC §3 documents.
    #[test]
    fn manual_override_emits_positive_label() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "build".to_string(),
            "COOL_AC".to_string(),
            vec!["pin:build".to_string(), "shield:COOL_AC".to_string()],
            vec![],
            0.05,
        );
        let (label, weight) = extract_label(&entry).expect("manual pin yields a label");
        assert_eq!(label, ActivityLabel::Build);
        assert!(
            (weight - MANUAL_OVERRIDE_WEIGHT).abs() < f32::EPSILON,
            "weight {weight} should be +1.0",
        );
    }

    /// A `pin:whisper` manual override maps onto `Idle` per the
    /// shared arm→activity taxonomy (the offline trainer uses the
    /// same mapping). Pins the multi-arm aliases so a refactor that
    /// drops `whisper` from the table trips this test.
    #[test]
    fn pin_whisper_maps_to_idle() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "whisper".to_string(),
            "COOL_AC".to_string(),
            vec!["pin:whisper".to_string()],
            vec![],
            0.05,
        );
        let (label, _) = extract_label(&entry).expect("whisper pin yields idle");
        assert_eq!(label, ActivityLabel::Idle);
    }

    /// SPEC §3 explicitly lists throttling and drain-residual as
    /// future-work paths; Step 28 returns `None` for them so the
    /// caller treats "no label" as "skip the partial_fit" without
    /// branching on a sentinel enumerant.
    #[test]
    fn entry_without_manual_pin_returns_none() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "build".to_string(),
            "COOL_AC".to_string(),
            vec![
                "bandit:build (ucb=0.42)".to_string(),
                "shield:COOL_AC".to_string(),
            ],
            vec![],
            0.05,
        );
        assert!(extract_label(&entry).is_none());
    }

    /// An unrecognised arm in `pin:<arm>` (e.g. an operator
    /// hand-edits the audit log) returns `None`. The classifier must
    /// never silently miscategorise a typo.
    #[test]
    fn unknown_arm_returns_none() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "imaginary".to_string(),
            "COOL_AC".to_string(),
            vec!["pin:imaginary".to_string()],
            vec![],
            0.05,
        );
        assert!(extract_label(&entry).is_none());
    }
}
