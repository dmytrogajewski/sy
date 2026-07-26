# sy-knowledge — Iteration 1 Feedback & Requirements

## 1. Context

This document captures concrete feedback after a real failed retrieval session
against `mcp__sy-knowledge`, plus product/engineering requirements to address it.

### 1.1 The failing session

The user asked, in Russian:

> "Look in sy knowledge — at some point in chat with Anna Lu I asked her the
> name of a guy from X5 or Magnit, we met him over the New Year holidays."

This is a **specific-fact lookup** against a corpus that demonstrably contains
the relevant chat:

- Source: full Telegram export at `/home/dmitriy/knowledge/telegram/ChatExport_2026-03-24/`
  (~76 000 messages spanning 2020–2026, HTML + media + `result.json`).
- The user is sure the conversation happened around New Year 2024 (Dec 2023 –
  Jan 2024) and that he was the one asking Лу.

### 1.2 What the agent tried

1. `mcp__sy-knowledge__knowledge_search` with multiple Russian and English
   query variants ("Анна Лу X5 Магнит новый год", "как зовут чувака X5",
   "познакомились новый год пятёрочка Перекрёсток", etc.).
2. After the semantic results were unusable, fell back to:
   - direct `grep` over `messages*.html`,
   - a custom Python HTML parser that reconstructs `{date, from, text}` per
     message, then keyword + window filtering,
   - parsing of `result.json` (failed: the export's `result.json` is
     truncated/invalid JSON at ~25 MB).

The fallback parsing recovered the structure but still failed to surface a
match — likely because the actual ask happened in voice/`[media]` or just is
not in the export window. That is a separate data problem, but the **search
experience** itself failed before we got there, and that is what this doc is
about.

### 1.3 Observed failure modes of `knowledge_search`

