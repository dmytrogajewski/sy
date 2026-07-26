# SPEC: sy-knowledge retrieval iteration 1 — hybrid retrieval, payload filters, per-source pipelines, transcription, calibration & eval

> Source feedback: [`specs/knowledge-feedback-iter1/FEEDBACK.md`](../../knowledge-feedback-iter1/FEEDBACK.md)
> (REQ-1 … REQ-10). This spec grounds those requirements in current
> qdrant / BGE / Whisper / MCP reality and turns them into one cohesive,
> implementable change set.

## 1. Summary

`sy-knowledge` today is **dense-only cosine retrieval** over a single
unnamed 768-dim vector (multilingual-e5-base) plus a bge-reranker-v2-m3
rerank stage, with the only filter surface being a `source` path prefix.
A real failing session (a specific-fact lookup over a 76k-message
Russian Telegram export) exposed six product-level failures: the agent's
own transcripts poison the index, chunks are huge and mix unrelated
years, there is no date/author/kind filter, literal named entities
(`X5`, `Магнит`) are ignored by pure dense retrieval, the tool never
abstains, and voice notes are never transcribed.

This iteration rebuilds the retrieval core into **hybrid (lexical sparse
+ dense, fused by Reciprocal Rank Fusion) with pre-search payload
filters**, adds **per-source preprocessing pipelines** that emit
per-message records with structured metadata, **transcribes voice/video
media on CPU/iGPU**, **calibrates confidence and supports abstain**, and
ships a **deterministic eval harness** to lock the gains. It is for the
single-host rice user and the MCP-driven agent that drives
`knowledge_search`. No embedding-model or reranker swap is required.

## 2. Background & Research

### Market Context

- **qdrant** is the substrate sy already uses. Its **Universal Query
  API** (introduced in qdrant 1.10, 2024-07-01) consolidates dense
  search, sparse search, fusion, and reranking into one `query` call
  with `prefetch` legs — exactly the hybrid shape this spec needs.
  <https://qdrant.tech/blog/qdrant-1.10.x/>,
  <https://qdrant.tech/documentation/search/hybrid-queries/>
- **BGE-M3 / FlagEmbedding** (the family sy's reranker already belongs
  to) explicitly recommends a **dense + sparse fusion → reranker**
  pipeline, validating the architecture here.
  <https://github.com/FlagOpen/FlagEmbedding/blob/master/research/BGE_M3/README.md>
- **Cohere Rerank** documents the practitioner method for turning
  reranker scores into an abstain threshold (calibrate on 30–50 domain
  queries; scores are for *ranking*, not absolute magnitudes), which
  directly informs REQ-6.
  <https://docs.cohere.com/docs/reranking-best-practices>
- **LangChain SelfQueryRetriever / Instructor time-filters / Haystack
  metadata extraction** are the mainstream way RAG systems turn a
  natural-language query into `query + structured filters` — prior art
  for REQ-2/REQ-8, though all are LLM-based; sy will do the deterministic
  rule-based analogue.
  <https://python.useinstructor.com/blog/2024/06/06/enhancing-rag-with-time-filters-using-instructor/>,
  <https://app.ailog.fr/en/blog/guides/self-query-retrieval>
- **BEIR** is the standard heterogeneous IR benchmark (nDCG@10); its
  *methodology* (query → qrels → ranked list → metric), not its scale,
  is what a 20–40-pair golden set should imitate for REQ-9.
  <https://arxiv.org/abs/2104.08663>

### Technical Context

- **Hybrid request shape (verified).** Two `prefetch` legs — a dense
  float-array leg (`using: "dense"`) and a sparse `indices/values` leg
  (`using: "sparse"`) — fused by `"query": { "rrf": {} }`. The Rust
  `qdrant-client` crate (current **1.18.0**, 2026-05-11) fully supports
  this via `PrefetchQueryBuilder`, `Query::new_rrf`, `RrfBuilder`,
  `SparseVectorsConfigBuilder`, and `Modifier::Idf`.
  <https://qdrant.tech/documentation/search/hybrid-queries/>,
  <https://docs.rs/qdrant-client/latest/qdrant_client/qdrant/struct.PrefetchQueryBuilder.html>,
  <https://crates.io/crates/qdrant-client>
