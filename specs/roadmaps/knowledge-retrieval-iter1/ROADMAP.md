# ROADMAP: knowledge-retrieval-iter1

Source: `specs/research/knowledge-retrieval-iter1/SPEC.md`
(grounds `specs/knowledge-feedback-iter1/FEEDBACK.md`, REQ-1 … REQ-10)

## Overview

Rebuild `sy-knowledge` retrieval from dense-only cosine into **hybrid
(sparse + dense, RRF-fused) search with pre-search payload filters**,
add **per-source pipelines** (per-message Telegram records, per-turn
transcripts), **CPU/iGPU voice transcription**, **confidence + abstain**,
a **deterministic eval harness**, and **fetch-by-id response hygiene**.
The end state: a specific-fact query (text or transcribed voice) returns
the right message in the right date window in the top few hits — or a
confident "not found" — with the agent's own transcripts excluded by
default. No embedding/reranker swap; the qdrant collection moves to named
`dense`+`sparse` vectors with indexed payload, requiring a one-time
resumable re-index migration.

Highest-leverage steps against the failing session are source-kind
segregation (Step 1 + Step 11) and hybrid retrieval (Steps 3–5).

**Re-slice note (post Step-2):** `sy` is a pure **binary** crate (no
`src/lib.rs`). A `pub`/`pub(crate)` item reachable only from
`#[cfg(test)]` is `dead_code` under `cargo clippy -D warnings`, so a
step must **never** land a helper module without a real consumer in
`main`'s call graph. The original roadmap's standalone "pure module"
steps (sparse encoder, eval metrics, calibration, whisper wrapper) were
therefore merged with their first consumer. The sparse signal is an
**in-house** term-frequency encoder (no `bm25` crate — it pulled
unmaintained `fxhash` / RUSTSEC-2025-0057 and failed the audit gate;
in-house also matches AGENTS.md vendor-neutrality and the SPEC's
"compute sparse without the crate" open question). Net: 22 → 18 steps.

Each step assumes the AGENTS.md working loop: **failing test first →
minimal code → `make lint` (clippy `-D warnings` + `cargo fmt --check`)
→ `make test`** (`cargo test --workspace --all-targets`), zero
`#[allow(dead_code)]` outside `#[cfg(test)]`, and **every new item
reachable from a non-test consumer by the end of its step**.

---

## Step 1 — Add `SourceKind` to the source model
**Goal:** Sources carry a stable `name` and a `kind` enum, with
auto-classification of well-known paths (REQ-1 data model).
**Files:** `src/knowledge/sources.rs` (modified: `Source` @71,
`KnowledgeSection` @22, `add` @143)
**Tests:**
- `src/knowledge/sources.rs::tests::kind_auto_classifies_claude_projects` — `~/.claude/projects/**` → `SourceKind::ClaudeTranscripts`
- `::tests::kind_defaults_to_generic_for_unknown_paths`
- `::tests::source_toml_roundtrips_with_name_and_kind` — serde with `#[serde(default)]` so old `sy.toml` still loads
**Definition of Done:**
- [x] `SourceKind` enum (`Telegram`, `ClaudeTranscripts`, `Email`, `Slack`, `Notes`, `Code`, `Generic`), kebab-case serde
- [x] `Source { name, kind, .. }`; `add()` classifies on insert; missing fields default (back-compat)
- [x] tests pass; `make lint` green; no `#[allow(dead_code)]`
**Risks / unknowns:** classification heuristics for non-telegram kinds are best-effort; only `claude-transcripts` and `telegram` need to be precise this iteration.

---

## Step 2 — Extend qdrant payload + create payload indexes
**Goal:** Points carry filterable metadata and the collection indexes it
before ingest (REQ-2 storage half).
**Files:** `src/knowledge/qdrant.rs` (modified: `PointPayload` @139,
`ensure_collection` @73, `ensure_payload_index` @107)
**Tests:**
- `src/knowledge/qdrant.rs::tests::payload_serializes_optional_metadata` — `kind, source_name, date (RFC3339), from, has_media, message_id, reply_to_id` serialize, all optional
- `::tests::ensure_collection_requests_datetime_keyword_bool_indexes` — index-create calls built for `date`(datetime), `kind`/`source_name`/`from`(keyword), `has_media`(bool)
**Definition of Done:**
- [x] `PointPayload` gains the new optional fields; existing upsert sites compile (default `None`)
- [x] payload indexes created inside `ensure_collection` (before any ingest)
- [x] tests pass; lint green
**Risks / unknowns:** index creation must precede ingest for filterable-HNSW edges; verified against qdrant docs in SPEC §2.

---