1. **Index self-poisoning.** The MCP indexes
   `/home/dmitriy/.claude/projects/**/*.jsonl` (this agent's own transcripts).
   The user's *current* prompt was immediately the top hit on a search for
   that same prompt, with `embed_score ≈ 0.85`. Several of the top-N results
   were just the conversation we were having now, echoed back.
2. **Chunks too large.** The first `knowledge_search` call returned a single
   ~67 KB response that exceeded the harness output cap and had to be saved
   to disk. Individual chunks mix dozens of unrelated Telegram messages from
   different years. This makes both retrieval and downstream reasoning noisy.
3. **No useful metadata filter surface.** The schema only exposes `source`
   (path prefix). There is no way to say "only messages between 2023-12-15
   and 2024-02-15", "only `from:Лу`", "exclude `.claude/projects`",
   "only telegram". So a query like the one above has to encode every
   constraint in natural language and hope the embedding picks it up.
4. **Lexical entity matches not prioritized.** Pure dense retrieval +
   bge-reranker-v2-m3 surfaces chunks that are *thematically* close
   ("Лу + New Year + promises + gifts") but contain **zero** literal
   `X5`/`Магнит` mentions. The two terms that should have been the strongest
   signal in the query are effectively ignored. Standard hybrid retrieval
   (BM25 / sparse + dense + RRF) would catch this; the model already
   (BGE-M3) supports sparse vectors natively.
5. **No abstain / no confidence calibration.** Top results came back with
   high absolute scores (0.83–0.85) and ~uniform spread across top-8. There
   is no signal to the caller that "this is just background similarity, not
   a real hit". Today the tool always returns top-k and looks confident.
6. **Voice / media not transcribed.** The Telegram export contains thousands
   of `[media]` placeholders for voice notes and video messages. None of
   them are indexed. For a chat-heavy corpus this is a large recall gap —
   the message the user is looking for *may literally be a voice message*.
7. **No domain expansion.** "X5" should expand to `{X5, Пятёрочка,
   Перекрёсток, Чижик}`; "Магнит" → `{Магнит, Тандер}`; "НГ 2024" →
   date range. None of this happens today.

### 1.4 Why this is a product issue, not a model issue

All six failure modes above can be fixed without changing the embedding
model or the reranker. They are about **what we index, how we chunk, what
filters we expose, and how we combine signals**.

---

## 2. Requirements

Requirements are written so each one is independently shippable.
Priority is set against the failing session above.

### REQ-1 — Exclude / segregate Claude Code transcripts from default scope
**Priority: P0. Effort: S.**

- The indexer MUST treat `~/.claude/projects/**` (and any other agent
  transcript locations) as a **separate, named source**, e.g.
  `claude-transcripts`.
- `knowledge_search` MUST NOT include this source in default results unless
  the caller passes it explicitly via `source` or a new `include_sources`
  parameter.
- Rationale: today the user's live prompt becomes the top hit on itself,
  which is broken behavior and pollutes every other query.
- Acceptance: a fresh prompt asking about "X" never returns the same
  prompt's own embedding in the top-k of the default scope.

### REQ-2 — First-class metadata filters
**Priority: P0. Effort: M.**

`knowledge_search` MUST accept structured filters in addition to `query`:

- `date_from`, `date_to` (ISO-8601) — applied to source-extracted timestamps.
- `from` / `author` — exact or set membership against extracted sender.
- `kind` — enum of source kinds: `telegram`, `claude-transcripts`,
  `email`, `slack`, `notes`, `code`, etc.
- `include_sources`, `exclude_sources` — list of registered source names.

These MUST be applied as qdrant payload filters *before* reranking, not as
post-filters, so the candidate pool is not blown by irrelevant items.

The indexer is responsible for populating the payload fields per source
(see REQ-4).

Acceptance: a query like
`{query: "новый год X5", date_from: "2023-12-01", date_to: "2024-02-29",
kind: "telegram"}` returns only telegram messages from that window.

### REQ-3 — Hybrid retrieval (sparse + dense + RRF)
**Priority: P0. Effort: M.**

- Switch retrieval to **hybrid**: dense (current BGE embeddings) +
  sparse (BM25 or BGE-M3's native sparse vectors).
- Combine via Reciprocal Rank Fusion before reranking.
- Keep cross-encoder rerank as the final stage, but with a smaller and
  better-grounded candidate set.
- Rationale: named entities ("X5", file paths, SHAs, person names) are
  precisely where dense retrieval is weakest and BM25 is strongest. This
  was the dominant failure mode in the session.
- Acceptance: a query containing a rare literal token present in exactly
  one indexed chunk MUST return that chunk in top-3.

### REQ-4 — Per-source preprocessing & chunking
**Priority: P1. Effort: M.**

Replace the current generic chunker with **per-source pipelines**. Each
pipeline emits structured records with payload metadata, not just blobs.

Minimum viable set:

- **Telegram (HTML export)** — one chunk per *message*, with payload
  `{date, from, file, message_id, reply_to_id, has_media}`. Optionally
  also emit a sliding 5-message context window as a secondary chunk type,
  but keep the per-message granularity as primary.
- **Telegram (`result.json`)** — same as HTML pipeline, used as canonical
  source when JSON is valid; fall back to HTML otherwise. (Note: current
  `result.json` for this user is invalid JSON past ~25 MB; the pipeline
  MUST tolerate this and not abort the whole index pass.)
- **Claude Code transcripts (`.jsonl`)** — one chunk per turn, with
  payload `{role, model, project_id, ts}`. Excluded from default scope
  per REQ-1.
- **Generic markdown / text** — keep current chunker but with a smaller
  target size (~500–800 tokens).

Acceptance: parsing a known Telegram export produces ≥ N chunks where
N = message count (± small delta for edge cases), each with a populated
`date` and `from`.

### REQ-5 — Voice / video transcription
**Priority: P1. Effort: M.**

- The Telegram pipeline MUST detect `voice_messages/`, `round_video_messages/`
  and other media references.
- Transcribe with Whisper (large-v3 or distil-large-v3 for speed) on the
  available NPU/CPU. Cache transcripts next to the source file.
- Emit the transcript as a chunk with payload pointing at the original
  media file, `kind: telegram-voice` (so it can be filtered/upweighted
  separately if needed).
- Acceptance: after one incremental index pass on a chat with N voice
  notes, ≥ 95% of them are transcribed and searchable.

### REQ-6 — Confidence calibration & abstain
**Priority: P1. Effort: S.**

`knowledge_search` MUST:

- Return a `confidence` field in addition to per-result scores. Confidence
  is computed from the top-1 reranker score AND the top-1/top-k spread
  (large spread → confident; flat distribution → background noise).
- Support an `abstain_threshold` parameter. If confidence is below it,
  return an empty result set with `reason: "no high-confidence match"`
  rather than top-k of noise.
- Default behavior: return top-k but include `confidence` so callers can
  decide. Calibration constants tuned against a small held-out set of
  positive/negative queries (see REQ-9).

Acceptance: on a query whose answer is provably not in the index, the
tool returns `confidence < threshold` and the agent can stop searching
instead of confidently quoting noise.

### REQ-7 — Domain / synonym expansion
**Priority: P2. Effort: S.**

- Maintain a small, user-editable synonyms file
  (`~/.config/sy-knowledge/synonyms.yaml`) of the form:
  ```yaml
  - canonical: X5
    aliases: [Пятёрочка, Перекрёсток, Чижик, X5 Group]
  - canonical: Магнит
    aliases: [Тандер]
  ```
- At query time, expand the query by OR-ing aliases into the sparse-side
  query (not the dense side — embeddings already handle synonymy
  reasonably). This keeps dense recall but adds precise BM25 matches.
- Acceptance: a query "X5" hits chunks that mention only "Перекрёсток".

### REQ-8 — Smarter date / time-expression handling
**Priority: P2. Effort: S.**

- The query parser SHOULD detect natural-language time expressions
  ("новый год 2024", "прошлым летом", "in January") and translate them
  into `date_from`/`date_to` filters automatically, with the option to
  override.
- Russian / English at minimum.
- Acceptance: "новогодние праздники 2024" auto-applies
  `date_from=2023-12-28, date_to=2024-01-10`.

### REQ-9 — Eval harness with a small labelled set
**Priority: P1. Effort: S.**

- Create `~/sources/sy/specs/knowledge-feedback-iter1/eval/` with a
  hand-curated set of 20–40 `(query, expected_chunk_id_or_substring)`
  pairs, drawn from real failing sessions like this one.
- Add a `make eval` (or equivalent) that runs the suite against the live
  index and reports recall@1, recall@5, MRR, and abstain accuracy.
- The eval set MUST include at least:
  - 5 named-entity lookups (REQ-3, REQ-7)
  - 5 date-range lookups (REQ-2, REQ-8)
  - 5 abstain cases (true negatives) (REQ-6)
  - 5 cross-source disambiguation cases (REQ-1, REQ-2 `kind`)
- Acceptance: eval runs in CI / pre-commit and regressions are visible.

### REQ-10 — Response size hygiene
**Priority: P2. Effort: S.**

- `knowledge_search` MUST cap each chunk's returned text to N characters
  (configurable, default ~1500) and total response size to fit the
  MCP/harness output budget without spillover to disk.
- If truncation occurs, MUST surface `truncated: true` and a stable
  chunk id callers can re-fetch via a (new) `knowledge_get_chunk(id)`
  endpoint.
- Rationale: a single 67 KB response on the first call of this session
  forced disk spillover and made the agent juggle file slices.

---

## 3. Suggested sequencing

The two highest-leverage changes against the observed failure are
**REQ-1** and **REQ-3** — they alone would very likely have surfaced the
right answer in this session (assuming the message itself is in the
index). REQ-2 is foundational for everything else.

Proposed order:

1. **REQ-1** — segregate Claude transcripts. Stops self-poisoning.
2. **REQ-2** — payload filters. Foundation for everything else.
3. **REQ-3** — hybrid retrieval. Fixes the dominant failure mode.
4. **REQ-4** — per-source chunking. Makes REQ-2/REQ-3 actually useful.
5. **REQ-9** — eval harness. Locks the gains in.
6. **REQ-6** — calibration / abstain. Stops "confident noise".
7. **REQ-5** — voice transcription. Closes the recall gap on chat sources.
8. **REQ-7, REQ-8, REQ-10** — polish.

REQ-1 → REQ-3 should be ~one focused iteration. REQ-4 → REQ-5 can be
parallelized once REQ-2 lands.

---

## 4. Non-goals (for this iteration)

- Swapping the embedding model.
- Swapping the reranker.
- Building a UI / dashboard.
- Cross-document entity graph / knowledge graph layer (interesting but
  out of scope; revisit once REQ-1..REQ-6 ship).
- Multi-tenant / sharing.

---

## 5. Open questions

- Where exactly does the indexer config live today, and what is the
  current source registration interface? (Need to confirm before
  designing REQ-1 / REQ-2 surface.)
- Is `result.json` (Telegram) intended as the canonical source, given
  that real-world exports can produce invalid JSON past a certain size?
  Recommend HTML as canonical with JSON as fallback (REQ-4).
- Whisper model size vs. NPU memory budget for REQ-5 — needs a quick
  benchmark on the target hardware.
- Should `abstain_threshold` (REQ-6) be a tool parameter or a
  server-side config? Leaning tool parameter so callers (this agent
  among them) can choose strict vs. permissive per query.
