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
/// ids/text (best first) plus the abstain decision and the calibrated
/// confidence it was derived from.
///
/// Contract (BUG-20260927-1910): `ids` must carry the search path's ranking
/// with **abstention disabled**. The daemon answers an abstained request with
/// an *empty* hit list, so measuring recall on abstained responses folds the
/// calibration decision into the retrieval metric — a near-tie top-2 then
/// reads as "the retriever never found the gold chunk" instead of "the
/// policy chose not to show it". `abstained` is recomputed from `confidence`
/// by the caller so both axes stay separately observable.
#[derive(Debug, Clone, Default)]
pub struct RankedResult {
    /// Ranked hit identifiers/text, best first. A gold "match" is a
    /// substring containment of `expected` in any entry.
    pub ids: Vec<String>,
    /// Whether the abstain policy would suppress this response.
    pub abstained: bool,
    /// Calibrated confidence (REQ-6) reported by the search path, 0.0 when
    /// the path returned none.
    pub confidence: f32,
}

/// Aggregate retrieval metrics over a labelled set (REQ-9 `--json` shape).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Metrics {
    pub recall_at_1: f64,
    pub recall_at_5: f64,
    pub mrr: f64,
    pub abstain_accuracy: f64,
    /// Fraction of *answerable* queries whose gold the retriever found but
    /// the abstain policy suppressed. See [`metrics`].
    pub false_abstain_rate: f64,
    pub n: usize,
}

/// True when `expected` is found at `ids[rank]` (substring containment).
fn hit_at(expected: &str, ids: &[String], rank: usize) -> bool {
    ids.get(rank).is_some_and(|id| relevant(expected, id))
}

/// 1-based rank of the first relevant hit, or `None` if absent.
fn first_relevant_rank(expected: &str, ids: &[String]) -> Option<usize> {
    ids.iter()
        .position(|id| relevant(expected, id))
        .map(|i| i + 1)
}

/// Substring containment with one deliberate exception: an **empty** gold
/// string matches nothing. Unanswerable golden-set rows carry `expected: ""`
/// (there is no answer to name) and `str::contains("")` is `true` for every
/// string, so without this guard every such row looked like a rank-1 hit —
/// a phantom gold in the `per_query` diagnostic, and an outright recall
/// inflation for any answerable row whose label went missing.
fn relevant(expected: &str, id: &str) -> bool {
    !expected.is_empty() && id.contains(expected)
}