## Step 3 — Collection schema v2: named `dense`+`sparse` + migration trigger
**Goal:** Move to a named-vector collection and auto-migrate old indexes
on daemon start (REQ-3 schema + migration). *(was Step 4)*
**Files:** `src/knowledge/qdrant.rs` (modified: `ensure_collection` @73,
`upsert` @153, `search` @240 — use named vector `"dense"`),
`src/knowledge/mod.rs` (modified: `COLLECTION` @295, add `SCHEMA_VERSION`),
`src/knowledge/daemon.rs` (modified: startup → detect old schema → set
`want_full_resync`, near `FullResync` handling @581)
**Tests:**
- `src/knowledge/qdrant.rs::tests::ensure_collection_v2_declares_named_dense_and_sparse` — body has `vectors.dense` (Cosine, `VECTOR_DIM`) + `sparse_vectors.sparse`
- `src/knowledge/daemon.rs::tests::stale_schema_triggers_full_resync`
**Definition of Done:**
- [x] collection created with named `dense` (Cosine) + `sparse` vectors; `SCHEMA_VERSION` stored/detected
- [x] dense-only search still works through the named `dense` vector (no behavior change yet); reachable via existing `upsert`/`search` paths (no dead code)
- [x] daemon detects a pre-v2 collection and queues a resumable `FullResync`
- [x] tests pass; lint green
**Risks / unknowns:** sparse `modifier` — since Step 4 emits **term-frequency** weights, enable `"modifier": "idf"` on the `sparse` vector so qdrant applies IDF server-side. Pin qdrant ≥ 1.16 (Step 5 needs configurable RRF `k`). The `sparse` vector is declared here but written in Step 4 — qdrant tolerates points missing a named vector, so no orphaned Rust.

---

## Step 4 — In-house sparse encoder + write sparse vector at index time
**Goal:** Deterministic term-frequency `{indices, values}` sparse
vectors, computed in pure Rust and written into every upserted point
(REQ-3 generation + ingest). *(merges old Steps 3 + 5; drops `bm25`)*
**Files:** `src/knowledge/sparse.rs` (new), `src/knowledge/mod.rs`
(modified: add `mod sparse`), `src/knowledge/qdrant.rs` (modified:
`upsert`/`Point` carry named `dense`+`sparse` vectors), `src/knowledge/cli.rs`
(modified: batch flush near @1413 computes `sparse::encode` per chunk)
**Tests:**
- `src/knowledge/sparse.rs::tests::encode_is_stable_for_fixed_text` — same text → same indices/values
- `::tests::rare_literal_token_appears_in_sparse_vector` — `X5` / `Магнит` → non-empty indices
- `::tests::tokenizer_handles_cyrillic`
- `src/knowledge/cli.rs::tests::upsert_point_carries_dense_and_sparse`
**Definition of Done:**
- [x] `encode(text) -> SparseVector { indices: Vec<u32>, values: Vec<f32> }` — **in-house** (no external crate): unicode-aware tokenizer + a fixed token→u32 hash (stable across calls/processes) + saturating term-frequency weights; no corpus/IDF state in-process (qdrant applies IDF via the Step 3 `modifier`)
- [x] index pass computes a sparse vector per chunk and upserts named `dense`+`sparse`; **`encode` is consumed by the live index path** (no dead code)
- [x] re-index idempotent on point id (unchanged); tests pass; lint green
**Risks / unknowns:** stable u32 ids are the load-bearing property — unit-test that the same token yields the same index across two `encode` calls. Sparse encode is CPU per chunk; keep it inside the existing throttle path.

---

## Step 5 — Hybrid Universal Query (dense + sparse → RRF)
**Goal:** Search fuses dense + sparse via the qdrant Query API before
rerank (REQ-3 retrieve). *(was Step 6)*
**Files:** `src/knowledge/qdrant.rs` (new fn `query_hybrid` + keep
`search` @240 for fallback), `src/knowledge/daemon.rs` (modified:
`handle_search_rerank` @1411 → call hybrid; query-side uses `sparse::encode`)
**Tests:**
- `src/knowledge/qdrant.rs::tests::hybrid_query_body_has_two_prefetch_legs_and_rrf_k60` — `prefetch[using=dense]` + `prefetch[using=sparse]`, `query.rrf.k = 60`
- (integration BM25 top-3 gate lands in Step 15)
**Definition of Done:**
- [x] `query_hybrid(dense, sparse, filter, limit)` issues one Universal Query with explicit `rrf.k = 60`
- [x] daemon search routes through hybrid (query-side `sparse::encode` consumed); rerank stage unchanged downstream
- [x] tests pass; lint green
**Risks / unknowns:** qdrant default `k=2`; **must** set `k` explicitly and run qdrant ≥ 1.16 (assert at startup — Step 6 / doctor).

---

## Step 6 — IPC wire format: filters, abstain threshold, confidence
**Goal:** Carry the new search inputs/outputs over IPC (plumbing-first).
*(was Step 7)*
**Files:** `src/aiplane/ipc.rs` (modified: `Req::Search` @97,
`Req::SearchRerank` @114, `Resp::Search` @142, `req_to_v1` @1113,
`try_method_to_req` @1157)
**Tests:**
- `src/aiplane/ipc.rs::tests::search_req_roundtrips_with_filter_and_threshold`
- `::tests::resp_search_carries_confidence`
- `::tests::method_to_req_parses_filter_params`
**Definition of Done:**
- [x] `SearchFilter { date_from, date_to, from, kind, include_sources, exclude_sources }` added; `Req::Search`/`SearchRerank` gain `filter: Option<SearchFilter>` + `abstain_threshold: Option<f32>`
- [x] `Resp::Search` gains `confidence: f32` (+ `abstained`), defaulted so existing paths compile
- [x] v1 method mapping round-trips; tests pass; lint green
**Risks / unknowns:** keep additive/defaulted so daemon and CLI built at different steps interoperate. New fields are serde-derived + constructed at the existing `cli.rs` call sites (@1050/@1058), so they stay live (no dead code).

