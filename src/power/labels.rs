//! Self-supervised label extractor (Step 28 of the `sy-power`
//! roadmap, SPEC §3 "Self-supervised labels"; extended in Step T2 /
//! BUG-20260525-2351 to plug the 100%-Idle telemetry hole).
//!
//! Four reason-chain prefixes the daemon emits in
//! [`crate::power::daemon::one_tick`] are mapped onto an
//! [`crate::power::activity::ActivityLabel`] + a confidence weight.
//! The classifier consumes the `(label, weight)` pair via
//! `OnlineClassifier::partial_fit`; the daemon gates on `weight > 0.0`
//! so a future "label-but-don't-train" signal can be added without
//! changing the call site.
//!
//! | Reason prefix                  | Source                                   | Weight |
//! | ------------------------------ | ---------------------------------------- | ------ |
//! | `pin:<arm>`                    | `sy power profile <arm>` manual override | 1.0    |
//! | `bandit:<arm> (ucb=<f>)`       | Post-onboarding CLUCB pick               | 1.0    |
//! | `onboarding-baseline:<arm>`    | Rules baseline during the 14-day window  | 0.25   |
//! | `drift-baseline:<arm>`         | Rules baseline forced by an ADWIN alarm  | 0.25   |
//!
//! Pin always wins over baseline (dedicated first-pass loop); within
//! a single reason chain the bandit / baseline matches are
//! first-wins, but the daemon only ever emits exactly one of those
//! three prefixes per tick so the ordering is moot in practice.
//!
//! Returning `None` for an unrecognised arm keeps the classifier
//! strict — a typo in a hand-edited audit log or a config-file
//! mutation that introduces a phantom arm name skips the partial-fit
//! rather than silently miscategorising into the wrong class.

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

/// Reason-chain prefix the daemon emits every tick while the 14-day
/// onboarding window is active — the rules-baseline arm name is
/// appended (see `daemon::one_tick`'s `onboarding-baseline:<name>`
/// source label).
const ONBOARDING_BASELINE_REASON_PREFIX: &str = "onboarding-baseline:";

/// Reason-chain prefix the daemon emits when ADWIN raises a drift
/// alarm and falls back to the rules baseline (see `daemon::one_tick`'s
/// `drift-baseline:<name>` source label).
const DRIFT_BASELINE_REASON_PREFIX: &str = "drift-baseline:";

/// Reason-chain prefix the daemon emits for a post-onboarding bandit
/// pick. The full source label is `"bandit:<arm> (ucb=<float>)"`; the
/// extractor tolerates the trailing ` (ucb=...)` suffix.
const BANDIT_REASON_PREFIX: &str = "bandit:";

/// Positive label weight for a manual override. SPEC §3:
/// "manual override = positive". Magnitude is +1.0 — the strongest
/// supervision signal available, because the user just told the
/// daemon explicitly what they want.
pub const MANUAL_OVERRIDE_WEIGHT: LabelConfidence = 1.0;

/// Confidence weight for a label derived from the rules baseline
/// (onboarding-baseline / drift-baseline reason chains). The baseline
/// is a hand-coded heuristic, not ground truth — too high a weight
/// makes the classifier overfit to rules state; too low and the
/// 14-day onboarding window produces no learning. 0.25 is half of the
/// "majority class" coin-flip threshold (0.5), giving the bandit /
/// drift signals room to dominate when they fire. BUG-20260525-2351
/// §Risks pins this for future tuning.
pub const RULES_BASELINE_LABEL_WEIGHT: LabelConfidence = 0.25;

/// Confidence weight for a label derived from the post-onboarding
/// bandit's pick. The bandit's chosen arm is the strongest
/// non-pin signal we have (a calibrated CLUCB posterior), so this
/// equals [`MANUAL_OVERRIDE_WEIGHT`] in magnitude.
pub const BANDIT_LABEL_WEIGHT: LabelConfidence = 1.0;

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

