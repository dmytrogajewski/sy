# ROADMAP: spark-gguf-engine-catalog

Source: `specs/journeys/JOURNEY-20260826-2220.md` and `specs/research/spark-recommended-model-checkpoints/SPEC.md`.

## Overview

Replace the single vLLM policy with declarative model and engine catalogs, add selective immutable GGUF acquisition, and make the existing protocol-neutral gateway fully compatible with llama.cpp. The end state serves configured Q4_K_XL/Q4_K_M artifacts through a signed CUDA llama.cpp image by default, retains an explicitly selectable vLLM FP8 engine, contains no model-specific Rust logic, and is proven end to end on the real `dgx-spark` without changing its platform stack.

## Step 1 — Define artifact and catalog wire contracts

**Goal:** Add strict wire documents for an exact model artifact set and expose them on download plans/models without changing command semantics.

**Files:**
- `src/spark/wire.rs` (modified) — versioned artifact format, primary file, auxiliary files, quantization, capabilities, and optional configured alias provenance.
- `src/spark/state.rs` (modified) — persisted model JSON requires an explicit artifact state; new documents round-trip exactly.

**Tests:**
- `src/spark/wire.rs::tests::model_artifacts_round_trip_without_losing_exact_files`.
- `src/spark/wire.rs::tests::model_document_requires_explicit_artifact_state`.
- `src/spark/state.rs::tests::stale_model_metadata_without_artifacts_is_rejected`.

**Definition of Done:**
- [x] The wire schema represents GGUF and safetensors without model-family enums.
- [x] Stale state without an explicit artifact identity is rejected.
- [x] Unknown artifact fields fail closed at catalog boundaries.
- [x] `make lint && make test` green.

**Risks / unknowns:** Stored JSON is part of crash recovery, so incompatible records fail explicitly and are redeployed rather than guessed.

## Step 2 — Load a declarative model catalog

**Goal:** Resolve friendly aliases to immutable repository revisions and exact downloadable files through configuration, while allowing arbitrary repositories with explicit artifact arguments.

**Files:**
- `src/spark/model_catalog.rs` (new) — strict `sy.spark.models/v2` parser, alias uniqueness, immutable revision, artifact path, typed auxiliary role, size, and capability validation.
- `src/spark/mod.rs` (modified) — register the catalog module.
- `configs/sy/spark/models.toml` (new) — Qwen3.8-27B, Muse Glimmer 30B, and Ornith 1.5 35B recommended aliases plus FP8 alternatives.

**Tests:**
- `src/spark/model_catalog.rs::tests::recommended_catalog_resolves_exact_immutable_artifacts`.
- `src/spark/model_catalog.rs::tests::duplicate_alias_or_mutable_revision_is_rejected`.
- `src/spark/model_catalog.rs::tests::model_names_do_not_affect_resolution`.

**Definition of Done:**
- [x] All recommended identities in the research spec are configuration, not Rust constants.
- [x] Aliases resolve deterministically; duplicate aliases and mutable revisions are rejected.
- [x] Catalog parsing denies unknown fields and path traversal.
- [x] `make lint && make test` green.

**Risks / unknowns:** Hub revisions and filenames are case-sensitive; configuration validation must preserve exact spelling.

## Step 3 — Acquire only selected immutable artifacts

**Goal:** Extend download planning and transport to fetch the exact configured files plus required metadata instead of every quantization in a GGUF repository.

**Files:**
- `src/spark/cli.rs` (modified) — accept configured aliases and generic `--artifact`/`--auxiliary` overrides with complete help/env support.
- `src/spark/wire.rs` (modified) — carry optional artifact selectors in download requests/plans.
- `src/spark/model.rs` (modified) — filter the resolved Hub tree, selectively download, independently verify, persist the descriptor, and retain resumability/fallback.
- `src/spark/agent.rs` (modified) — resolve model catalog entries before admission and acquisition.