---

## Step 7 — Compile `SearchFilter` → qdrant `Filter` (pre-filter)
**Goal:** Filters constrain both prefetch legs before scoring
(REQ-2 retrieval half). *(was Step 8)*
**Files:** `src/knowledge/qdrant.rs` (new `build_filter` + apply in
`query_hybrid` from Step 5)
**Tests:**
- `src/knowledge/qdrant.rs::tests::filter_builds_datetime_range_and_keyword_match` — `date_from/to` → datetime range; `from`/`kind` → match/any; `include/exclude_sources` → must/must_not on `source_name`
- `::tests::empty_filter_is_none`
**Definition of Done:**
- [x] `build_filter(&SearchFilter, prefix) -> Option<qdrant Filter>`; **applied via `query_hybrid`'s `filter` param** in the live `handle_search_rerank` path (the `SearchRerank` filter is now compiled, folding the Step 5 `prefix` file_path text-match into the same `must`)
- [x] tests pass; lint green
**Risks / unknowns:** datetime stored RFC 3339 (matches Step 2's `date` index type).

---

## Step 8 — Pipeline trait + generic pipeline (refactor)
**Goal:** Route indexing through per-kind pipelines; lift the current
chunker into the generic pipeline unchanged (REQ-4 foundation).
*(was Step 9)*
**Files:** `src/knowledge/pipeline/mod.rs` (new: `trait Pipeline`,
`Record { text, payload, chunk_id }`, `select(kind)`),
`src/knowledge/pipeline/generic.rs` (new: wraps `chunk::chunk` @16 at
500–800-token target), `src/knowledge/cli.rs` (modified: `run_index`
@1182 routes through pipeline), `src/knowledge/mod.rs` (add `mod pipeline`)
**Tests:**
- `src/knowledge/pipeline/generic.rs::tests::generic_emits_records_with_stable_chunk_ids` — chunk_id == blake3(`file::index`) per `chunk.rs` @46
- `src/knowledge/pipeline/mod.rs::tests::select_returns_generic_for_unknown_kind`
**Definition of Done:**
- [x] indexing produces identical chunk_ids/text for generic sources (behavior-preserving lift; `run_index` is the live consumer)
- [x] target chunk size reduced to 500–800 tokens (documented)
- [x] tests pass; lint green
**Risks / unknowns:** smaller chunk size changes existing point ids → part of the Step 3 re-index migration.

---

## Step 9 — Telegram pipeline (per-message, streaming JSON + HTML fallback)
**Goal:** One record per Telegram message with structured payload,
tolerant of multi-GB / truncated exports (REQ-4 telegram). *(was Step 10)*
**Files:** `src/knowledge/pipeline/telegram.rs` (new),
`src/knowledge/pipeline/mod.rs` (modified: route `kind=telegram`),
`Cargo.toml` (streaming JSON parser if needed, e.g. `struson`)
**Tests:**
- `::tests::json_export_emits_one_record_per_message` — fixture → N records, `date`/`from`/`message_id` populated
- `::tests::reply_links_and_has_media_detected`
- `::tests::truncated_result_json_yields_partial_records_without_panicking`
- `::tests::falls_back_to_html_when_json_invalid`
**Definition of Done:**
- [x] `result.json` streamed primary; HTML fallback when JSON invalid/truncated
- [x] per-message payload `{date, from, file, message_id, reply_to_id, has_media}`; pass never aborts on one bad file; routed by `select` (live consumer)
- [x] tests pass; lint green
**Risks / unknowns:** this user's `result.json` is invalid past ~25 MB — the streaming/tolerant path is the core requirement; keep the parser ≤ ~300 lines or split JSON/HTML into sibling modules.

---

## Step 10 — Claude-transcripts pipeline (per-turn, kind-tagged)
**Goal:** One record per transcript turn, tagged `claude-transcripts`
(REQ-4 transcripts + REQ-1 kind population). *(was Step 11)*
**Files:** `src/knowledge/pipeline/transcripts.rs` (new),
`src/knowledge/pipeline/mod.rs` (modified: route `kind=claude-transcripts`)
**Tests:**
- `::tests::jsonl_emits_one_record_per_turn` — payload `{role, model, project_id, ts}`, `kind=claude-transcripts`
- `::tests::malformed_jsonl_line_is_skipped_not_fatal`
**Definition of Done:**
- [x] transcript points carry `kind=claude-transcripts` + `date` from `ts`; routed by `select` (live consumer)
- [x] tests pass; lint green
**Risks / unknowns:** none significant; format is line-delimited JSON.

**Mapping (Step 10):** `select(ClaudeTranscripts) -> TranscriptsPipeline`
(one `Record` per parseable `.jsonl` line; blank/malformed lines skipped).
Per-turn `RecordPayload`: `ts`/`timestamp` → `date`, `role` → `from`, plus
new `model` / `project_id` fields (`project_id` derived from the
`.claude/projects/<project>/` key segment). `kind=claude-transcripts` is
stamped onto `PointPayload` in `cli.rs::build_point` from the job's
`SourceKind` (threaded via `PendingFile.kind`) — previously `kind` was
never populated; `SourceKind::as_kebab()` is the single kebab serializer
(also now reused by `sources.rs::write`). `PointPayload` gained matching
`model`/`project_id` optional fields; generic/telegram default them to
`None`.

---

## Step 11 — Search filter args + default-exclude transcripts (consumer)
**Goal:** Expose filters on CLI/MCP and exclude `claude-transcripts` from
default scope (REQ-1 + REQ-2 consumer; closes REQ-1 acceptance).
*(was Step 12)*
**Files:** `src/knowledge/mcp.rs` (modified: `knowledge_search` schema
@110, `tool_search` @158), `src/knowledge/mod.rs` (modified:
`KnowledgeCmd::Search` @89, `dispatch` @222), `src/knowledge/cli.rs`
(modified: `search_hits_opts` @1030 build `SearchFilter`)
**Tests:**
- `src/knowledge/mcp.rs::tests::default_scope_excludes_claude_transcripts`
- `::tests::explicit_kind_or_include_source_overrides_default_exclude`
- `src/knowledge/cli.rs::tests::search_args_compile_to_searchfilter` — `--date-from/-to`, `--from`, `--kind`, `--include-source`, `--exclude-source`
**Definition of Done:**
- [x] CLI flags + `SY_KB_*` env (precedence flags > env > default); MCP args mirror them; `--json` schema per SPEC §4
- [x] default `exclude_sources` includes `claude-transcripts`; a fresh prompt never returns its own transcript in default scope (REQ-1 acceptance)
- [x] tests pass; lint green; MCP tool description updated
**Risks / unknowns:** source-name validation against the registry at the boundary (security NFR).

**Mapping (Step 11):** REQ-1's default-exclude is represented as a new
additive `SearchFilter.exclude_kinds` field (defaulted, serde
skip-if-empty — same back-compat pattern as the Step 6 fields), compiled
by `qdrant::build_filter` into a `must_not` MatchAny on the `kind`
payload (transcripts are identified by `kind`, not `source_name`). The
consumer boundary injects `claude-transcripts` into `exclude_kinds`
unless the caller opts it back in — either by naming the kind in
`--kind`/`kind`, or via `--include-source`/`include_sources` that
resolves (against the registry) to a `claude-transcripts` source.
`cli::build_search_filter` is the single shared compiler used by both the
CLI (`KnowledgeCmd::Search` → `cli::search` via `SearchArgs`) and the MCP
(`tool_search` via `search_filter_from_args`); `SourceKind` gained a
`clap::ValueEnum` derive so `--kind` is validated at the boundary.
`search_hits_filtered` threads the compiled filter onto
`Req::Search`/`Req::SearchRerank` (the Step 6 `filter` field; Step 7's
`build_filter` already applies it server-side). The `--json` result shape
is unchanged this step (`confidence`/`abstained` arrive in Step 12).

---

## Step 12 — Calibration + wire confidence/abstain into search (+ daemon-in-thread harness)
**Goal:** Compute confidence (reranker sigmoid + top1−top2 margin),
abstain below threshold, and surface it through search; introduce the
knowledge daemon-in-thread integration harness (REQ-6 + acceptance
foundation for REQ-1/2/3). *(merges old Steps 15 + 16)*
**Files:** `src/knowledge/calibrate.rs` (new), `src/knowledge/mod.rs`
(add `mod calibrate`), `src/knowledge/daemon.rs` (modified:
`handle_search_rerank` @1411 → compute confidence, apply
`abstain_threshold`), `src/knowledge/mcp.rs` (modified: `tool_search`
@158 output `confidence`/`abstained`/`reason`),
`tests/sy_file_knowledge_daemon.rs` (new integration harness, mirroring
the power/daemon-in-thread pattern)
**Tests:**
- `src/knowledge/calibrate.rs::tests::sigmoid_maps_logit_zero_to_half`
- `::tests::confidence_rises_with_top1_margin`
- `::tests::abstains_below_threshold`
- `tests/sy_file_knowledge_daemon.rs::abstains_when_answer_absent` (REQ-6)
- `::date_kind_filter_returns_only_in_window_telegram` (REQ-2)
- `::fresh_prompt_excludes_own_transcript_in_default_scope` (REQ-1)
**Definition of Done:**
- [x] `confidence(top_scores)` + `should_abstain(confidence, threshold)`; logit 0 = sigmoid 0.5 boundary; **consumed by `handle_search_rerank`** (no dead code)
- [x] below-threshold → `{ results: [], abstained: true, reason: "no high-confidence match", confidence }`
- [x] integration harness drives the acceptance assertions through the **real** `calibrate` module + the production `SearchFilter` default-exclude / date-kind pre-filter semantics over a fixture corpus; stub embed/rerank, **needs no NPU and no qdrant** (hermetic). *(Deviation: the qdrant REST client has a hardcoded base-URL constant with no injection seam and `sy` is a binary-only crate, so per the Step-12 brief the harness drives the highest hermetic layer — real filter/calibrate logic + fixture hits — instead of booting an ephemeral qdrant. Boundary documented at the top of `tests/sy_file_knowledge_daemon.rs`.)*
- [x] tests pass; lint green
**Risks / unknowns:** large step (calibration + wiring + new harness). Calibration constants are named consts, tuned later against eval negatives (Step 13). If the harness alone pushes the diff past ~300 LOC, land the harness scaffolding first as its own micro-commit within the step, then the calibration wiring.

---

## Step 13 — Eval metrics + `sy knowledge eval` + golden set + `make eval`
**Goal:** Compute recall@1/5, MRR, abstain accuracy over a labelled set
and run it against the live index, gating CI (REQ-9).
*(merges old Steps 13 + 14)*
**Files:** `src/knowledge/eval.rs` (new: metrics + golden-set runner),
`src/knowledge/mod.rs` (modified: add `mod eval` + `KnowledgeCmd::Eval`
near @33/@212), `src/knowledge/cli.rs` (new `eval_cmd`),
`specs/knowledge-feedback-iter1/eval/queries.jsonl` (new, 20–40 rows:
≥5 named-entity, ≥5 date-range, ≥5 abstain, ≥5 cross-source),
`Makefile` (add `eval:` target near `test:` @20)
**Tests:**
- `src/knowledge/eval.rs::tests::recall_and_mrr_match_known_rankings`
- `::tests::abstain_accuracy_counts_true_negatives` — SQuAD-2.0-style
- `src/knowledge/cli.rs::tests::eval_cmd_emits_json_metrics`
- `::tests::eval_returns_nonzero_on_regression_past_tolerance`
**Definition of Done:**
- [x] `metrics(labelled, ranked) -> { recall_at_1, recall_at_5, mrr, abstain_accuracy, n }`; **consumed by `sy knowledge eval`** (live CLI command — no dead code)
- [x] `sy knowledge eval [--json]` → metrics; exit non-zero when below tolerance (exit codes per CLAUDE.md); `make eval` runs it; checked-in `queries.jsonl` with the required category counts
- [x] tests pass; lint green
**Risks / unknowns:** for single-gold queries recall@k == hit-rate@k (document). CI needs a live index fixture — reuse the Step 12 daemon-in-thread harness with a tiny corpus, not the real export.

**Mapping (Step 13):** Metrics are **pure/I-O-free** in `src/knowledge/eval.rs`
(`metrics(&[LabelledQuery], &[RankedResult]) -> Metrics`; `recall@k` is
hit-rate@k over answerable rows, `mrr` reciprocal-rank within `RECALL_K=5`,
`abstain_accuracy` SQuAD-2.0 TP+TN/n). The **hermetic seam** is
`cli::run_eval(queries, runner, json, &Tolerance)` — the runner is an
injected `Fn(&LabelledQuery) -> Result<RankedResult>`, so the two CLI
unit tests drive metrics + JSON emission + the drift exit (`EVAL_DRIFT=3`,
CLAUDE.md "drift") with fixture rankings and **no daemon/qdrant**. The
live `cli::eval_cmd` loads `queries.jsonl` (located via `SY_ROOT` env →
`CARGO_MANIFEST_DIR` fallback, matching the policy resolver) and uses
`run_query_live`, backed by `search_outcome_filtered`, as the runner.
`make eval` runs `cargo run -- knowledge eval --json`. Golden set: 24 rows
(6 each named-entity / date-range / abstain / cross-source), drawn from
the FEEDBACK X5/Магнит/New-Year theme.

---

## Step 14 — `knowledge_get_chunk` fetch-by-id
**Goal:** Bounded results expose stable `chunk_id`s; full text fetched on
demand (REQ-10). *(was Step 17)*
**Files:** `src/aiplane/ipc.rs` (modified: add `Req::GetChunk { chunk_id }`
near @86, `Resp` variant), `src/knowledge/qdrant.rs` (new `get_point`),
`src/knowledge/mcp.rs` (modified: add `knowledge_get_chunk` tool @107/@147;
results carry `chunk_id` + `total`), `src/knowledge/mod.rs` (modified:
add `KnowledgeCmd::GetChunk`)
**Tests:**
- `src/aiplane/ipc.rs::tests::getchunk_req_roundtrips`
- `tests/sy_file_knowledge_daemon.rs::get_chunk_roundtrips_full_text` (REQ-10)
- `src/knowledge/mcp.rs::tests::search_results_include_chunk_id_and_total`
**Definition of Done:**
- [x] IPC op + daemon handler + `knowledge_get_chunk` MCP tool + `sy knowledge get-chunk <id>` (all reachable)
- [x] every search result carries `chunk_id`; response carries `truncated` + `total`
- [x] tests pass; lint green
**Risks / unknowns:** `chunk_id` == qdrant point id (already blake3-derived, `chunk.rs` @46) — no new id scheme needed.

---

## Step 15 — BM25 top-3 acceptance (integration)
**Goal:** Lock REQ-3's dominant-failure fix with an end-to-end assertion.
*(was Step 18; test-only deliverable)*
**Files:** `tests/sy_file_knowledge_daemon.rs` (modified: add the
rare-literal-token case)
**Tests:**
- `::rare_literal_token_returns_its_chunk_in_top3` — a token present in exactly one indexed chunk is in top-3 (hybrid on)
**Definition of Done:**
- [x] test indexes a fixture where one chunk uniquely contains an `X5`-like token and asserts top-3
- [x] passes with hybrid retrieval; would fail under dense-only (guards against regression)
- [x] lint green
**Risks / unknowns:** keep the fixture deterministic with the stub embedder so dense can't accidentally surface it. *(Deviation: server-side RRF cannot be proven hermetically — `sy` has no Rust RRF fn and the harness boots no qdrant/NPU. The test is the faithful hermetic proxy: the REAL `sparse::encode` uniquely fingerprints the gold chunk (`X5` in no distractor), sparse-dot ranking puts it #1, and a rigged dense-only ranking buries it past top-3 — proving the sparse leg is the rescue. The both-legs-+-`rrf.k=60` config sy sends to the server-side fuser stays pinned by the Step-5 unit `qdrant::tests::hybrid_query_body_has_two_prefetch_legs_and_rrf_k60`; the true top-3-over-live-index gate is `sy knowledge eval`, SPEC §4.)*

---

## Step 16 — Voice/video transcription end-to-end (wrapper + index integration)
**Goal:** Voice notes become searchable chunks via a CPU/iGPU Whisper
wrapper wired into the Telegram index pass (REQ-5).
*(merges old Steps 19 + 20 — a feature-gated wrapper with no consumer
would be dead code / untested under `cargo test --workspace`)*
**Files:** `src/knowledge/transcribe.rs` (new: always-compiled `trait
Transcriber` + content-addressed sidecar cache + a disabled/no-op
backend; `whisper-rs` FFI backend behind `#[cfg(feature = "transcribe")]`),
`src/knowledge/mod.rs` (add `mod transcribe`), `Cargo.toml` (add
`whisper-rs` from Codeberg under a `transcribe` feature),
`src/knowledge/pipeline/telegram.rs` (modified: detect `voice_messages/`
/ `round_video_messages/`), `src/knowledge/cli.rs` (modified: `run_index`
@1182 invokes the transcriber for un-cached media), `Makefile`/build
notes (productize whisper.cpp build)
**Tests:**
- `src/knowledge/transcribe.rs::tests::sidecar_path_is_content_addressed`
- `::tests::cached_transcript_short_circuits` (fake transcriber)
- `src/knowledge/pipeline/telegram.rs::tests::voice_media_emits_transcript_chunk` — `kind=telegram-voice`, payload points at the media file (fake transcriber)
- `::tests::already_transcribed_media_is_skipped`
**Definition of Done:**
- [x] `trait Transcriber` + cache + a no-op backend are **always compiled** (so `cargo test --workspace` exercises them with the fake — the gate doesn't pass `--all-features`); only the `whisper-rs` impl is feature-gated; the index pass is the live consumer (no dead code, no untested module)
- [x] media detected, transcribed (cached), emitted as `kind=telegram-voice` chunks pointing at the source media; incremental + cancellable under the existing throttle
- [x] model resolution (large-v3 Russian fine-tune, medium fallback) lives behind the feature; build productized (no manual host step)
- [x] tests pass; lint green
**Risks / unknowns:** `whisper-rs` vendors a C library (Codeberg source; GitHub archived). Model fetch + C build must land in the reproducible build. Large step — if it exceeds budget, land the always-compiled trait+cache+no-op+index-consumer first (fully testable), then the feature-gated `whisper-rs` backend as a second micro-commit within the step.

**Mapping (Step 16):** `src/knowledge/transcribe.rs` carries the
always-compiled `trait Transcriber`, the content-addressed `sidecar_path`
(`<media>.<blake3-hex>.txt`) + `transcribe_cached` short-circuit cache, and a
`DisabledTranscriber` fallback (errors with a clear "transcription disabled"
message — a real backend, not a stub). The `whisper-rs` FFI `WhisperTranscriber`
(CPU/iGPU whisper.cpp, ffmpeg-decoded PCM, large-v3-russian → medium model
resolution under `~/.cache/sy/aiplane/whisper/`) is `#[cfg(feature =
"transcribe")]` only; `default_transcriber()` returns it when a model is present
else falls back to `DisabledTranscriber`. The Telegram pipeline gained a
`voice_media` field (parsed from `voice_message`/`video_message` JSON keys and
`voice_messages/`/`round_video_messages/` HTML hrefs) and
`TelegramPipeline::voice_records`, which resolves media relative to the export
root, transcribes-cached, and emits `kind=telegram-voice` `Record`s whose payload
`file_path` points at the source media. `RecordPayload` gained `kind`/`file_path`
overrides (defaulted), threaded through `cli::build_point`. `run_index` is the
live consumer — for `SourceKind::Telegram` it calls `voice_records` inside the
same scan loop that honours the cancellation check, so transcription is
incremental (sidecar cache) and cancellable under the existing throttle.
`whisper-rs = { optional = true }` + `transcribe = ["dep:whisper-rs"]` keep the C
build off the default gate. *(Deviation: the C build genuinely compiles
(`cargo build --features transcribe` is green, whisper.cpp built via cmake), but
the default `make lint`/`make test` do NOT build it — intended, per the re-slice.
Audit: whisper-rs's transitive deps (`bindgen`/`cexpr`/`clang-sys`/`cmake`/
`fs_extra`/`whisper-rs-sys`) add no new RUSTSEC advisories; the pre-existing
`yaml-rust`/`paste` advisories come from `syntect`/`burn`, unrelated and behind
the off-by-default feature regardless.)*

---

## Step 17 — Synonym expansion (sparse-side only)
**Goal:** Alias expansion boosts lexical recall without hurting dense
(REQ-7). *(was Step 21)*
**Files:** `src/knowledge/query.rs` (new: `expand_synonyms`),
`configs/sy-knowledge/synonyms.yaml` (new, shipped default),
`src/knowledge/mod.rs` (add `mod query`), `src/knowledge/daemon.rs`
or `cli.rs` (modified: search path calls `expand_synonyms` before
building the sparse query); apply-wiring so `sy apply` installs the
default to `~/.config/sy-knowledge/synonyms.yaml`
**Tests:**
- `src/knowledge/query.rs::tests::x5_expands_to_aliases_on_sparse_side`
- `::tests::expansion_does_not_touch_dense_query`
- `::tests::missing_or_empty_synonyms_file_is_noop`
**Definition of Done:**
- [x] aliases OR-ed into the sparse query only; **`expand_synonyms` called from the live search path** (no dead code); default `synonyms.yaml` shipped from `configs/` (no snowflake)
- [x] tests pass; lint green
**Risks / unknowns:** apply-wiring mirrors existing `configs/` dotfile management.

**Mapping (Step 17):** `src/knowledge/query.rs` carries the **pure**
`expand_synonyms(query, &[SynGroup]) -> String` (case-insensitive whole-token
match; on a hit it OR-s the matched group's canonical + every alias into the
returned string, preserving the original query verbatim) plus a thin I/O
loader `load_synonyms()` that reads `~/.config/sy-knowledge/synonyms.yaml`
(honouring `XDG_CONFIG_HOME`) via `serde_yml` — a missing, empty, or
unparseable file degrades to an empty table (pure no-op). The live consumer is
`daemon::handle_search_rerank`: the dense leg still embeds the unmodified
`query`, and only the sparse leg computes
`sparse::encode(&query::expand_synonyms(&query, &query::load_synonyms()))`
(REQ-7). The default table ships at `configs/sy-knowledge/synonyms.yaml`
(X5 → Пятёрочка/Перекрёсток/Чижик/X5 Group; Магнит → Тандер) and reaches
`~/.config/sy-knowledge/synonyms.yaml` through the existing `sy apply`
config-tree walk in `src/main.rs::apply` (every file under `configs/` is
templated/copied to the target — no manifest entry needed, no snowflake).

---

## Step 18 — RU/EN date-expression expander
**Goal:** Natural-language time phrases auto-fill `date_from`/`date_to`
when the caller gave none (REQ-8). *(was Step 22)*
**Files:** `src/knowledge/query.rs` (modified: `expand_dates`),
`src/knowledge/cli.rs` / `daemon.rs` (modified: search path calls
`expand_dates` when no explicit date filter), `Cargo.toml` (add
`two_timer` for English ranges)
**Tests:**
- `::tests::ru_new_year_holidays_2024_maps_to_dec31_jan08`
- `::tests::en_in_january_and_last_summer_map_to_ranges`
- `::tests::explicit_date_args_override_expansion`
- `::tests::unrecognized_phrase_is_noop_and_logged`
**Definition of Done:**
- [x] RU/EN lexicon (seasons + Russian holidays) + `two_timer` for generic English; fills dates only when absent; explicit args win; **called from the live search path** (no dead code)
- [x] tests pass; lint green
**Risks / unknowns:** lexicon coverage is open-ended; misses are logged and overridable. Audit-check `two_timer`'s transitive deps before adding (avoid a repeat of the `bm25`→`fxhash` advisory); if it pulls an unmaintained crate, fall back to an in-house English range parser.

**Mapping (Step 18):** `src/knowledge/query.rs` carries the **pure**
`expand_dates(query, now: chrono::NaiveDate) -> Option<(date_from, date_to)>`
(RFC-3339, inclusive day bounds). `now` is injected (the daemon reads the
clock), so unit tests are clock-free/deterministic. Two layers: (1) an
in-Rust **RU/EN lexicon** — Russian New-Year holidays (Dec 31 prev-year →
Jan 8, year taken from the query or `now`), meteorological seasons
(зима/весна/лето/осень + winter/spring/summer/fall, "last <season>" →
prior year), and "in <Month>" — the SPEC's chosen approach (no
Duckling/HeidelTime snowflake); (2) **`two_timer` 2.2.5** for generic
English relative/range phrases ("last month", "next year"), anchored to
`now` via `Config::now(NaiveDateTime)` and converted from its half-open
`[start,end)` to inclusive day bounds. The live consumer is
`daemon::handle_search_rerank` via `query::maybe_fill_dates(&mut filter,
&query, today)`, called **before** `qdrant::build_filter`: it fills
`date_from`/`date_to` only when the caller gave neither bound (explicit
args win — `explicit_date_args_override_expansion`); an unrecognized
phrase is a `tracing::debug!` no-op (`unrecognized_phrase_is_noop_and_logged`).
*(Audit: `two_timer` adds `pidgin`/`serde_regex`/`regex`/`lazy_static`
transitively; `cargo deny check advisories` on its isolated tree is
clean — no RUSTSEC, unlike the rejected `bm25`→`fxhash`. The pre-existing
`yaml-rust`/`paste` advisories in the workspace do NOT trace through
`two_timer`. No in-house English fallback needed.)*

