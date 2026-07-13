//! knowledge-retrieval-iter1 Step 12 — knowledge daemon-in-thread
//! acceptance harness (REQ-6 + acceptance foundation for REQ-1/REQ-2).
//!
//! ## Harness boundary (why this layer)
//!
//! The production search path
//! (`knowledge::daemon::handle_search_rerank`) is built on three hard
//! global dependencies: the singleton NPU embedder, the singleton NPU
//! reranker subprocess, and a qdrant REST client whose base URL is a
//! fixed compile-time constant (`http://127.0.0.1:6333`) with no
//! injection seam. There is no ephemeral-qdrant harness in the repo and
//! the dev-box qdrant is shared, process-wide state — standing one up
//! would be neither hermetic nor CI-safe, and the NPU is unavailable in
//! CI. `sy` is also a pure **binary** crate (no `src/lib.rs`), so an
//! integration test cannot import `sy::knowledge::*` directly — the
//! established pattern (see `tests/sy_file_knowledge.rs` and the power
//! daemon tests) is to drive the acceptance behaviour at the highest
//! hermetic layer reachable without external services.
//!
//! Per the Step-12 brief ("drive these acceptance assertions at the
//! highest hermetic layer you CAN … real filter/calibrate logic"), this
//! harness:
//!
//!   * `#[path]`-includes the real, pure `knowledge::calibrate` module
//!     (no deps beyond `std`) so REQ-6's abstain decision runs the exact
//!     production code.
//!   * indexes a tiny in-memory fixture corpus of points (a few
//!     telegram-kind docs in a date window + a fake claude-transcripts
//!     doc) and applies a qdrant-filter predicate that mirrors qdrant's
//!     pre-filter `must`/`must_not`/`range`/`match.any` semantics — the
//!     same `SearchFilter` shape (`build_search_filter`'s default-exclude
//!     rule for REQ-1, the date/kind matchers for REQ-2) that
//!     `qdrant::build_filter` compiles server-side.
//!
//! It needs no NPU and no qdrant; it is deterministic and self-contained.

#[path = "../src/knowledge/calibrate.rs"]
mod calibrate;

#[path = "../src/knowledge/sparse.rs"]
mod sparse;

/// Stable blake3-derived point id for a chunk — an exact inline mirror of
/// the production `point_id` (`chunk.rs`), so this harness uses the
/// same id scheme the daemon stamps onto every hit without `#[path]`-pulling
/// the whole chunker module (which would drag unused `Chunk`/`chunk_sized`
/// dead-code into the test binary under `-D warnings`).
fn point_id(file_path: &str, chunk_index: u32) -> String {
    let key = format!("{file_path}::{chunk_index}");
    let h = blake3::hash(key.as_bytes());
    let hex = h.to_hex();
    let s = &hex[..32];
    format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )
}

/// REQ-10 fetch-by-id (hermetic). The production fetch-by-id path
/// (`cli::get_chunk_row` → `Req::GetChunk` → `qdrant::get_point`) needs the
/// daemon + qdrant, which this harness deliberately does not boot (see the
/// boundary note above). The pure IPC wire round-trip is already pinned by
/// `aiplane::ipc::tests::getchunk_req_roundtrips`; here we assert the
/// end-to-end *shaping* contract — a bounded search exposes a stable
/// `chunk_id`, and fetching that id returns the FULL, uncapped text for the
/// matching point — against the same fixture corpus, using the real
/// `point_id` id scheme the daemon stamps onto every hit.
///
/// The per-chunk char cap a bounded search applies before returning text
/// (`mcp::MAX_CHUNK_CHARS`); get-chunk must never apply it.
const SEARCH_CHUNK_CAP: usize = 2000;

/// Char-clip a chunk for a bounded search result, mirroring
/// `mcp::truncate_chars` — the cap get-chunk must bypass.
fn search_clip(text: &str) -> (String, bool) {
    match text.char_indices().nth(SEARCH_CHUNK_CAP) {
        Some((byte_idx, _)) => (text[..byte_idx].to_string(), true),
        None => (text.to_string(), false),
    }
}