**Tests:**
- `src/spark/model.rs::tests::plan_selects_primary_auxiliary_and_required_metadata_only`.
- `src/spark/model.rs::tests::missing_or_ambiguous_artifact_fails_before_download`.
- `src/spark/cli.rs::tests::download_artifact_flags_are_agent_friendly`.
- `src/spark/agent.rs::tests::configured_alias_download_persists_exact_artifact_identity`.

**Definition of Done:**
- [x] A configured Q4 alias does not download other GGUF quantizations.
- [x] Arbitrary repositories remain supported through explicit generic selectors.
- [x] Missing files, unsafe paths, size/hash mismatch, and ambiguity fail closed.
- [x] Interrupted downloads remain resumable through the existing cache.
- [x] `make lint && make test` green.

**Risks / unknowns:** `hf-hub` selective APIs may require per-file downloads; progress and independent snapshot verification must retain their current guarantees.

## Step 4 — Generalize engine policy into a declarative catalog

**Goal:** Select one engine from artifact traits and priority, with all image, arguments, parser modes, resource limits, and placeholders owned by configuration.

**Files:**
- `src/spark/engine.rs` (modified) — parse multiple strict engine declarations, match generic artifact/model metadata, reject ties, and bind validated artifact placeholders by role.
- `configs/sy/spark/engines/llama-cpp.toml` (new) — digest-pinned CUDA llama.cpp profile, build/version floor, GGUF arguments, health/semantic probes, and Spark tuning.
- `configs/sy/spark/engines/vllm.toml` (new) — move the existing proven FP8 policy unchanged into the catalog.
- `configs/sy/spark/engine.toml` (removed after migration) — eliminate the single-policy boundary.

**Tests:**
- `src/spark/engine.rs::tests::gguf_selects_llama_and_safetensors_selects_vllm_from_config`.
- `src/spark/engine.rs::tests::equal_priority_match_is_rejected_instead_of_guessed`.
- `src/spark/engine.rs::tests::artifact_placeholders_are_confined_and_exact`.

**Definition of Done:**
- [x] Engine selection uses declared artifact traits only; no repository/model-name branches exist.
- [x] Q4 uses llama.cpp and configured FP8 uses vLLM.
- [x] Unknown placeholders, ambiguous matches, and unsupported formats fail at config load/admission.
- [x] Projectors and weight shards remain distinct; only explicitly bound roles produce engine arguments.
- [x] Existing vLLM behavior is preserved by its migrated config.
- [x] `make lint && make test` green.

**Risks / unknowns:** Existing instance records name one engine ID; migration must keep that identity stable or produce an explicit upgrade action.

## Step 5 — Execute artifact-aware engine specifications

**Goal:** Make serve admission and executor launch the selected engine with exact read-only artifact paths and configuration-derived identity.

**Files:**
- `src/spark/executor.rs` (modified) — replace the single policy with the catalog, expand primary/projector placeholders, mount verified paths, and label engine/config/artifact fingerprints.
- `src/spark/agent.rs` (modified) — use selected engine resources/routes for admission, persistence, reconciliation, and restart.
- `src/spark/resources.rs` (modified) — report artifact-aware resource decisions without recipe terminology on the public wire.
- `src/spark/state.rs` (modified) — preserve exact engine and artifact identity across restart.

**Tests:**
- `src/spark/executor.rs::tests::gguf_spec_mounts_only_verified_artifacts_read_only`.
- `src/spark/executor.rs::tests::container_identity_changes_when_engine_or_artifact_config_changes`.
- `src/spark/agent.rs::tests::reconcile_restarts_the_same_engine_and_artifact_generation`.
- `src/spark/agent.rs::tests::dry_run_reports_engine_artifact_and_reserve_without_side_effects`.

**Definition of Done:**
- [x] Dry-run truthfully reports exact engine/artifact selection.
- [x] Container command and mounts come entirely from validated configuration.
- [x] Existing reserve, emergency floor, confinement, and route publication boundaries remain intact.
- [x] Restart reconciliation cannot silently change engine/artifact identity.
- [x] `make lint && make test` green.

