//! Deterministic retrieval-eval metrics + golden-set runner (REQ-9).
//!
//! Pure, I/O-free metric computation lives here so it can be unit-tested
//! with fixture rankings; the live `sy knowledge eval` command
//! ([`crate::knowledge::cli::eval_cmd`]) is the production consumer — it
//! loads the checked-in `queries.jsonl`, runs each query through the
//! daemon search path, and feeds the resulting rankings into [`metrics()`].
//!
//! Definitions (single-gold labels):
//! - `recall_at_1` / `recall_at_5` — hit-rate@k, averaged over the
//!   *answerable* queries (an unanswerable query has no gold to recall).
//!   For single-gold queries recall@k is exactly hit-rate@k.
//! - `mrr` — mean reciprocal rank of the first relevant hit within the
//!   top [`RECALL_K`] (0 if the gold is absent), over answerable queries.
//! - `abstain_accuracy` — SQuAD-2.0 style over the FULL set: a query is
//!   *correct* when an answerable query surfaced its gold OR an
//!   unanswerable query abstained (true-positive + true-negative) / n.

use serde::{Deserialize, Serialize};

/// Reciprocal-rank window: hits beyond rank 5 contribute 0 to recall@5.
pub const RECALL_K: usize = 5;

/// One labelled golden-set row (a JSONL line in `queries.jsonl`).
#[derive(Debug, Clone, Deserialize)]
pub struct LabelledQuery {
    /// The natural-language query to run.
    pub query: String,
    /// Gold chunk id or a representative substring expected in a hit.
    #[serde(default)]
    pub expected: String,
    /// Whether the corpus actually contains an answer (SQuAD-2.0 style).
    pub answerable: bool,
    /// Optional source-kind hint (documentation / category bookkeeping).
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional inclusive date bounds the query implies (RFC-3339).
    #[serde(default)]
    pub date_from: Option<String>,
    #[serde(default)]
    pub date_to: Option<String>,
}

/// Per-query ranked outcome fed into [`metrics()`]: the ranked chunk
/// ids/text (best first) plus whether the search abstained.
#[derive(Debug, Clone, Default)]
pub struct RankedResult {
    /// Ranked hit identifiers/text, best first. A gold "match" is a
    /// substring containment of `expected` in any entry.
    pub ids: Vec<String>,
    /// Whether the search returned a confident-abstain (no results).
    pub abstained: bool,
}

/// Aggregate retrieval metrics over a labelled set (REQ-9 `--json` shape).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Metrics {
    pub recall_at_1: f64,
    pub recall_at_5: f64,
    pub mrr: f64,
    pub abstain_accuracy: f64,
    pub n: usize,
}

/// True when `expected` is found at `ids[rank]` (substring containment).
fn hit_at(expected: &str, ids: &[String], rank: usize) -> bool {
    ids.get(rank).is_some_and(|id| id.contains(expected))
}

/// 1-based rank of the first relevant hit, or `None` if absent.
fn first_relevant_rank(expected: &str, ids: &[String]) -> Option<usize> {
    ids.iter()
        .position(|id| id.contains(expected))
        .map(|i| i + 1)
}

/// Compute the aggregate metrics. `labelled` and `ranked` are parallel
/// (one ranked outcome per labelled query); mismatched lengths are
/// truncated to the shorter so the function stays total.
pub fn metrics(labelled: &[LabelledQuery], ranked: &[RankedResult]) -> Metrics {
    let n = labelled.len().min(ranked.len());
    let mut answerable = 0usize;
    let mut r1 = 0usize;
    let mut r5 = 0usize;
    let mut rr = 0.0f64;
    let mut correct = 0usize;
    for (q, res) in labelled.iter().zip(ranked.iter()).take(n) {
        if q.answerable {
            answerable += 1;
            if hit_at(&q.expected, &res.ids, 0) {
                r1 += 1;
            }
            if let Some(rank) = first_relevant_rank(&q.expected, &res.ids) {
                if rank <= RECALL_K {
                    r5 += 1;
                    rr += 1.0 / rank as f64;
                    correct += 1;
                }
            }
        } else if res.abstained {
            correct += 1;
        }
    }
    let over_ans = |c: usize| {
        if answerable == 0 {
            0.0
        } else {
            c as f64 / answerable as f64
        }
    };
    let mrr = if answerable == 0 {
        0.0
    } else {
        rr / answerable as f64
    };
    Metrics {
        recall_at_1: over_ans(r1),
        recall_at_5: over_ans(r5),
        mrr,
        abstain_accuracy: if n == 0 {
            0.0
        } else {
            correct as f64 / n as f64
        },
        n,
    }
}