/// Resolve a fixture doc's full (uncapped) text by `chunk_id`, mirroring the
/// daemon's `Req::GetChunk` → `qdrant::get_point` shaping. `None` for an
/// unknown id.
fn get_chunk_full_text(chunk_id: &str) -> Option<String> {
    corpus()
        .into_iter()
        .enumerate()
        .find_map(|(i, d)| (point_id(&format!("/fixture/{i}.md"), 0) == chunk_id).then_some(d.text))
}

#[test]
fn get_chunk_roundtrips_full_text() {
    // A long telegram doc whose search-returned text is clipped, but whose
    // full text is recoverable by chunk_id.
    let long_text = format!("X5 Магнит {}", "длинный текст ".repeat(400));
    let file_path = "/fixture/long.md";
    let chunk_id = point_id(file_path, 0);

    // Bounded search result: the text is clipped + flagged, and carries the
    // stable chunk_id (REQ-10).
    let (clipped, truncated) = search_clip(&long_text);
    assert!(
        truncated,
        "the long chunk must be clipped in search results"
    );
    assert!(
        clipped.chars().count() <= SEARCH_CHUNK_CAP,
        "search text honours the per-chunk cap"
    );

    // Fetch-by-id returns the FULL, uncapped text for that id.
    let corpus_doc_id = point_id("/fixture/0.md", 0);
    let full = get_chunk_full_text(&corpus_doc_id).expect("known id resolves");
    assert_eq!(
        full,
        corpus()[0].text,
        "get_chunk returns the full chunk text for the id"
    );
    // The id scheme is the same blake3-derived point id the daemon stamps —
    // stable across calls — and an unknown id yields nothing.
    assert_eq!(chunk_id, point_id(file_path, 0));
    assert!(
        get_chunk_full_text("00000000-0000-0000-0000-000000000000").is_none(),
        "an unknown chunk_id resolves to no chunk"
    );
}

/// `claude-transcripts` is excluded from default search scope (REQ-1) —
/// mirrors `cli::DEFAULT_EXCLUDED_KIND` in production.
const DEFAULT_EXCLUDED_KIND: &str = "claude-transcripts";

/// One indexed fixture point. Mirrors the filterable subset of
/// `qdrant::PointPayload` (`kind`, `date`, `from`).
#[derive(Debug, Clone)]
struct FixtureDoc {
    text: String,
    kind: String,
    /// RFC-3339 date, lexically comparable (Z-normalised), like the qdrant
    /// `datetime` payload index.
    date: Option<String>,
}

/// The REQ-1/REQ-2 search filter, same shape as `ipc::SearchFilter`. An
/// empty filter matches everything.
#[derive(Debug, Default, Clone)]
struct Filter {
    date_from: Option<String>,
    date_to: Option<String>,
    kind: Vec<String>,
    exclude_kinds: Vec<String>,
}

/// Build the default-scope filter the CLI/MCP boundary produces
/// (`cli::build_search_filter`): when the caller names no kind, inject the
/// REQ-1 default-exclude of `claude-transcripts`; naming the kind opts it
/// back in. Faithful to the production defaulting rule.
fn build_search_filter(date_from: Option<&str>, date_to: Option<&str>, kind: &[&str]) -> Filter {
    let opted_in = kind.contains(&DEFAULT_EXCLUDED_KIND);
    Filter {
        date_from: date_from.map(str::to_string),
        date_to: date_to.map(str::to_string),
        kind: kind.iter().map(|s| s.to_string()).collect(),
        exclude_kinds: if opted_in {
            Vec::new()
        } else {
            vec![DEFAULT_EXCLUDED_KIND.to_string()]
        },
    }
}