**Risks / unknowns:** llama.cpp accepts a file path while vLLM accepts a snapshot directory; this difference must be expressed by placeholders/config, not an engine-name branch.

## Step 6 — Complete llama.cpp protocol compatibility

**Goal:** Accept llama.cpp's streamed OpenAI output at the engine boundary and preserve complete OpenAI/Anthropic reasoning, usage, cancellation, images, and tool semantics at the public gateway.

**Files:**
- `src/spark/upstream.rs` (modified) — normalize configured upstream field mappings/event shapes into existing protocol-neutral generation events.
- `src/spark/gateway.rs` (modified) — preserve reasoning/text/tool ordering and finish/usage semantics for both APIs.
- `src/spark/probe.rs` (modified) — require streamed chat and tool probes before route publication for advertised capabilities.
- `tests/spark_gateway.rs` (modified) — black-box llama.cpp-shaped SSE fixtures through real HTTP routes.

**Tests:**
- `src/spark/upstream.rs::tests::llama_cpp_stream_maps_reasoning_text_tools_usage_and_done`.
- `tests/spark_gateway.rs::openai_stream_from_llama_fixture_is_protocol_complete`.
- `tests/spark_gateway.rs::anthropic_stream_from_llama_fixture_is_protocol_complete`.
- `tests/spark_gateway.rs::client_cancellation_closes_the_llama_upstream`.

**Definition of Done:**
- [x] OpenAI chat/completions and Anthropic messages stream valid ordered events.
- [x] Reasoning, text, parallel tool deltas, usage, finish reasons, errors, and cancellation are covered.
- [x] Image capability is published only when projector-backed E2E passes.
- [x] No route publishes after an incomplete semantic/protocol probe.
- [x] `make lint && make test` green.

**Risks / unknowns:** llama.cpp event fields can evolve between builds; pin the image and keep any mapping declarative where the wire permits.

## Step 7 — Productize installation, confinement, and upgrade

**Goal:** Install and deterministically replace the model/engine catalogs and signed llama.cpp image boundary without updating protected DGX platform components.

**Files:**
- `src/spark/install.rs` (modified) — manifest catalogs/directories, strict validation, hashes, ownership, AppArmor, systemd read-only paths, and authoritative upgrade replacement.
- `configs/systemd/system/sy-spark-agent.service` (modified) — read-only catalog access.
- `configs/systemd/system/sy-spark-executor.service` (modified) — read-only engine catalog/model cache access.
- `configs/apparmor.d/sy-spark-agent` (modified) — catalog read rules.
- `configs/apparmor.d/sy-spark-executor` (modified) — engine catalog and artifact read rules.

**Tests:**
- `src/spark/install.rs::tests::manifest_installs_both_catalogs_with_expected_hashes`.
- `src/spark/install.rs::tests::upgrade_replaces_shipped_catalogs_without_legacy_merge`.
- `src/spark/install.rs::tests::upgrade_rejects_stale_schema_and_unavailable_engine_identity`.
- `src/spark/install.rs::tests::confinement_allows_catalog_reads_but_not_writes`.

**Definition of Done:**
- [x] Clean install and upgrade materialize every required declaration with correct ownership/mode.
- [x] Config edits do not require recompiling Rust; invalid edits stop at validation.
- [x] Signed image/digest verification remains mandatory.
- [x] No OS, driver, CUDA platform, firmware, or appliance package update command is introduced.
- [x] `make lint && make test` green.

**Risks / unknowns:** AppArmor path changes can block startup only on the target; real deployment is required after fixture tests. Deprecated schemas and unavailable active engine identities fail explicitly and are redeployed in Step 9; there is no compatibility reader or catalog merge.

## Step 8 — Ship the operator surface and documentation

**Goal:** Make configuration and observed runtime identity understandable through CLI/JSON and document adding future models without code changes.