- **RRF `k` gotcha (load-bearing).** qdrant's built-in RRF default is
  **`k = 2`, NOT the textbook 60**. Configurable `k` only exists as of
  **qdrant v1.16.0** (`"query": { "rrf": { "k": 60 } }`). Rank fusion is
  scale-invariant — it discards raw scores and uses only rank position,
  which is exactly why it fixes the dense-cosine (bounded) vs sparse-BM25
  (unbounded) score-scale mismatch that breaks weighted-sum fusion.
  <https://github.com/qdrant/landing_page/blob/master/qdrant-landing/content/documentation/search/hybrid-queries.md>,
  <https://bigdataboutique.com/blog/reciprocal-rank-fusion-how-it-works-and-when-to-use-it>
- **Pre-search filters (verified).** qdrant filters are true
  pre-filters evaluated *during* HNSW traversal (filterable HNSW), not
  post-processing — so a date/kind filter constrains the candidate pool
  before scoring, satisfying REQ-2's "before reranking" requirement.
  Native `datetime` payload index (RFC 3339, since v1.8.0) means no
  epoch-int conversion. Payload indexes **must be created before
  ingest** to build the filter-aware graph edges.
  <https://qdrant.tech/documentation/concepts/indexing/>,
  <https://qdrant.tech/documentation/search/filtering/>,
  <https://qdrant.tech/articles/vector-search-filtering/>
- **Sparse generation in Rust without a model swap.** The pure-Rust
  **`bm25` crate (2.3.2)** emits `TokenEmbedding { index, value }`
  sparse vectors (token-id → BM25 weight), documented as directly
  uploadable to qdrant, with a multilingual `DefaultTokenizer`
  (stemming, stop-words, unicode normalization). Alternatively
  `fastembed` 5.x can produce BGE-M3 *learned* sparse, but only by
  introducing the BGE-M3 model (an embedding-model change). qdrant's
  `modifier: "idf"` computes IDF server-side, keeping weights correct
  under incremental insert/delete.
  <https://docs.rs/bm25/latest/bm25/>,
  <https://github.com/Anush008/fastembed-rs>,
  <https://qdrant.tech/articles/bm42/>
- **Reranker score semantics (verified).** bge-reranker-v2-m3 outputs
  an **unbounded raw logit**, not a normalized score; map to [0,1] with
  sigmoid. Card examples: irrelevant pair raw ≈ −8.19 → 0.00028; strong
  match raw ≈ +5.26 → 0.9948. Logit ≈ 0 (sigmoid 0.5) is the model's
  indifference point — the natural decision boundary for REQ-6.
  <https://huggingface.co/BAAI/bge-reranker-v2-m3>,
  <https://bge-model.com/bge/bge_reranker_v2.html>

### Deep Dives

- **Whisper on AMD Ryzen AI is Windows-only today (decisive for
  REQ-5).** Both NPU paths — AMD's `whisper.cpp` fork
  (`-DWHISPER_VITISAI=1`, offloads encoder to NPU) and the ONNX Runtime
  **VitisAI EP** — document **Windows-only NPU support; Linux is
  "planned."** On Fedora 43 the realistic accelerated path is the
  **Radeon iGPU via whisper.cpp Vulkan/ROCm** (~4.45× real-time on a
  Radeon 8060S) or **faster-whisper INT8 on CPU** (~4× over reference
  openai/whisper). `whisper-rs` (now on Codeberg; the GitHub repo was
  archived 2025-07-30) exposes `vulkan`/`hipblas`/`openblas` features —
  **no XDNA/NPU feature exists.** The NPU also caps at Whisper
  **medium** (large "exceeds practical NPU limits").
  <https://ryzenai.docs.amd.com/en/latest/whisper_cpp.html>,
  <https://onnxruntime.ai/docs/execution-providers/Vitis-AI-ExecutionProvider.html>,
  <https://codeberg.org/tazz4843/whisper-rs>,
  <https://rocm.blogs.amd.com/artificial-intelligence/whisper/README.html>,
  <https://www.amd.com/en/developer/resources/technical-articles/2025/unlocking-on-device-asr-with-whisper-on-ryzen-ai-npus.html>
- **distil-whisper is English-only** in its canonical releases — unfit
  for a Russian Telegram corpus. Russian quality floor: whisper-large-v3
  scores 9.84 WER on Common Voice 17 Russian; the Russian fine-tune
  `antony66/whisper-large-v3-russian` cuts that to 6.39 WER. Telegram
  voice notes (conversational, accented, compressed Opus) are the hard
  case — argues for the largest affordable multilingual model.
  <https://github.com/SYSTRAN/faster-whisper>,
  <https://huggingface.co/antony66/whisper-large-v3-russian>