/// Strip the trailing ` (ucb=<float>)` suffix the daemon appends to
/// every post-onboarding bandit reason label (see
/// `daemon::one_tick`'s `format!("bandit:{top} (ucb={score:.2})")`).
/// Returns the slice up to (but not including) the first ` (` if one
/// exists, otherwise the input unchanged.
fn strip_bandit_score_suffix(arm_with_score: &str) -> &str {
    match arm_with_score.find(" (") {
        Some(i) => &arm_with_score[..i],
        None => arm_with_score,
    }
}

/// Inspect an audit entry's reason chain for any of the four
/// daemon-emitted source-label prefixes and project the embedded arm
/// name onto an [`ActivityLabel`] with an appropriate confidence
/// weight:
///
/// - `pin:<arm>` → `(label, MANUAL_OVERRIDE_WEIGHT)` (manual user
///   intent — strongest signal).
/// - `bandit:<arm> (ucb=<f>)` → `(label, BANDIT_LABEL_WEIGHT)`
///   (post-onboarding bandit pick — same magnitude as a pin because
///   the CLUCB posterior is calibrated).
/// - `onboarding-baseline:<arm>` → `(label, RULES_BASELINE_LABEL_WEIGHT)`
///   (rules baseline during the 14-day onboarding window — weak
///   proxy).
/// - `drift-baseline:<arm>` → `(label, RULES_BASELINE_LABEL_WEIGHT)`
///   (rules baseline after a drift alarm — same magnitude as
///   onboarding-baseline since both are heuristic fallbacks).
///
/// Returns `None` when no reason prefix matches OR when the embedded
/// arm name is not in the canonical taxonomy ([`arm_to_label`] returns
/// `None`). Pin still wins over baseline because the pin path is
/// checked first.
pub fn extract_label(entry: &AuditEntry) -> Option<(ActivityLabel, LabelConfidence)> {
    for reason in &entry.reason_chain {
        if let Some(arm) = reason.strip_prefix(PIN_REASON_PREFIX) {
            return arm_to_label(arm).map(|l| (l, MANUAL_OVERRIDE_WEIGHT));
        }
    }
    for reason in &entry.reason_chain {
        if let Some(arm_with_score) = reason.strip_prefix(BANDIT_REASON_PREFIX) {
            let arm = strip_bandit_score_suffix(arm_with_score);
            return arm_to_label(arm).map(|l| (l, BANDIT_LABEL_WEIGHT));
        }
        if let Some(arm) = reason.strip_prefix(ONBOARDING_BASELINE_REASON_PREFIX) {
            return arm_to_label(arm).map(|l| (l, RULES_BASELINE_LABEL_WEIGHT));
        }
        if let Some(arm) = reason.strip_prefix(DRIFT_BASELINE_REASON_PREFIX) {
            return arm_to_label(arm).map(|l| (l, RULES_BASELINE_LABEL_WEIGHT));
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

    /// An audit entry whose reason chain contains none of the four
    /// daemon-emitted source-label prefixes (`pin:`, `bandit:`,
    /// `onboarding-baseline:`, `drift-baseline:`) yields no label.
    /// Pre-T2 this fired on a `bandit:` chain too; the new contract
    /// extracts a label from `bandit:` so the synthetic shape here
    /// uses only `shield:` / `apply:` actuator entries.
    #[test]
    fn entry_without_recognised_source_label_returns_none() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "build".to_string(),
            "COOL_AC".to_string(),
            vec!["shield:COOL_AC".to_string(), "apply:epp=power".to_string()],
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

    /// BUG-20260525-2351 / Step T2: the daemon's post-onboarding
    /// bandit reason label is `"bandit:<arm> (ucb=<f>)"`. The
    /// extractor must tolerate the trailing ` (ucb=...)` suffix and
    /// project the bare arm name; the bandit's pick is the strongest
    /// non-pin signal, so weight equals [`BANDIT_LABEL_WEIGHT`].
    #[test]
    fn extracts_code_from_bandit_reason_with_ucb_suffix() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "code".to_string(),
            "WARM_AC".to_string(),
            vec![
                "bandit:code (ucb=0.42)".to_string(),
                "shield:WARM_AC".to_string(),
            ],
            vec![],
            0.05,
        );
        let (label, weight) = extract_label(&entry).expect("bandit pick yields a label");
        assert_eq!(label, ActivityLabel::Code);
        assert!(
            (weight - BANDIT_LABEL_WEIGHT).abs() < f32::EPSILON,
            "weight {weight} should equal BANDIT_LABEL_WEIGHT",
        );
    }

    /// BUG-20260525-2351 / Step T2: the daemon emits
    /// `"onboarding-baseline:<arm>"` on every tick during the 14-day
    /// onboarding window. Treat it as a weak self-supervision label so
    /// the classifier learns something instead of staying at the
    /// zero-init "everything is Idle" attractor.
    #[test]
    fn extracts_browse_from_onboarding_baseline_reason() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "browse".to_string(),
            "COOL_AC".to_string(),
            vec![
                "onboarding-baseline:browse".to_string(),
                "shield:COOL_AC".to_string(),
            ],
            vec![],
            0.05,
        );
        let (label, weight) =
            extract_label(&entry).expect("onboarding-baseline yields a weak label");
        assert_eq!(label, ActivityLabel::Browse);
        assert!(
            (weight - RULES_BASELINE_LABEL_WEIGHT).abs() < f32::EPSILON,
            "weight {weight} should equal RULES_BASELINE_LABEL_WEIGHT",
        );
    }

    /// BUG-20260525-2351 / Step T2: a `"drift-baseline:whisper"`
    /// reason chain (ADWIN drift alarm forced the rules baseline and
    /// the baseline picked the `whisper` power profile) maps onto
    /// [`ActivityLabel::Idle`] — `whisper` is a power profile, not an
    /// activity, and the shared arm→activity taxonomy folds it into
    /// Idle. Weight matches the onboarding-baseline path because both
    /// are rules-baseline-derived heuristics.
    #[test]
    fn extracts_idle_from_drift_baseline_whisper() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "whisper".to_string(),
            "COOL_AC".to_string(),
            vec![
                "drift-baseline:whisper".to_string(),
                "shield:COOL_AC".to_string(),
            ],
            vec![],
            0.05,
        );
        let (label, weight) = extract_label(&entry).expect("drift-baseline yields a weak label");
        assert_eq!(label, ActivityLabel::Idle);
        assert!(
            (weight - RULES_BASELINE_LABEL_WEIGHT).abs() < f32::EPSILON,
            "weight {weight} should equal RULES_BASELINE_LABEL_WEIGHT",
        );
    }

    /// BUG-20260525-2351 / Step T2: a manual pin always dominates the
    /// rules-baseline fallback because the user just told the daemon
    /// what they want. The extractor walks pins on a dedicated first
    /// pass so reason-chain order can't accidentally invert the
    /// priority.
    #[test]
    fn pin_still_wins_over_baseline() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "build".to_string(),
            "COOL_AC".to_string(),
            // Reason chain intentionally lists the baseline first so a
            // naive "first-match-wins" implementation would pick the
            // baseline; the new extractor's two-pass loop ensures pin
            // wins regardless of order.
            vec![
                "onboarding-baseline:browse".to_string(),
                "pin:build".to_string(),
                "shield:COOL_AC".to_string(),
            ],
            vec![],
            0.05,
        );
        let (label, weight) = extract_label(&entry).expect("pin dominates baseline");
        assert_eq!(label, ActivityLabel::Build);
        assert!(
            (weight - MANUAL_OVERRIDE_WEIGHT).abs() < f32::EPSILON,
            "weight {weight} should equal MANUAL_OVERRIDE_WEIGHT (pin precedence)",
        );
    }

    /// BUG-20260525-2351 / Step T2: an unrecognised arm name in any of
    /// the three new baseline / bandit prefix shapes returns `None`,
    /// matching the strictness of the existing pin path. Prevents a
    /// silent miscategorisation when an operator hand-edits the audit
    /// log or a future config-file typo introduces a phantom arm.
    #[test]
    fn unrecognised_baseline_arm_returns_none() {
        let entry = AuditEntry::r3(
            empty_snapshot(),
            "imaginary".to_string(),
            "COOL_AC".to_string(),
            vec![
                "onboarding-baseline:imaginary".to_string(),
                "shield:COOL_AC".to_string(),
            ],
            vec![],
            0.05,
        );
        assert!(extract_label(&entry).is_none());
    }

    /// BUG-20260525-2351 / Step T2 acceptance: replay 60 synthetic
    /// audit entries through `extract_label` + `OnlineClassifier::
    /// partial_fit` (mirroring the daemon's `one_tick` partial_fit
    /// gate at `daemon.rs:881-885`). Assert that after the replay the
    /// classifier produces ≥ 2 distinct labels across a small set of
    /// probe snapshots — proving the new label paths actually drive
    /// learning instead of leaving the all-zero "everything is Idle"
    /// attractor in place. Smaller-than-roadmap-spec NDJSON harness;
    /// the larger `tests/power_classifier_learns_from_telemetry.rs`
    /// integration would require >30 LoC of `one_tick` scaffolding
    /// per the orchestrator's "prefer the smaller path" note.
    #[test]
    fn replay_through_partial_fit_yields_multiple_classes() {
        use crate::power::activity::{ActivityLabel, OnlineClassifier};
        use crate::power::snapshot::FEATURE_LEN;

        const ROWS_PER_CLASS: usize = 12;
        const EPOCHS: usize = 8;

        // Three well-separated synthetic feature centres, one per
        // activity class. The `applied_arm` is irrelevant — what the
        // classifier learns from is `extract_label`'s output.
        let cases: &[(&str, ActivityLabel, usize)] = &[
            ("onboarding-baseline:browse", ActivityLabel::Browse, 1),
            ("bandit:code (ucb=0.42)", ActivityLabel::Code, 3),
            ("drift-baseline:build", ActivityLabel::Build, 4),
        ];

        let mut clf = OnlineClassifier::new();
        for _ in 0..EPOCHS {
            for (reason, _expected_label, spike_dim) in cases {
                for _ in 0..ROWS_PER_CLASS {
                    let mut features = [0.0_f32; FEATURE_LEN];
                    features[*spike_dim] = 1.0;
                    let mut snap = empty_snapshot();
                    snap.features = features;
                    let entry = AuditEntry::r3(
                        snap.clone(),
                        "browse".to_string(),
                        "COOL_AC".to_string(),
                        vec![(*reason).to_string(), "shield:COOL_AC".to_string()],
                        vec![],
                        0.05,
                    );
                    let (label, weight) = extract_label(&entry)
                        .expect("synthetic reason chains all match the new label paths");
                    if weight > 0.0 {
                        clf.partial_fit(&snap, label);
                    }
                }
            }
        }

        // Probe the trained classifier with each spike vector; assert
        // ≥ 2 distinct labels appear across the probes (the bug
        // manifests as "every label collapses to Idle").
        let mut observed: std::collections::HashSet<ActivityLabel> =
            std::collections::HashSet::new();
        for (_, _, spike_dim) in cases {
            let mut features = [0.0_f32; FEATURE_LEN];
            features[*spike_dim] = 1.0;
            let mut snap = empty_snapshot();
            snap.features = features;
            observed.insert(clf.classify(&snap));
        }
        assert!(
            observed.len() >= 2,
            "classifier collapsed to {observed:?}; T2 requires ≥ 2 distinct labels after replay",
        );
    }
}
