//! Confidence calibration + abstain decision for hybrid search (REQ-6).
//!
//! **Score contract (load-bearing).** The scores fed to [`confidence`] come
//! from the rerank workload, and its ONNX export bakes `sigmoid(logits[..])`
//! into the graph — they are **already probabilities in `[0, 1]`**, as
//! [`crate::aiplane::workloads::rerank`]'s module docs state. An earlier
//! revision of this module assumed raw logits and applied a second sigmoid on
//! the theory that "logit 0 is the indifference boundary" (SPEC §2 quotes the
//! raw-logit card values −8.19 → 0.00028 and +5.26 → 0.9948). Sigmoids do not
//! compose: `sigmoid(sigmoid(x))` for every reachable `x ∈ [0,1]` lands in
//! `[0.500, 0.731]`, and multiplying by the tied-rival term squashed the whole
//! confidence scale into `[0.25, 0.54]`. The documented `0.5` cutoff then sat
//! *inside* the range of correct answers. Measured on the live corpus, 4 of 15
//! answerable golden queries were suppressed — the caller received
//! `results: []` while the gold chunk sat at rank 1 with a 0.96 relevance
//! score (BUG-20260927-1910). The SPEC's *intent* ("confidence from the
//! calibrated reranker sigmoid and the top1−top2 margin") is satisfied by
//! consuming the probability directly; the double application was the defect.
//!
//! Calibration is therefore the top-1 probability itself, discounted by up to
//! half when the top-2 rival is statistically tied with it. A decisive lead
//! keeps the full probability; a coin-flip pair is halved, which preserves the
//! discrimination the old formula achieved *by accident* on irrelevant
//! candidates (they score ≈ 0.0, so they were never near the cutoff anyway).
//!
//! Pure functions only; the live consumer is
//! [`crate::knowledge::daemon`]'s search handler, which feeds the reranked
//! scores in and abstains below the request's `abstain_threshold`.

/// Steepness of the margin discount, in probability space. A top1−top2 lead
/// of 0.5 (a decisive gap on a `[0,1]` scale) already saturates the discount
/// at ≈ 0.96 of the top-1 probability, while an exact tie sits at the neutral
/// half. Moderate on purpose: the margin is a tie-detector, not a second
/// score.
const MARGIN_GAIN: f32 = 2.0;

/// Weight a perfectly tied rival contributes — a 50 % discount, the point of
/// no information about which of the top two is right.
const NEUTRAL_MARGIN: f32 = 0.5;

/// Reranked top scores are relevance probabilities, so a mis-exported graph
/// that leaks outside `[0, 1]` is clamped rather than trusted.
fn probability(score: f32) -> f32 {
    score.clamp(0.0, 1.0)
}

/// Discount for the top1−top2 margin, in `(0, 1)`: 0.5 at an exact tie,
/// rising to ≈ 1 for a decisive lead.
fn margin_weight(margin: f32) -> f32 {
    NEUTRAL_MARGIN * (1.0 + (MARGIN_GAIN * margin).tanh())
}

/// Calibrated confidence in `[0,1]` from the reranked top scores (descending
/// relevance probabilities). Returns `0.0` for an empty slice — nothing was
/// retrieved, so nothing can be asserted. With a single hit, confidence is
/// that hit's probability (no rival to be ambiguous against). Otherwise it is
/// the top-1 probability discounted toward a half share when the rival is
/// tied with it, per [`margin_weight`].
pub fn confidence(top_scores: &[f32]) -> f32 {
    let Some(&top1) = top_scores.first() else {
        return 0.0;
    };
    let top1 = probability(top1);
    match top_scores.get(1) {
        Some(&top2) => top1 * margin_weight(top1 - probability(top2)),
        None => top1,
    }
}

/// REQ-6 abstain decision: abstain when calibrated `confidence` is
/// strictly below `threshold`.
pub fn should_abstain(confidence: f32, threshold: f32) -> bool {
    confidence < threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `[0,1]` contract, pinned: a decisive top-1 maps to (nearly) its own
    /// probability. Under the superseded double-sigmoid this returned 0.53 for
    /// the same input — the compression that hid real answers.
    #[test]
    fn decisive_top1_confidence_is_its_own_probability() {
        let c = confidence(&[0.96, 0.02]);
        assert!(
            c > 0.90,
            "a 0.96 top-1 with a 0.94 lead must not collapse (got {c})"
        );
    }

    /// Regression guard for BUG-20260927-1910: a confident answer whose rival
    /// is close behind must still clear the documented 0.5 cutoff, so the
    /// policy never answers "no high-confidence match" about a chunk that is
    /// sitting at rank 1 with a 0.96 score.
    #[test]
    fn confident_near_tie_is_not_suppressed() {
        let c = confidence(&[0.96, 0.59]);
        assert!(
            !should_abstain(c, 0.5),
            "confident near-tie must be answered (got {c})"
        );
    }

    /// Irrelevant candidates score ≈ 0.0 on this scale, so noise stays far
    /// below the cutoff — the discrimination the old formula produced only by
    /// accident must survive the fix.
    #[test]
    fn irrelevant_candidates_abstain() {
        assert!(
            should_abstain(confidence(&[0.003, 0.001]), 0.5),
            "noise must abstain"
        );
        assert!(
            should_abstain(confidence(&[]), 0.5),
            "an empty result set must abstain"
        );
    }

    /// A top-1 sitting on the 0.5 indifference probability with a tied rival
    /// is exactly the coin flip abstention exists for.
    #[test]
    fn tied_coin_flip_abstains() {
        let c = confidence(&[0.5, 0.5]);
        assert!(
            should_abstain(c, 0.5),
            "tied coin flip must abstain (got {c})"
        );
    }

    /// At an equal top-1, a wide lead beats a flat top-2 (the REQ-6 margin
    /// term, unchanged in spirit).
    #[test]
    fn confidence_rises_with_top1_margin() {
        let dominant = confidence(&[0.8, 0.1]);
        let flat = confidence(&[0.8, 0.8]);
        assert!(
            dominant > flat,
            "dominant top-1 ({dominant}) should beat a flat top-2 ({flat})"
        );
    }

    /// Confidence is monotonic in the top-1 probability and bounded by it, so
    /// the value stays readable as "how much the reranker believes rank 1".
    #[test]
    fn confidence_is_bounded_by_the_top1_probability() {
        for score in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!(
                confidence(&[score, 0.0]) <= score + f32::EPSILON,
                "confidence {score} exceeded its top-1 probability"
            );
        }
    }

    /// Scores from a mis-exported graph that leave `[0,1]` are clamped: the
    /// envelope must never report more than certain, nor go negative.
    #[test]
    fn out_of_range_scores_are_clamped() {
        assert!(confidence(&[1.4]) <= 1.0, "must not exceed certainty");
        assert!(
            confidence(&[-0.2, 0.1]).abs() < f32::EPSILON,
            "a clamped-to-zero top-1 must be zero confidence"
        );
    }

    /// Below the threshold the calibrator abstains; at/above it does not
    /// (strict comparison is the documented boundary).
    #[test]
    fn abstains_below_threshold_only() {
        assert!(should_abstain(0.49, 0.5), "below must abstain");
        assert!(
            !should_abstain(0.5, 0.5),
            "exactly at the threshold must not abstain"
        );
        assert!(!should_abstain(0.9, 0.5), "above must not abstain");
    }
}