- **No Rust crate parses Russian dates / seasons / holidays.**
  `two_timer` is the only Rust crate returning *ranges* (English only);
  `interim`/`chrono-english`/`htp` are single-instant English. The
  established multilingual taggers (**HeidelTime** has Russian
  resources; **Facebook Duckling** has a `RU` Time dir + holiday
  helpers) are all rule/regex + knowledge-resource based — i.e. a small
  in-Rust lexicon is the same technique at smaller scale, and avoids
  embedding a Haskell runtime.
  <https://docs.rs/two_timer/latest/two_timer/>,
  <https://github.com/HeidelTime/heideltime>,
  <https://github.com/facebook/duckling/tree/master/Duckling/Time>
- **MCP bounds tool-result size by convention, not spec.** Pagination
  in the MCP spec (2025-06-18) covers **list ops only** (`tools/list`,
  `resources/list`, …) — **`tools/call` results are not paginated.** The
  spec-blessed pattern for large data is **summary + fetch-by-id** via
  `resource_link` / a dedicated `get` tool, with a self-defined
  `truncated`/`total` field inside `structuredContent`; the only
  protocol-level truncation hook is `_meta.truncated` (since 2025-03-26).
  This matches REQ-10 and the work already started in commit `d12528a`.
  <https://modelcontextprotocol.io/specification/2025-06-18/server/tools>,
  <https://modelcontextprotocol.io/specification/2025-06-18/server/utilities/pagination>

### Current sy-knowledge architecture (what we extend vs build)

| Concern | Today | File |
|---|---|---|
| Source registry | `sy.toml [[knowledge.sources]]`, `{path, enabled, mode}`; no `kind`/`name` | `src/knowledge/sources.rs` |
| Indexing | walk → extract → 512-tok/64-overlap whitespace chunks → embed batch (64) → upsert; offloaded to daemon | `src/knowledge/cli.rs`, `daemon.rs` |
| qdrant schema | **single unnamed dense vector**, 768-dim, Cosine; payload `{source,file_path,chunk_index,chunk_text,file_mtime,content_hash,tags}`; only `tags` keyword-indexed | `src/knowledge/qdrant.rs` |
| Search | embed query → dense top-N → optional rerank (TextPair) → sort → truncate | `daemon.rs::handle_search_rerank` |
| MCP | `knowledge_search` / `knowledge_index` / `knowledge_list_sources`; `MAX_LIMIT=20`, `MAX_CANDIDATES=64`, `MAX_CHUNK_CHARS=2000`, `truncated` flag | `src/knowledge/mcp.rs` |
| IPC | Unix socket `sy-knowledge.sock`; `Req::Search{...}` / `Req::SearchRerank{...}`; embed/rerank via aiplane supervisor NPU workers | `src/aiplane/ipc.rs`, `daemon.rs` |
| Exec | embed = multilingual-e5-base, rerank = bge-reranker-v2-m3, both NPU subprocess workloads | `src/aiplane/workloads/{embed,rerank}.rs` |
| Tests | MCP stub round-trip; no daemon-in-thread for knowledge (power/daemon has the pattern) | `tests/sy_file_knowledge.rs` |

## 3. Proposal

### Approach

Rebuild the retrieval core around qdrant's Universal Query API with a
**named-vector collection** (`dense` + `sparse`) and a rich, indexed
payload. Generate the sparse signal **in pure Rust (BM25)** so no model
changes. Replace the generic chunker with **per-source pipelines** that
emit one record per logical unit (Telegram message, transcript turn)
with `{date, from, kind, source_name, has_media, …}` payload. Expose
**structured filters** (`date_from/to`, `from`, `kind`,
`include/exclude_sources`) applied as qdrant pre-filters. **Transcribe
voice/video on CPU/iGPU** and index transcripts as filterable chunks.
Add **confidence + abstain** computed from the calibrated reranker
sigmoid and top1−top2 margin. Ship a **`sy knowledge eval`** harness and
a **`knowledge_get_chunk`** fetch-by-id endpoint. Segregate agent
transcripts as a default-excluded `kind`.

### Key Decisions