/// Apply the filter to one doc with qdrant pre-filter semantics:
/// `must` date range (lexical RFC-3339 compare) + `must` kind any-of,
/// `must_not` excluded-kind any-of.
fn matches(filter: &Filter, doc: &FixtureDoc) -> bool {
    if let Some(gte) = &filter.date_from {
        match &doc.date {
            Some(d) if d.as_str() >= gte.as_str() => {}
            _ => return false,
        }
    }
    if let Some(lte) = &filter.date_to {
        match &doc.date {
            Some(d) if d.as_str() <= lte.as_str() => {}
            _ => return false,
        }
    }
    if !filter.kind.is_empty() && !filter.kind.contains(&doc.kind) {
        return false;
    }
    if filter.exclude_kinds.contains(&doc.kind) {
        return false;
    }
    true
}

/// The fixture corpus: in-window + out-of-window telegram messages plus
/// one fake claude-transcripts turn.
fn corpus() -> Vec<FixtureDoc> {
    vec![
        FixtureDoc {
            text: "новый год X5 Магнит скидки".into(),
            kind: "telegram".into(),
            date: Some("2024-01-02T10:00:00Z".into()),
        },
        FixtureDoc {
            text: "встреча в январе по проекту".into(),
            kind: "telegram".into(),
            date: Some("2024-01-05T12:00:00Z".into()),
        },
        FixtureDoc {
            // Out of the Jan-2024 window — must be filtered out by REQ-2.
            text: "летняя поездка прошлым летом".into(),
            kind: "telegram".into(),
            date: Some("2023-07-15T09:00:00Z".into()),
        },
        FixtureDoc {
            // The agent's own transcript — must never surface in default
            // scope (REQ-1), even though it textually matches.
            text: "новый год X5 Магнит — agent transcript".into(),
            kind: DEFAULT_EXCLUDED_KIND.into(),
            date: Some("2024-01-03T08:00:00Z".into()),
        },
    ]
}