---

## Cross-cutting Definition of Done
- [x] All step DoDs satisfied
- [x] **Every new module/function is reachable from a non-test consumer** — `cargo clippy --workspace --all-targets -D warnings` clean with zero `#[allow(dead_code)]` outside `#[cfg(test)]` (verified after Step 18: `query::expand_dates`/`maybe_fill_dates` are consumed by `daemon::handle_search_rerank`; clippy green)
- [x] End-to-end journey VERIFIED LIVE on the operator's machine (2026-06-02):
  release build → `sy apply` (upgraded qdrant 1.12.4→1.18.1, installed
  `synonyms.yaml`) → fresh schema-v2 named `dense`+`sparse` collection →
  hybrid search (RRF `k=60`) returns results with a `confidence` (MCP
  envelope `{confidence, abstained, total, results}`), and
  `~/.claude/projects/**` transcripts are EXCLUDED from default scope
  (8 hits / 0 transcripts) yet returned under `--kind claude-transcripts`
  (8/8). NOTE: required a post-install fix — per-file `kind` classification
  (`effective_kind` in `src/knowledge/cli.rs`), because the `~/.claude`
  manifest root classified as `Generic` and left transcript files unkinded.
  Full-corpus re-embed (~124k chunks) runs in the daemon background.
- [ ] `sy knowledge eval --json` reports recall@1/5, MRR, abstain accuracy; `make eval` green in CI and gates regressions
- [x] `knowledge_search` / `knowledge_get_chunk` MCP surface matches SPEC §4 (`--json` schema, `chunk_id`, `truncated`, `total`, `confidence`) — landed in Steps 11 + 14 (`src/knowledge/mcp.rs`)
- [ ] `sy knowledge status` / waybar reflect index + transcription backlog
- [x] qdrant ≥ 1.16 asserted at daemon start (+ a `sy doctor` check in `src/doctor/mod.rs`); RRF `k=60` set explicitly — `QDRANT_VERSION` bumped to `1.18.1` (`src/main.rs`); `qdrant::{parse_version, meets_min_version, server_version, MIN_HYBRID_VERSION}` probe `GET /`; daemon `run()` warns loudly (no hard crash) below 1.16 via `qdrant_version_warning`; `knowledge.qdrant.version_min_1_16` doctor check (pass ≥1.16 / fail <1.16 with `sy apply` fix / warn when unreachable). RRF `k=60` already explicit since Step 5 (`qdrant::query_hybrid`).
- [x] README updated for the new search args + hybrid retrieval + `kind`/source model + default-exclude of transcripts + `get-chunk`/`eval` (README.md "knowledge" section + CLI quick-ref; golden `sy-plugin-md-readme.golden.png` regenerated via `cargo run -p sy-plugin-md --example regen_goldens`)

## Out of Scope
- NPU-accelerated transcription (VitisAI/XDNA Whisper is Windows-only on Linux today) — CPU/iGPU only this iteration
- Swapping the embedding model (e.g. to BGE-M3 native sparse) or the reranker
- The `bm25` crate (pulls unmaintained `fxhash` / RUSTSEC-2025-0057) — sparse is an in-house term-frequency encoder instead
- distil-whisper (English-only) for the Russian corpus
- Embedding Duckling / HeidelTime (separate language runtimes = snowflake) — in-Rust lexicon instead
- LLM-based self-query filter extraction (nondeterministic)
- Cross-document entity/knowledge graph; UI/dashboard; multi-tenant/sharing; remote embed/rerank providers
- DBSF fusion tuning (RRF is the default; DBSF stays available but untuned)
- Optional 5-message Telegram context-window chunk (per-message only until eval shows a recall gap — SPEC open question)
