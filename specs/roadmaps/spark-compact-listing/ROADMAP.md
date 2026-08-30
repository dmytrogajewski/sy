# ROADMAP: Spark compact model and process listings

Source: `specs/journeys/JOURNEY-20260828-2221.md`

## Overview
Replace the verbose default Spark inventory renderers with aligned Ollama-style tables. `ls` remains the installed model inventory, `ps` becomes an active-process view, and detailed provenance stays available through `show`, `logs`, and stable JSON schemas.

## Step 1 — Render compact inventory tables and active processes
**Goal:** Make the two everyday read commands concise without weakening diagnostic or machine-readable surfaces.
**Files:** `src/spark/cli.rs` (modified), `README.md` (modified), `docs/reference/spark.md` (modified), `docs/reference/cli.md` (modified)
**Architecture:** The client filters the existing `InstanceListDocument` by observed lifecycle state before either human or JSON rendering. Private render helpers project existing wire documents into terminal rows; no control-plane endpoint, persisted state, or engine configuration changes.
**Main delivered CJM:** `sy spark dgx-spark ls` shows one row per installed model; `sy spark dgx-spark ps` shows one row per active instance; `show`, `logs`, and `--json` provide deeper inspection.
**Tests:**
- `src/spark/cli.rs::tests::model_list_is_one_compact_ollama_style_table` — verifies concise columns, alias/repository fallback, sizes, and absence of provenance noise.
- `src/spark/cli.rs::tests::process_list_contains_only_active_lifecycle_instances` — verifies active-state filtering and compact state/context rows.
- `src/spark/cli.rs::tests::empty_model_and_process_lists_render_headers_only` — verifies deterministic empty output.
- `src/spark/cli.rs::tests::model_and_process_render_engine_artifact_identity` — preserves engine and artifact provenance outside the compact listings.
**Definition of Done:**
- [x] Tests above pass.
- [x] Human `ls` and `ps` are compact aligned tables.
- [x] `ps --json` excludes absent and failed instances without changing its schema.
- [x] `show`, `logs`, and `ls --json` retain detailed identity and inventory data.
- [x] Real `sy spark dgx-spark ls` and `ps` succeed with the freshly built local binary.
- [x] `make lint` and two consecutive `make test` runs are green.
- [x] README and Spark CLI reference describe the command split.
- [x] No dead code, lint suppression, wire change, or host configuration change is introduced.
**Risks / unknowns:** Existing Spark agents can contain historical instances and alias-less models; the renderer must remain useful for both without mutating that state.

## Cross-cutting Definition of Done
- [x] All step DoDs satisfied.
- [x] End-to-end journey works against `dgx-spark`: `sy spark dgx-spark ls`, `sy spark dgx-spark ps`, and their JSON variants.
- [x] Detailed model and log inspection remains available.

## Out of Scope
- Control-plane schema changes, state deletion, and new CLI filtering flags are excluded because the existing documents already contain everything required for the corrected default views.