**Files:**
- `src/spark/cli.rs` (modified) — render artifact/engine details in download dry-run, `ls`, `ps`, and `logs` while retaining JSON compatibility.
- `README.md` (modified) — model alias, arbitrary artifact, serve/stop/launch examples and config extension guide.
- `specs/research/spark-recommended-model-checkpoints/SPEC.md` (modified) — record final engine image, configuration schema, and measured model results.
- `specs/journeys/JOURNEY-20260826-2220.md` (modified) — check verified acceptance criteria.

**Tests:**
- `src/spark/cli.rs::tests::model_and_process_render_engine_artifact_identity`.
- `src/spark/cli.rs::tests::json_documents_remain_machine_readable`.
- Documentation command examples are exercised with `--dry-run` against the test agent.

**Definition of Done:**
- [x] A user can add a compatible model by editing config, with no Rust patch.
- [x] Human and JSON output expose the immutable artifact and engine fingerprints.
- [x] All command help includes examples, `--json`, `--dry-run`, and `SY_*` equivalents where applicable.
- [x] `make lint && make test` green.

**Risks / unknowns:** Avoid exposing secrets or host cache paths while adding operational detail.

## Step 9 — Deploy and verify all three models on the real Spark

**Goal:** Build/install the signed aarch64 release and engine image, then prove the complete journey for every recommended artifact on `dgx-spark` without platform changes.

**Files:**
- `specs/runs/spark-gguf-engine-catalog.md` (new) — commands, immutable fingerprints, model metrics, protocol evidence, and before/after protected-platform hashes.
- `specs/research/spark-recommended-model-checkpoints/SPEC.md` (modified) — measured TTFT/decode/startup/memory and any config-only tuning changes.
- `configs/sy/spark/models.toml` (modified only if evidence corrects metadata).
- `configs/sy/spark/engines/llama-cpp.toml` (modified only for evidence-backed generic Spark tuning).

**Tests:**
- Real `sy spark dgx-spark download|serve|ps|logs|stop` for Qwen3.8-27B, Muse Glimmer 30B, and Ornith 1.5 35B.
- Real streamed OpenAI and Anthropic text/reasoning/tool requests for each model; image request for models whose projector capability is declared.
- Real headless `launch claude` and `launch opencode`; Codex path verified through the OpenAI protocol suite or real client when credentials permit.
- Stop/restart and agent restart preserve exact generation identity and health.

**Definition of Done:**
- [x] Signed release and digest-pinned llama.cpp image are installed on `dgx-spark`.
- [x] All three exact GGUF artifacts complete download, cold serve, protocol/tool turn, stop, and restart.
- [x] Claude and OpenCode launches work with streaming against the configured model route.
- [x] TTFT, decode rate, startup peak, steady memory, and fingerprints are recorded.
- [x] Protected platform fingerprint is byte-for-byte unchanged.
- [x] Local `sy` is updated to the same verified release.
- [x] Final `make lint && make test` green.

**Risks / unknowns:** Downloads total roughly 58 GiB including projectors; verification is sequential to preserve memory and disk reserve. Real Codex launch can depend on external client authentication, but its full API path remains mandatory.

## Cross-cutting Definition of Done

- [x] Every step DoD and journey acceptance criterion is satisfied.
- [x] `sy spark dgx-spark download <alias>`, `serve`, `ps`, `logs`, `stop`, and `launch` work from the locally installed binary.
- [x] OpenAI and Anthropic compatibility includes streaming reasoning, text, tools, usage, finish, errors, and cancellation.
- [x] The engine/model selection system contains no model-specific Rust branches or compiled operational catalogs.
- [x] A clean signed deploy changes no protected DGX platform component.
- [x] `make lint && make test` are green.

## Out of Scope

- Stress/soak gates, MTP/DFlash, one-million-token context, and automatic silent engine fallback.
- Updating the DGX OS, kernel, NVIDIA driver, CUDA platform, firmware, or appliance stack.
- Reintroducing per-model recipe files or hardcoded model policy in the binary.