/// Per-metric regression floor for CI gating (REQ-9). A run that falls
/// below any floor is a regression → non-zero exit.
#[derive(Debug, Clone, Copy)]
pub struct Tolerance {
    pub min_recall_at_1: f64,
    pub min_recall_at_5: f64,
    pub min_mrr: f64,
    pub min_abstain_accuracy: f64,
}

impl Tolerance {
    /// First metric that dropped below its floor, if any (for an
    /// actionable error message).
    pub fn regression(&self, m: &Metrics) -> Option<String> {
        let checks = [
            ("recall_at_1", m.recall_at_1, self.min_recall_at_1),
            ("recall_at_5", m.recall_at_5, self.min_recall_at_5),
            ("mrr", m.mrr, self.min_mrr),
            (
                "abstain_accuracy",
                m.abstain_accuracy,
                self.min_abstain_accuracy,
            ),
        ];
        checks.iter().find_map(|(name, got, floor)| {
            (got < floor).then(|| format!("{name} {got:.3} < tolerance {floor:.3}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(query: &str, expected: &str, answerable: bool) -> LabelledQuery {
        LabelledQuery {
            query: query.to_string(),
            expected: expected.to_string(),
            answerable,
            kind: None,
            date_from: None,
            date_to: None,
        }
    }

    fn ranked(ids: &[&str]) -> RankedResult {
        RankedResult {
            ids: ids.iter().map(|s| s.to_string()).collect(),
            abstained: false,
        }
    }

    #[test]
    fn recall_and_mrr_match_known_rankings() {
        // q0: gold at rank 1 → r@1, r@5, rr=1.0
        // q1: gold at rank 3 → r@5, rr=1/3
        // q2: gold absent within k → no recall, rr=0
        let labelled = [
            q("a", "gold-a", true),
            q("b", "gold-b", true),
            q("c", "gold-c", true),
        ];
        let results = [
            ranked(&["gold-a", "x", "y"]),
            ranked(&["x", "y", "gold-b"]),
            ranked(&["x", "y", "z", "w", "v", "gold-c"]),
        ];
        let m = metrics(&labelled, &results);
        assert_eq!(m.recall_at_1, 1.0 / 3.0);
        assert_eq!(m.recall_at_5, 2.0 / 3.0);
        assert!((m.mrr - (1.0 + 1.0 / 3.0) / 3.0).abs() < 1e-9);
        assert_eq!(m.n, 3);
    }

    #[test]
    fn abstain_accuracy_counts_true_negatives() {
        // TP: answerable + gold surfaced. TN: unanswerable + abstained.
        // FP: unanswerable + answered. FN: answerable + abstained/missed.
        let labelled = [
            q("tp", "gold", true),
            q("tn", "", false),
            q("fp", "", false),
            q("fn", "gold", true),
        ];
        let results = [
            ranked(&["gold"]), // TP
            RankedResult {
                ids: vec![],
                abstained: true,
            }, // TN
            ranked(&["noise"]), // FP (answered)
            RankedResult {
                ids: vec![],
                abstained: true,
            }, // FN (abstained on answerable)
        ];
        let m = metrics(&labelled, &results);
        // 2 correct (TP + TN) out of 4.
        assert_eq!(m.abstain_accuracy, 0.5);
    }
}