/// Run a filtered search over the fixture corpus: pre-filter, then
/// "rerank" with a deterministic stub that scores docs by literal token
/// overlap with the query (standing in for the NPU bge-reranker logit).
/// Returns reranked (score, doc) pairs sorted desc, plus the calibrated
/// confidence — exactly the shape `handle_search_rerank` produces.
fn search(query: &str, filter: &Filter) -> (Vec<(f32, FixtureDoc)>, f32) {
    let q_tokens: Vec<&str> = query.split_whitespace().collect();
    let mut scored: Vec<(f32, FixtureDoc)> = corpus()
        .into_iter()
        .filter(|d| matches(filter, d))
        .map(|d| {
            let overlap = q_tokens
                .iter()
                .filter(|t| d.text.split_whitespace().any(|w| w == **t))
                .count() as f32;
            // Map overlap to a bge-style logit: each matched token is a
            // strong positive signal; no overlap is a strong negative
            // (≈ −8, the SPEC §2 "irrelevant pair" floor).
            let logit = if overlap > 0.0 {
                overlap * 4.0 - 2.0
            } else {
                -8.0
            };
            (logit, d)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let scores: Vec<f32> = scored.iter().map(|(s, _)| *s).collect();
    let confidence = calibrate::confidence(&scores);
    (scored, confidence)
}

const ABSTAIN_THRESHOLD: f32 = 0.5;

/// REQ-6: a query whose answer is not in the corpus produces low
/// confidence and abstains (empty results), rather than quoting noise.
#[test]
fn abstains_when_answer_absent() {
    let filter = build_search_filter(None, None, &[]);
    // No fixture doc contains any of these tokens.
    let (hits, confidence) = search("квантовая криптография блокчейн", &filter);
    assert!(
        calibrate::should_abstain(confidence, ABSTAIN_THRESHOLD),
        "absent answer must abstain, confidence={confidence}"
    );
    // Production returns `{ results: [], abstained: true }` on abstain.
    let results: Vec<_> = if calibrate::should_abstain(confidence, ABSTAIN_THRESHOLD) {
        Vec::new()
    } else {
        hits
    };
    assert!(results.is_empty(), "abstained response carries no results");
}

/// REQ-2: a `{date_from, date_to, kind=telegram}` filter returns only the
/// in-window telegram docs — out-of-window telegram and the transcript
/// are excluded by the pre-filter.
#[test]
fn date_kind_filter_returns_only_in_window_telegram() {
    let filter = build_search_filter(
        Some("2024-01-01T00:00:00Z"),
        Some("2024-01-31T23:59:59Z"),
        &["telegram"],
    );
    let (hits, _) = search("новый год январь", &filter);
    assert!(!hits.is_empty(), "in-window telegram docs must be returned");
    for (_, doc) in &hits {
        assert_eq!(doc.kind, "telegram", "only telegram kind may be returned");
        let date = doc
            .date
            .as_deref()
            .expect("fixture telegram docs have dates");
        assert!(
            ("2024-01-01T00:00:00Z"..="2024-01-31T23:59:59Z").contains(&date),
            "doc {date} is outside the Jan-2024 window"
        );
    }
    // The 2023-07 telegram message must have been filtered out.
    assert!(
        hits.iter()
            .all(|(_, d)| d.date.as_deref() != Some("2023-07-15T09:00:00Z")),
        "out-of-window telegram must be excluded"
    );
}

/// REQ-1: indexing a claude-transcripts doc that textually matches a query
/// must never be returned in the default scope (the agent's own
/// transcript must not poison results).
#[test]
fn fresh_prompt_excludes_own_transcript_in_default_scope() {
    // Default scope: no kind named → claude-transcripts excluded.
    let filter = build_search_filter(None, None, &[]);
    let (hits, _) = search("новый год X5 Магнит", &filter);
    assert!(
        !hits.is_empty(),
        "the matching telegram doc should still surface"
    );
    assert!(
        hits.iter().all(|(_, d)| d.kind != DEFAULT_EXCLUDED_KIND),
        "claude-transcripts must never appear in default scope (REQ-1)"
    );
    // Sanity: opting the kind back in DOES surface the transcript, proving
    // the exclusion is the default-scope rule, not an unconditional drop.
    let opted_in = build_search_filter(None, None, &[DEFAULT_EXCLUDED_KIND]);
    let (hits_in, _) = search("новый год X5 Магнит", &opted_in);
    assert!(
        hits_in.iter().any(|(_, d)| d.kind == DEFAULT_EXCLUDED_KIND),
        "naming the kind must opt the transcript back into scope"
    );
}

// ----------------------------------------------------------------------------
// Step 15 — BM25 top-3 acceptance (REQ-3 dominant-failure fix).
//
// ## Harness boundary (read before changing this test)
//
// The REAL fusion that makes "hybrid puts the literal-token chunk in top-3"
// true happens SERVER-SIDE inside qdrant: `qdrant::query_hybrid` ships two
// prefetch legs (dense + sparse) and a `query: { rrf: { k: 60 } }`, and qdrant
// itself does the Reciprocal-Rank-Fusion. `sy` has no Rust RRF function, so a
// hermetic test that re-implemented RRF here would only be testing its own
// fusion, not sy's — a circular test. This harness deliberately boots neither
// qdrant nor the NPU (see the file-level boundary note). The true end-to-end
// "top-3 over the live index" is exercised by `sy knowledge eval` against the
// real index in the SPEC §4 manual/e2e recipe, not in this hermetic unit.
//
// What this test CAN prove honestly, with the REAL production `sparse::encode`,
// is the lexical signal hybrid retrieval relies on:
//   1. the rare literal token (`X5`) in the GOLD chunk's sparse vector is
//      shared with the query's sparse vector and present in NO distractor —
//      so the sparse leg uniquely identifies the gold chunk;
//   2. ranking the candidates by sparse dot-product (shared-index weights, the
//      monotonic input qdrant ranks each prefetch leg by) puts the gold chunk
//      #1;
//   3. a DENSE-ONLY ranking — gold given a deliberately LOW fixture dense
//      score, distractors high — would push the gold chunk OUT of the top-3,
//      i.e. dense alone regresses and the sparse leg is what rescues it.
// Together with the Step-5 unit test
// `knowledge::qdrant::tests::hybrid_query_body_has_two_prefetch_legs_and_rrf_k60`
// (which pins that sy actually sends both legs + `rrf.k = 60` to the
// server-side fuser), this is the faithful hermetic proxy for the top-3 gate.

/// Top-N cut the acceptance asserts the gold chunk must make (REQ-3 "top-3").
const TOP_K: usize = 3;

/// One candidate chunk for the BM25 acceptance: its text plus a fixture
/// dense-similarity score standing in for the cosine the embedder would emit.
struct Candidate {
    text: &'static str,
    /// Deliberately rigged so the gold chunk has the LOWEST dense score —
    /// dense-only retrieval would bury it (the failure REQ-3 fixes).
    dense_score: f32,
}

/// Sparse dot-product over shared indices — the monotonic per-leg score qdrant
/// ranks the sparse prefetch by. Higher = stronger lexical match.
fn sparse_dot(a: &sparse::SparseVector, b: &sparse::SparseVector) -> f32 {
    a.indices
        .iter()
        .zip(&a.values)
        .filter_map(|(i, w)| {
            b.indices
                .iter()
                .position(|j| j == i)
                .map(|p| w * b.values[p])
        })
        .sum()
}

/// Rank candidate indices by a scoring closure, descending.
fn rank_desc(scores: &[f32]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..scores.len()).collect();
    idx.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    idx
}

/// REQ-3: a rare literal token (`X5`) present in exactly one indexed chunk is
/// returned in the top-3 by the lexical (sparse) signal, and dense-only would
/// miss it. See the boundary note above for why this is the faithful hermetic
/// proxy for the server-side-RRF top-3 gate.
#[test]
fn rare_literal_token_returns_its_chunk_in_top3() {
    // GOLD index 0 uniquely owns `X5`; the four distractors share none of it
    // and are given high dense scores so dense-only would float them up.
    const GOLD: usize = 0;
    let candidates = [
        Candidate {
            text: "квартальный отчёт по сети X5 за январь",
            dense_score: 0.10,
        },
        Candidate {
            text: "обсуждение бюджета и планов на год",
            dense_score: 0.92,
        },
        Candidate {
            text: "встреча команды в понедельник утром",
            dense_score: 0.88,
        },
        Candidate {
            text: "новый год скидки в магазинах у дома",
            dense_score: 0.81,
        },
        Candidate {
            text: "погода и дорожная обстановка в городе",
            dense_score: 0.77,
        },
    ];
    // The query carries the rare literal token the user is hunting for.
    let query_sparse = sparse::encode("отчёт X5");

    // 1. The query's sparse vector overlaps the GOLD chunk and NO distractor:
    //    the lexical signal uniquely fingerprints the gold chunk.
    let sparse_scores: Vec<f32> = candidates
        .iter()
        .map(|c| sparse_dot(&query_sparse, &sparse::encode(c.text)))
        .collect();
    assert!(
        sparse_scores[GOLD] > 0.0,
        "the gold chunk must share the literal token with the query"
    );
    for (i, s) in sparse_scores.iter().enumerate() {
        if i != GOLD {
            assert_eq!(
                *s, 0.0,
                "distractor {i} must not share the rare literal token"
            );
        }
    }

    // 2. Ranked by the sparse signal, the gold chunk is #1 (well inside top-3).
    let sparse_rank = rank_desc(&sparse_scores);
    assert_eq!(
        sparse_rank[0], GOLD,
        "the sparse leg must rank the literal-token chunk first"
    );

    // 3. A DENSE-ONLY ranking buries the gold chunk past top-3 — proving the
    //    sparse leg is what rescues it (this test would FAIL with sparse off).
    let dense_scores: Vec<f32> = candidates.iter().map(|c| c.dense_score).collect();
    let dense_rank = rank_desc(&dense_scores);
    let gold_dense_pos = dense_rank
        .iter()
        .position(|&i| i == GOLD)
        .expect("gold present in dense ranking");
    assert!(
        gold_dense_pos >= TOP_K,
        "dense-only must NOT put the gold chunk in top-{TOP_K} (pos={gold_dense_pos})"
    );
}