/// Compute the aggregate metrics. `labelled` and `ranked` are parallel
/// (one ranked outcome per labelled query); mismatched lengths are
/// truncated to the shorter so the function stays total.
///
/// `false_abstain_rate` counts answerable queries whose gold IS in the top
/// [`RECALL_K`] but whose response the abstain policy suppressed. It exists
/// because recall and abstention are independent axes and conflating them
/// hid a real defect (BUG-20260927-1910): the confidence calibrator
/// double-sigmoied the rerank scores, so a correct answer could be answered
/// with an empty result set and the suite only reported "recall 0.40" — a
/// retrieval number — while the retriever had in fact found every one of
/// those answers at rank 1.
pub fn metrics(labelled: &[LabelledQuery], ranked: &[RankedResult]) -> Metrics {
    let n = labelled.len().min(ranked.len());
    let mut answerable = 0usize;
    let mut r1 = 0usize;
    let mut r5 = 0usize;
    let mut rr = 0.0f64;
    let mut correct = 0usize;
    let mut suppressed = 0usize;
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
                // The retriever found the answer and the policy hid it: the
                // user sees "no high-confidence match" about a chunk that was
                // sitting in the ranking. Recall cannot express this (it
                // scores the ranking), so it needs its own axis.
                if res.abstained {
                    suppressed += 1;
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
        false_abstain_rate: over_ans(suppressed),
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

/// One row of the per-query diagnostic (`eval --json`'s `per_query` array).
///
/// The aggregate metrics deliberately hide *which* query regressed; a
/// recall-only report cannot distinguish "the retriever lost the gold chunk"
/// from "the abstain policy suppressed a correct answer". Surfacing the rank
/// alongside the confidence and the abstain decision makes that distinction
/// observable from a single `make eval` run.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QueryOutcome {
    /// The query text, for locating the row in `queries.jsonl`.
    pub query: String,
    /// Golden-set label: does the corpus contain the answer?
    pub answerable: bool,
    /// 1-based rank of the first relevant hit, `None` when absent.
    pub rank: Option<usize>,
    /// Calibrated confidence from the search path.
    pub confidence: f32,
    /// Whether the abstain policy suppresses the response at the runner's
    /// threshold.
    pub abstained: bool,
}

/// Per-query [`QueryOutcome`] rows, parallel to `labelled` (truncated to the
/// shorter input so the function stays total, like [`metrics`]).
pub fn per_query(labelled: &[LabelledQuery], ranked: &[RankedResult]) -> Vec<QueryOutcome> {
    labelled
        .iter()
        .zip(ranked.iter())
        .take(labelled.len().min(ranked.len()))
        .map(|(q, res)| QueryOutcome {
            query: q.query.clone(),
            answerable: q.answerable,
            rank: first_relevant_rank(&q.expected, &res.ids),
            confidence: res.confidence,
            abstained: res.abstained,
        })
        .collect()
}

/// Per-metric regression floor for the retrieval gate (REQ-9). A run that
/// falls below any floor is a regression → `sy knowledge eval` exits 3
/// (drift, SPEC §4.7), which is what `make eval` reports.
///
/// Honest scope: this gate is *not* wired into GitHub Actions — the golden
/// set scores against the live `sy-knowledge` index, which CI has no
/// access to. `.github/workflows/docs.yml` runs `make lint` +
/// `make docs-lint` + `cargo test --doc` only. Treat `make eval` as the
/// pre-push / on-host gate and re-baseline the floors here whenever the
/// corpus or the chunker contract changes (see BUG-20260927-1215.md for
/// why the current floors sit where they do).
#[derive(Debug, Clone, Copy)]
pub struct Tolerance {
    pub min_recall_at_1: f64,
    pub min_recall_at_5: f64,
    pub min_mrr: f64,
    pub min_abstain_accuracy: f64,
    /// Ceiling (not floor) on [`Metrics::false_abstain_rate`].
    pub max_false_abstain_rate: f64,
}

impl Tolerance {
    /// First metric that dropped below its floor, if any (for an
    /// actionable error message).
    pub fn regression(&self, m: &Metrics) -> Option<String> {
        let floors: [(&str, f64, f64); 4] = [
            ("recall_at_1", m.recall_at_1, self.min_recall_at_1),
            ("recall_at_5", m.recall_at_5, self.min_recall_at_5),
            ("mrr", m.mrr, self.min_mrr),
            (
                "abstain_accuracy",
                m.abstain_accuracy,
                self.min_abstain_accuracy,
            ),
        ];
        if let Some((name, got, floor)) = floors.into_iter().find(|(_, got, floor)| got < floor) {
            return Some(format!("{name} {got:.3} < tolerance {floor:.3}"));
        }
        if m.false_abstain_rate > self.max_false_abstain_rate {
            return Some(format!(
                "false_abstain_rate {:.3} > tolerance {:.3} (correct answers hidden by the abstain policy)",
                m.false_abstain_rate, self.max_false_abstain_rate
            ));
        }
        None
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
            confidence: 0.9,
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

    /// An unanswerable row has no gold to find (`expected: ""`) and every
    /// string "contains" the empty string. Without the guard each such row
    /// reported a phantom rank-1 hit.
    #[test]
    fn empty_gold_never_matches_a_hit() {
        let labelled = [q("no answer", "", false), q("labeled", "gold", true)];
        let results = [ranked(&["totally unrelated noise"]), ranked(&["gold here"])];
        let rows = per_query(&labelled, &results);
        assert_eq!(rows[0].rank, None, "empty gold must not match a hit");
        assert_eq!(rows[1].rank, Some(1));
        // Nor may it count as a recalled answer for an answerable row.
        let only_empty = [q("no answer", "", true)];
        let noise_ranking = [ranked(&["noise"])];
        assert_eq!(metrics(&only_empty, &noise_ranking).recall_at_1, 0.0);
    }

    /// The axis that separates "the retriever missed it" from "the policy hid
    /// it": a found-but-suppressed answer raises `false_abstain_rate` while
    /// leaving recall untouched (BUG-20260927-1910).
    #[test]
    fn false_abstain_rate_counts_found_but_suppressed_answers() {
        let labelled = [
            q("found and shown", "gold", true),
            q("found but hidden", "gold", true),
            q("never found", "gold", true),
        ];
        let results = [
            ranked(&["gold"]),
            RankedResult {
                ids: vec!["gold".into()],
                abstained: true,
                confidence: 0.49,
            },
            ranked(&["noise"]),
        ];
        let m = metrics(&labelled, &results);
        assert!(
            (m.false_abstain_rate - 1.0 / 3.0).abs() < 1e-9,
            "one of three suppressed, got {}",
            m.false_abstain_rate
        );
        // Two of the three surfaced their gold at rank 1 — including the
        // suppressed one, which recall cannot see being hidden.
        assert_eq!(m.recall_at_1, 2.0 / 3.0, "recall is scored on the ranking");
        // And the gate reads it as a ceiling, not a floor.
        let tol = Tolerance {
            min_recall_at_1: 0.0,
            min_recall_at_5: 0.0,
            min_mrr: 0.0,
            min_abstain_accuracy: 0.0,
            max_false_abstain_rate: 0.1,
        };
        assert!(tol
            .regression(&m)
            .expect("regression")
            .contains("false_abstain_rate"));
    }

    /// The per-query diagnostic must expose rank + confidence + abstain
    /// independently: a gold at rank 1 that the policy suppresses is a
    /// *calibration* miss, not a retrieval miss, and that distinction is the
    /// whole point of the row (BUG-20260927-1910).
    #[test]
    fn per_query_separates_rank_from_abstain_decision() {
        let labelled = [
            q("suppressed but found", "gold", true),
            q("answered, absent", "gold", true),
            q("noise", "", false),
        ];
        let ranked = [
            RankedResult {
                ids: vec!["gold here".into(), "other".into()],
                abstained: true,
                confidence: 0.49,
            },
            RankedResult {
                ids: vec!["x".into(), "y".into()],
                abstained: false,
                confidence: 0.80,
            },
            RankedResult {
                ids: vec![],
                abstained: true,
                confidence: 0.10,
            },
        ];
        let rows = per_query(&labelled, &ranked);
        assert_eq!(
            rows.iter().map(|r| r.rank).collect::<Vec<_>>(),
            vec![Some(1), None, None]
        );
        assert_eq!(
            rows.iter().map(|r| r.abstained).collect::<Vec<_>>(),
            vec![true, false, true]
        );
        assert!((rows[0].confidence - 0.49).abs() < 1e-6);
        // Recall still counts the suppressed hit: retrieval is scored on the
        // ranking, the policy is scored separately.
        assert_eq!(metrics(&labelled, &ranked).recall_at_1, 1.0 / 2.0);
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
                confidence: 0.1,
            }, // TN
            ranked(&["noise"]), // FP (answered)
            RankedResult {
                ids: vec![],
                abstained: true,
                confidence: 0.1,
            }, // FN (abstained on answerable)
        ];
        let m = metrics(&labelled, &results);
        // 2 correct (TP + TN) out of 4.
        assert_eq!(m.abstain_accuracy, 0.5);
    }
}