| Decision | Choice | Reasoning | Alternatives |
|---|---|---|---|
| Sparse signal | **Pure-Rust BM25** (`bm25` crate) lexical sparse vector + qdrant server-side `modifier: idf` | Honors the "no embedding-model swap" non-goal; no 768→1024 re-embed; BM25 is precisely the tool for literal entity matches (`X5`, `Магнит`) — the dominant failure; no extra NPU contention | **BGE-M3 learned sparse** (rejected: forces embedding-model swap + full re-embed; out of scope per feedback non-goals); SPLADE (same objection) |
| Fusion | **RRF via Universal Query API**, pin qdrant **≥1.16.0**, set **`k=60` explicitly** | Rank fusion is scale-invariant across cosine vs unbounded BM25; one round-trip; qdrant's default `k=2` is too top-heavy | DBSF (kept available as a tunable; RRF default); client-side fusion (rejected: extra round-trips, reimplements qdrant) |
| Collection schema | **Named vectors `dense`+`sparse`** + payload indexes, via a **versioned full re-index migration** | Sparse needs a named-vector collection; payload indexes must exist *before* ingest for filterable HNSW; single-host can re-index cheaply | Dual-collection / shadow write (rejected: fragmentation, sync complexity) |
| Transcription backend | **CPU faster-whisper / whisper-rs**, multilingual **large-v3 (Russian fine-tune)**, **medium** fallback; iGPU Vulkan/ROCm when available | VitisAI/XDNA Whisper is **Windows-only**; distil-large-v3 is English-only; Russian corpus needs a multilingual large model | NPU Whisper (anti-goal — Linux unsupported); distil-large-v3 (wrong language) |
| Date expansion | **In-Rust rule-based RU/EN lexicon** (seasons + Russian holidays) + `two_timer`/`interim` for generic English ranges | No Rust crate does Russian/seasons/holidays; deterministic; no extra runtime; "no snowflakes" | Duckling (anti-goal — Haskell HTTP runtime = snowflake); LLM self-query (adds model dep + nondeterminism) |
| Transcript segregation | **`kind` payload field + named sources**; `kind=claude-transcripts` **excluded by default** | Stops self-poisoning at the filter layer; reuses REQ-2 machinery; one mechanism for all source kinds | Separate collection (rejected: a filter already solves it) |
| Confidence / abstain | **sigmoid(reranker logit)** + **top1−top2 margin**; `abstain_threshold` is a **tool parameter** with a server default; cutoff calibrated on the eval set's negatives | Reranker emits raw logits (logit 0 = indifference); per-query margin is robust to query-dependent score scales (Cohere guidance); per-query strict/permissive control | Global absolute threshold only (fragile per Cohere); learned answerability model (heavier; revisit) |
| Telegram source of truth | **`result.json` primary via streaming parse**, **HTML fallback** when JSON is invalid/truncated | Per-message granularity + canonical metadata; streaming survives multi-GB / `.partN` / truncated exports (this user's JSON is invalid past ~25 MB) | HTML-primary (rejected: brittle DOM parsing); whole-file JSON load (rejected: OOM on multi-GB) |

### Scope

One cohesive change set covering all ten requirements:

1. **Source model (REQ-1)** — extend `Source` with a stable `name` and a
   `kind` enum (`telegram`, `claude-transcripts`, `email`, `slack`,
   `notes`, `code`, `generic`). Auto-classify well-known paths
   (`~/.claude/projects/**` → `claude-transcripts`). `knowledge_search`
   **excludes `claude-transcripts` from the default scope** unless named
   in `include_sources`/`kind`.
2. **Payload schema + indexes (REQ-2)** — payload gains `kind`,
   `source_name`, `date` (RFC 3339 datetime, when derivable), `from`
   (keyword), `has_media` (bool), `message_id`, `reply_to_id`. Create
   `datetime`, `keyword`, and `bool` payload indexes at collection-create
   time (before ingest). Filters (`date_from`, `date_to`, `from`,
   `kind`, `include_sources`, `exclude_sources`) compile to a qdrant
   `Filter` applied as a pre-filter on **both** prefetch legs.
3. **Hybrid retrieval (REQ-3)** — add a `sparse` named vector; generate
   BM25 sparse vectors at index and query time; issue a single Universal
   Query (`dense` prefetch + `sparse` prefetch → RRF `k=60`) feeding the
   existing rerank stage with a smaller, better-grounded candidate set.
4. **Per-source pipelines (REQ-4)** — a `Pipeline` trait selected by
   `kind`. **Telegram**: one chunk per message
   (`{date, from, file, message_id, reply_to_id, has_media}`), JSON
   streaming-primary + HTML fallback, plus an optional sliding
   5-message context chunk. **Claude transcripts**: one chunk per turn
   (`{role, model, project_id, ts}`). **Generic md/text**: keep the
   current chunker at a smaller 500–800-token target. Pipelines tolerate
   malformed input without aborting the index pass.
5. **Transcription (REQ-5)** — detect `voice_messages/`,
   `round_video_messages/`, etc.; transcribe with Whisper
   (CPU/iGPU) via `whisper-rs`; **cache transcripts next to the source
   media** (content-addressed); emit a chunk with `kind: telegram-voice`
   and payload pointing at the original media. Incremental: only
   un-transcribed media is processed.
6. **Confidence & abstain (REQ-6)** — `knowledge_search` returns a
   `confidence` field per response (from top-1 sigmoid score and the
   top-1/top-2 spread) and honors an `abstain_threshold`; below it,
   returns `{results: [], reason: "no high-confidence match",
   confidence}`.
7. **Synonym expansion (REQ-7)** — load `~/.config/sy-knowledge/synonyms.yaml`
   (shipped from `configs/`); at query time OR the aliases into the
   **sparse-side** query only (dense already handles synonymy).
8. **Date/time expression handling (REQ-8)** — a query pre-parser
   detects RU/EN time expressions ("новогодние праздники 2024", "in
   January", "прошлым летом") and fills `date_from`/`date_to` when the
   caller didn't supply them; explicit filter args always override.
9. **Eval harness (REQ-9)** — `specs/knowledge-feedback-iter1/eval/queries.jsonl`
   (20–40 labelled `{query, expected, answerable, kind?, date_from?, …}`
   rows, ≥5 each of named-entity / date-range / abstain / cross-source);
   `sy knowledge eval --json` reports recall@1, recall@5, MRR, abstain
   accuracy; `make eval`; CI gate on regression tolerance.
10. **Response hygiene (REQ-10)** — keep the per-chunk char cap and total
    cap; add a stable `chunk_id` to every result and a
    `knowledge_get_chunk(id)` MCP tool + `sy knowledge get-chunk` CLI;
    surface `truncated` and `total` in `structuredContent`.

### Anti-Goals

- **NPU-accelerated transcription.** The VitisAI/XDNA Whisper path is
  Windows-only; Linux is unsupported. Targeting it now would be building
  against a non-existent platform capability. Transcription runs on
  CPU/iGPU; an NPU backend is revisited if/when AMD ships Linux support.
- **distil-whisper.** English-only — the wrong primitive for a Russian
  corpus.
- **Swapping the embedding model (to BGE-M3 or anything) and swapping
  the reranker.** Carried from the feedback non-goals; additionally a
  dense-model swap forces a 768→1024 full re-embed and a collection
  rebuild for no benefit the BM25 sparse signal doesn't already deliver
  for the literal-match failure.
- **Embedding Duckling / HeidelTime for date parsing.** Both are
  separate language runtimes (Haskell / Java) — a snowflake hazard on
  the rice and a heavy dependency for what a compact in-Rust lexicon
  covers.
- **LLM-based self-query filter extraction.** Introduces a model
  dependency and nondeterminism into a path that must be reproducible
  and offline; the rule-based expander covers the common RU/EN cases.
- **Cross-document entity/knowledge graph.** A different storage
  primitive orthogonal to retrieval quality; the failing session is
  solvable with hybrid retrieval + filters. (Feedback non-goal.)
- **UI/dashboard, multi-tenant/sharing, remote embed/rerank providers.**
  Wrong surface / architectural mismatch for a single-host CLI+MCP+waybar
  rice; remote providers add latency and snowflake config.

## 4. Technical Design

### Architecture

Modules (extend unless marked **new**):

- `src/knowledge/sources.rs` — add `name: String`, `kind: SourceKind`;
  auto-classification; `default_scope_excludes()`.
- `src/knowledge/qdrant.rs` — named-vector collection (`dense` Cosine +
  `sparse`), payload index creation, Universal Query (`query` endpoint
  with `prefetch` + `rrf`), `Filter` builder from search args.
- `src/knowledge/sparse.rs` **(new)** — BM25 tokenizer + sparse vector
  `{indices, values}`; corpus IDF handled by qdrant `modifier: idf`.
- `src/knowledge/pipeline/mod.rs` **(new)** + `telegram.rs`,
  `transcripts.rs`, `generic.rs` — `trait Pipeline { fn records(&self,
  file) -> Vec<Record> }`, `Record { text, payload, chunk_id }`.
- `src/knowledge/transcribe.rs` **(new)** — `whisper-rs` wrapper, model
  resolution, sidecar transcript cache.
- `src/knowledge/query.rs` **(new)** — date-expression expander
  (RU/EN lexicon + `two_timer`), synonym expansion from `synonyms.yaml`.
- `src/knowledge/calibrate.rs` **(new)** — `confidence(scores)` and
  abstain decision.
- `src/knowledge/eval.rs` **(new)** — golden-set runner + metrics.
- `src/knowledge/mcp.rs` — extend `knowledge_search` schema; add
  `knowledge_get_chunk`.
- `src/aiplane/ipc.rs` — extend `Req::Search`/`Req::SearchRerank` with
  `filter`, `abstain_threshold`; add `Req::GetChunk{chunk_id}`; `Resp`
  carries `confidence`. (Default: extend existing IPC, no new channel.)
- `configs/sy-knowledge/synonyms.yaml` **(new)** — default applied by
  `sy apply`.

Data flow (search): query → `query.rs` (date + synonym expansion) →
embed (dense, NPU) + BM25 (sparse, CPU) → `qdrant.rs` Universal Query
(`dense`+`sparse` prefetch with shared `Filter` → RRF `k=60`,
limit=candidates) → rerank (NPU, TextPair) → `calibrate.rs`
(confidence/abstain) → `mcp.rs` (cap + `chunk_id` + `truncated`/`total`).

Transcription is a CPU/iGPU job in the index pass (not an NPU aiplane
workload, since Whisper cannot use the NPU on Linux); it may run on a
bounded worker pool with the same `cpu_max_percent`/`nice` throttle the
indexer already honors.

### Non-Functional Requirements

- **Performance.**
  - Hybrid query adds one CPU BM25 encode (sub-ms for short queries) and
    a second prefetch leg; pre-filters shrink the candidate pool, so net
    rerank cost should not exceed today's. Gate: **`knowledge_search`
    (rerank on, 8 candidates) p99 ≤ today's measured rerank latency +
    20%.**
  - Filter gate (REQ-2): a query with `date_from/to + kind` returns only
    matching points (verified by eval).
  - BM25 gate (REQ-3): a query containing a rare literal token present
    in exactly one indexed chunk returns that chunk in **top-3**.
  - Transcription: CPU faster-whisper ≈ 4× real-time; document expected
    throughput; the pass must remain cancellable and throttled.
  - Migration re-index cost is bounded by corpus size; report progress
    via the existing heartbeat/status mechanism.
- **Reliability.** Pipelines never abort the whole pass on one malformed
  file (REQ-4) — per-file errors are logged and skipped. Streaming JSON
  parse tolerates truncated/`.partN` exports. Transcript cache is
  content-addressed and idempotent. Collection migration is versioned;
  an interrupted migration resumes (re-index is idempotent on point id).
- **Security.** All new filter inputs validated at the CLI/MCP boundary
  (ISO-8601 parse, kind enum, source-name allowlist against the
  registry). `synonyms.yaml` read-only, user-scoped perms. Transcript
  cache inherits source-dir perms; no media leaves the host. qdrant
  stays bound to `127.0.0.1`.
- **Observability.** New `tracing` spans: `kb.sparse_encode`,
  `kb.hybrid_query`, `kb.filter`, `kb.transcribe`, `kb.calibrate`.
  Structured stderr (`--log-format json`) includes
  `{confidence, abstained, candidates, filtered_count}`. Waybar tooltip
  unchanged except an optional transcription-backlog count.

### CLI / MCP Surface

`sy knowledge search` (and `knowledge_search`) — additive args:

| Arg | Env | Type | Default |
|---|---|---|---|
| `--date-from` | `SY_KB_DATE_FROM` | ISO-8601 | none |
| `--date-to` | `SY_KB_DATE_TO` | ISO-8601 | none |
| `--from` | `SY_KB_FROM` | string (repeatable) | none |
| `--kind` | `SY_KB_KIND` | enum (repeatable) | all except `claude-transcripts` |
| `--include-source` | `SY_KB_INCLUDE` | name (repeatable) | none |
| `--exclude-source` | `SY_KB_EXCLUDE` | name (repeatable) | `claude-transcripts` |
| `--abstain-threshold` | `SY_KB_ABSTAIN` | float 0–1 | server default (e.g. 0.5) |
| `--no-rerank` | `SY_KB_NO_RERANK` | bool | false |

- **`--json` schema** (`knowledge_search`): `{ confidence: f32,
  abstained: bool, reason?: string, total: u32, truncated: bool,
  results: [{ chunk_id, score, embed_score?, rerank_score?, kind,
  source_name, file_path, chunk_index, date?, from?, has_media?,
  chunk_text, chunk_chars, truncated }] }`.
- **New `knowledge_get_chunk` / `sy knowledge get-chunk <chunk_id>`** →
  `{ chunk_id, file_path, chunk_index, kind, source_name, payload, text }`
  (full, uncapped text).
- **New `sy knowledge eval [--json]`** → `{ recall_at_1, recall_at_5,
  mrr, abstain_accuracy, n }`.
- **Exit codes** (unchanged convention): 0 ok, 1 generic, 2 usage,
  3 drift; `eval` returns **non-zero** when a metric regresses past the
  configured tolerance (for CI).
- Non-interactive when stdin isn't a TTY; no prompts; `--json` everywhere.

### Testing Strategy

- **Unit.** BM25 sparse vector generation (token ids/weights stable for
  a fixed corpus); date expander (RU/EN: "новогодние праздники 2024" →
  `2023-12-31..2024-01-08`, "in January", "прошлым летом"); Telegram
  per-message parser (message count == chunk count ± delta; `date`/`from`
  populated; reply links; media detection); streaming-parse of a
  truncated `result.json`; synonym expansion (sparse-only); calibration
  (`sigmoid`, margin, abstain boundary at logit 0); filter compilation to
  qdrant `Filter`.
- **Integration (daemon-in-thread).** Introduce the daemon-in-thread
  pattern (mirroring `power/daemon`) for knowledge: stand up an ephemeral
  qdrant + stub embed/rerank, index a fixture corpus (small Telegram
  export + a fake claude-transcripts dir), then assert: (a) a fresh
  prompt never returns its own transcript embedding in default scope
  (REQ-1); (b) `{date_from,date_to,kind}` returns only in-window
  telegram (REQ-2); (c) a rare literal token returns its chunk in top-3
  (REQ-3); (d) a provably-absent answer yields `confidence < threshold`
  and abstains (REQ-6); (e) `knowledge_get_chunk` round-trips full text
  (REQ-10).
- **E2E / manual recipe.** `sy knowledge eval` against the live index
  after a full re-index of the real Telegram export; transcription smoke
  test on a handful of voice notes; `make eval` in CI.

### Migration & Compatibility

- **Collection schema change** (unnamed dense → named `dense`+`sparse`,
  new payload indexes) is **not** in-place compatible. Ship a versioned
  collection name or a stored `schema_version`; on daemon start, if the
  live collection predates this schema, trigger a **`FullResync`**
  (drop + re-create with indexes-before-ingest, then re-embed). Re-index
  is idempotent on point id; an interrupted run resumes.
- **State file** (`index.json`) gains per-file `kind`/`source_name`;
  reading an old state without them defaults to `generic` and forces a
  re-pass for that file.
- **Transcript sidecars** are new files alongside media; additive, no
  migration.
- **`synonyms.yaml`** is created by `sy apply` if absent; user edits
  preserved (managed like other `configs/` dotfiles).

### Dependencies

| Crate / artifact | Purpose | Assessment |
|---|---|---|
| `qdrant-client` (already? REST today) → optionally gRPC 1.18.0 | Universal Query / sparse / filters | Official, actively maintained (2026-05). REST also works; keep REST if preferred to avoid a new dep. |
| `bm25` 2.3.2 | Pure-Rust sparse vectors | Small, focused, multilingual tokenizer; qdrant-ready output. Audit-check. |
| `whisper-rs` (Codeberg) | CPU/iGPU transcription | Mature; GitHub mirror archived — pin the Codeberg source. Vendors whisper.cpp (submodule, C build). FFI/system-lib cost on the rice install path — must be productized in the build, not a manual step. |
| `two_timer` 2.2.5 (+ `interim` 0.2.1) | English range/relative dates | Small; English only — RU handled by the in-Rust lexicon. |
| Whisper model weights (large-v3 Russian fine-tune; medium fallback) | ASR | Downloaded on first use into the model cache, same as existing embed/rerank ONNX models; not vendored. |

`whisper-rs` is the only heavyweight addition (vendors a C library and a
multi-hundred-MB model). It must be wired into the reproducible build so
a fresh `cargo build --release && sy apply` produces a working
transcription path with **no manual host steps** ("no snowflakes").

## 5. User Journey Sketch

**Actor:** the rice user (or an MCP agent acting for them) doing a
specific-fact lookup over a large personal corpus.

1. **Register & index** — `sy knowledge add ~/knowledge/telegram/…`
   classifies it `kind=telegram`; the daemon re-indexes with per-message
   chunks, transcribes voice notes, and excludes the user's own
   `~/.claude/projects` transcripts from default scope.
2. **Ask in natural language** — `knowledge_search "новый год X5 Магнит
   Лу"`. The query pre-parser expands "новый год" → a Dec–Jan window and
   ORs `X5 → {Пятёрочка, Перекрёсток}` into the sparse leg.
3. **Hybrid retrieve** — dense + BM25 prefetch under a `kind=telegram`
   pre-filter, RRF-fused, reranked; the literal `X5`/`Магнит` chunk now
   surfaces because BM25 caught the exact token.
4. **Calibrated answer or abstain** — results carry `confidence`; if the
   fact genuinely isn't in the corpus, the tool **abstains** instead of
   quoting background noise, so the agent stops digging.
5. **Drill in** — a bounded result lists `chunk_id`s; the agent fetches
   full context with `knowledge_get_chunk` instead of dumping 67 KB.
6. **Lock it in** — the failing query becomes a row in the eval set;
   `make eval` keeps the fix from regressing.

### Friction Map

| Friction | Phase | Opportunity |
|---|---|---|
| First full re-index after migration is heavyweight | Index | Progress via existing heartbeat/status; resumable, throttled; communicate clearly in CLI/waybar |
| Transcribing thousands of voice notes is slow on CPU | Index | Incremental + content-addressed cache; bounded worker pool; iGPU Vulkan when present; surface backlog count |
| Russian date phrases are open-ended; lexicon will miss some | Query | Explicit `--date-from/--date-to` always override; log when no expansion matched so gaps are visible |
| Abstain threshold too strict hides real hits / too loose returns noise | Search | Per-query `abstain_threshold`; calibrate against the eval negatives; return `confidence` always so callers decide |
| `synonyms.yaml` drift vs the corpus | Query | Ship sensible defaults in `configs/`; user-editable; expansion applied only to sparse leg so it can't hurt dense recall |

### North Star

A specific-fact question in Russian or English returns the exact
message (text **or** transcribed voice note) within the right date
window in the top few hits — or a confident "not found" — without the
agent ever falling back to `grep`.

## 6. Risks & Mitigation

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| qdrant < 1.16 in the rice → RRF `k` not configurable (silently `k=2`) | Med | Med | Pin/assert qdrant ≥ 1.16 at daemon start; `doctor` check; explicit `k=60` |
| BM25 IDF interaction (client weights vs server `modifier: idf` double-counting) | Med | Med | Emit term-frequency sparse vectors and let qdrant compute IDF server-side; cover with a unit + eval test |
| Whisper Russian WER on noisy Telegram Opus is poor | Med | Med | Use large-v3 Russian fine-tune (6.39 WER) not base; medium only as speed fallback; transcripts are additive recall, never overwrite text |
| `whisper-rs` C build / model size complicates `sy apply` | Med | Med | Productize the C build + model fetch in the reproducible build; feature-gate transcription so the core indexer never blocks on it |
| Full re-index migration disrupts a live index | Med | High | Versioned schema; resumable idempotent re-index; run under existing throttle; status reporting |
| Date lexicon misfires and over-filters | Low | Med | Expansion only when caller gave no date filter; explicit args override; log misses |
| Calibration constants overfit a tiny eval set | Med | Med | Keep `abstain_threshold` a per-query param with a conservative server default; expand eval negatives over time |

## 7. Open Questions

- **qdrant client transport**: stay on REST (current) or adopt
  `qdrant-client` gRPC for the Query API? (Leaning: keep REST to avoid a
  new dep unless the builder ergonomics materially help.)
- **Sparse weighting**: emit raw term-frequency + qdrant `modifier: idf`
  (robust to incremental updates) vs precomputed BM25 weights from the
  `bm25` crate (full BM25 length-normalization but corpus-stat
  maintenance). Default recommendation: server-side IDF.
- **Transcription concurrency**: dedicated CPU pool vs reuse the index
  throttle — what bound keeps the laptop usable during a big backlog?
- **Context-window chunk**: ship the optional 5-message Telegram context
  chunk in this iteration or keep per-message only until eval shows a
  recall gap?
- **Russian Whisper weights distribution**: confirm license/size of
  `antony66/whisper-large-v3-russian` for inclusion in the model cache
  fetch.

## 8. Hand-off

- **Journey:** run `/journey` against this spec →
  `specs/journeys/JOURNEY-<dt>.md`
- **Roadmap:** run `/roadmap` against the journey →
  `specs/roadmaps/knowledge-retrieval-iter1/ROADMAP.md`
  (suggested order from the feedback: REQ-1 → REQ-2 → REQ-3 → REQ-4 →
  REQ-9 → REQ-6 → REQ-5 → REQ-7/8/10)
- **Implement:** `/implement` one roadmap step at a time
- **New NPU model:** none (transcription is CPU/iGPU; embed/rerank
  unchanged) — so **no `/npu-prep`** this iteration
- **New Workload:** transcription is a CPU/iGPU index-pass job, not an
  aiplane NPU workload — so **no `/workload`** unless we later decide to
  model it through the registry
