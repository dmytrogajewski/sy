//! Confidence calibration + abstain decision for hybrid search (REQ-6).
//!
//! bge-reranker-v2-m3 emits an **unbounded raw logit**, not a normalized
//! score (SPEC §2 "Reranker score semantics"): an irrelevant pair scores
//! ≈ −8.19 → sigmoid 0.00028, a strong match ≈ +5.26 → 0.9948, and a
//! logit of 0 (sigmoid 0.5) is the model's indifference boundary — the
//! natural decision point for "is there a high-confidence match here?".
//!
//! We turn the reranked top scores into a single `confidence` in [0,1] by
//! combining the top-1 sigmoid with the top1−top2 margin: a dominant
//! top-1 keeps the full sigmoid signal, while a flat distribution (top-1
//! and top-2 nearly tied) is discounted toward the indifference point.
//! Cohere's reranking guidance motivates the per-query margin: reranker
//! scores are for *ranking*, so a query-relative spread is more robust
//! than an absolute magnitude alone.
//!
//! Pure functions only; the live consumer is
//! `daemon::handle_search_rerank`, which feeds the reranked scores in and
//! abstains below the request's `abstain_threshold`.

/// Steepness of the margin discount. The margin term is
/// `sigmoid(MARGIN_GAIN * (top1 - top2))`, so a margin of 0 (a perfectly
/// flat top-2) contributes a neutral 0.5 multiplier and a wide margin
/// saturates toward 1.0. Tuned later against the Step 13 eval negatives;
/// a moderate gain keeps a ~1-logit lead already near-confident.
const MARGIN_GAIN: f32 = 1.0;

/// Logistic sigmoid: `1 / (1 + e^-x)`. Maps a raw reranker logit into
/// (0,1); `sigmoid(0) == 0.5` is the indifference boundary.
pub fn sigmoid(logit: f32) -> f32 {
    1.0 / (1.0 + (-logit).exp())
}

/// Calibrated confidence in [0,1] from the reranked top scores
/// (descending raw logits). Returns 0.0 for an empty slice (nothing to be
/// confident about). With a single hit, confidence is just its sigmoid
/// (no margin to discount). Otherwise it is `sigmoid(top1)` modulated by
/// the top1−top2 margin so a dominant top-1 → high confidence and a flat
/// distribution → lower.
pub fn confidence(top_scores: &[f32]) -> f32 {
    let Some(&top1) = top_scores.first() else {
        return 0.0;
    };
    let base = sigmoid(top1);
    match top_scores.get(1) {
        Some(&top2) => {
            let margin = sigmoid(MARGIN_GAIN * (top1 - top2));
            base * margin
        }
        None => base,
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

    /// SPEC §2: a reranker logit of 0 is the model's indifference point,
    /// which the sigmoid maps to exactly 0.5.
    #[test]
    fn sigmoid_maps_logit_zero_to_half() {
        assert!((sigmoid(0.0) - 0.5).abs() < f32::EPSILON);
    }

    /// A dominant top-1 (wide top1−top2 margin) must yield higher
    /// confidence than a flat distribution where top-1 and top-2 are tied,
    /// even at the same top-1 logit.
    #[test]
    fn confidence_rises_with_top1_margin() {
        let dominant = confidence(&[3.0, -2.0]);
        let flat = confidence(&[3.0, 3.0]);
        assert!(
            dominant > flat,
            "dominant top-1 ({dominant}) should beat a flat top-2 ({flat})"
        );
    }

    /// Below the threshold the calibrator abstains; at/above it does not.
    /// An all-negative-logit result (irrelevant matches) lands well under
    /// a 0.5 cutoff.
    #[test]
    fn abstains_below_threshold() {
        let low = confidence(&[-8.0, -9.0]);
        assert!(should_abstain(low, 0.5), "low confidence must abstain");
        let high = confidence(&[5.0, -2.0]);
        assert!(
            !should_abstain(high, 0.5),
            "high confidence must not abstain"
        );
    }
}
