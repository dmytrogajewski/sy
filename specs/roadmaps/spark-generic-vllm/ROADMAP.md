# ROADMAP: Spark generic vLLM serving

Source: `specs/journeys/JOURNEY-20260825-2251.md`

## Overview

Replace model-specific serving recipes with one declarative vLLM engine configuration. Downloads remain immutable and generic, while the executor converts a verified model record plus the matching declarative architecture profile into a locked container specification. Existing recipe-backed generations retain a read/stop-only migration path until the real Spark cutover succeeds.

## Step 1 — Define the generic engine configuration contract

**Goal:** Represent every operational vLLM value separately from compiled Rust and select finite architecture profiles from model metadata.

**Files:** `src/spark/wire.rs`, `src/spark/engine.rs` (new), `src/spark/mod.rs`, `configs/sy/spark/engine.toml` (new)

**Tests:**
- `src/spark/engine.rs::tests::shipped_configuration_is_valid` — verifies the installed document is complete and internally valid.
- `src/spark/engine.rs::tests::operational_values_live_only_in_configuration` — parses the real configuration and scans the production engine/executor/gateway/agent sources for duplicated image, arguments, environment, paths, resources, model policy, or sampling values.
- `src/spark/wire.rs::tests::serve_request_defaults_to_generic_engine` — preserves JSON compatibility while removing recipe selection from new requests.

**Definition of Done:**
- [x] Engine configuration and architecture profiles are independent of repository/commit.
- [x] No public request can select an image, executable, mount, network, or arbitrary argv.
- [x] A regression test prevents operational configuration from being embedded in the binary again.
- [x] Legacy recipe identities are rejected for every new prepare/start request; they are migration read/stop data only.

**Risks / unknowns:** Existing state contains recipe fields and needs an explicit migration-compatible representation.

## Step 2 — Construct a locked vLLM container from any verified model

**Goal:** Make the executor construct one bounded engine command from the installed configuration and exact cached snapshot.

**Files:** `src/spark/executor.rs`, `src/spark/engine.rs`, `src/spark/resources.rs`

**Tests:**
- `src/spark/executor.rs::tests::generic_spec_accepts_unlisted_verified_repository` — serves a repository absent from embedded recipes.
- `src/spark/executor.rs::tests::generic_spec_owns_all_security_sensitive_fields` — asserts image, digest, entrypoint, mounts, network, UID, capabilities, and resource limits come from the trusted configuration.
- `src/spark/executor.rs::tests::generic_spec_selects_profile_from_model_type` — asserts model metadata selects only a declared architecture profile.

**Definition of Done:**
- [x] Container construction no longer requires repository/commit compatibility with a recipe.
- [x] The model mount resolves only beneath the verified native Hugging Face cache.
- [x] Existing executor reconciliation tests remain green.

**Risks / unknowns:** vLLM task/parser combinations fail at runtime for incompatible model architectures and must surface as ordinary startup failures.

## Step 3 — Route admission and lifecycle through the generic engine

**Goal:** Remove recipe selection, compatibility ranking, and unverified overrides from all new serve operations.

**Files:** `src/spark/agent.rs`, `src/spark/cli.rs`, `src/spark/client.rs`, `src/spark/launch.rs`

**Tests:**
- `src/spark/agent.rs::tests::serve_admits_downloaded_model_without_recipe` — exercises the HTTP admission/start path for an unlisted model.
- `src/spark/agent.rs::tests::serve_rejects_missing_or_unverified_snapshot` — retains provenance enforcement.
- `src/spark/cli.rs::tests::serve_surface_has_no_recipe_or_unverified_flags` — locks the Ollama-like public command.
- `src/spark/launch.rs::tests::cold_launch_uses_generic_serve_request` — keeps all three launch adapters on the same lifecycle.

**Definition of Done:**
- [x] `sy spark <host> serve <model>` needs no recipe or unsafe override.
- [x] HTTP client/agent round trips carry model identity only; profiles are resolved on Spark.
- [x] Existing stable exit codes and operation following remain intact.

**Risks / unknowns:** Removing legacy routes immediately would prevent inspection of an instance started by the prior binary.

## Step 4 — Preserve state safely while retiring recipe-only control surfaces

**Goal:** Keep legacy generations observable/stoppable during cutover, while removing recipe/benchmark/tuning from the normal user path.

**Files:** `src/spark/state.rs`, `src/spark/wire.rs`, `src/spark/agent.rs`, `src/spark/cli.rs`, `src/spark/executor.rs`

**Tests:**
- `src/spark/state.rs::tests::legacy_instance_decodes_and_round_trips` — proves upgrade compatibility.
- `src/spark/executor.rs::tests::legacy_running_generation_can_be_stopped` — proves migration does not strand a container.
- `src/spark/cli.rs::tests::help_describes_engine_and_model_not_recipes` — removes obsolete public semantics.

**Definition of Done:**
- [x] Legacy instance state remains readable until cutover.
- [x] New state records engine identity and model identity separately.
- [x] Recipe selection/ranking does not participate in new serve or launch requests.

**Risks / unknowns:** SQLite schema evolution must remain transactional and readable by the upgraded service.

## Step 5 — Install, cut over, and verify on the real Spark

**Goal:** Upgrade only sy-managed components, replace the healthy Ornith generation with vLLM 0.27.1, and verify the complete journey.

**Files:** `src/spark/install.rs`, `configs/sy/spark/engine.toml`, `README.md`, `docs/how-to/serve-a-model-on-spark.md`, `docs/reference/spark.md`, `specs/roadmaps/spark-generic-vllm/ROADMAP.md`

**Tests:**
- Installer unit/golden tests — verify the engine policy is installed atomically with correct ownership/mode.
- Real `dgx-spark` command sequence from the journey — verifies download/serve/ls/ps/show/logs/stop/re-serve and protocol streaming.
- Strict all-target clippy and the complete Spark-enabled test suite — verifies the implementation once without redundant or stress checks.

**Definition of Done:**
- [x] Local `sy` and both remote sy services run the new binary/configuration.
- [x] Ornith is healthy under the digest-pinned vLLM 0.27.1 ARM64 image.
- [x] OpenAI Chat/Responses, Anthropic Messages, and one launched client stream successfully.
- [x] No protected DGX platform component is updated.
- [x] Documentation and every roadmap checkbox reflect observed reality.

**Risks / unknowns:** The first image pull and model initialization are long but bounded; failure must preserve enough logs for diagnosis and permit restoring the preceding healthy generation.

## Cross-cutting Definition of Done

- [x] All step definitions of done are satisfied.
- [x] A repository absent from source/config serves through `sy spark dgx-spark serve <alias>`.
- [x] Security tests prove the HTTP API cannot become arbitrary Docker or command execution.
- [x] The existing Ornith-backed Codex/Claude/OpenCode path remains protocol-compatible and streaming.
- [x] Strict all-target clippy and the complete Spark-enabled test suite pass.
- [x] No stress/load testing or protected DGX platform update is performed.

## Out of Scope

- Alternative inference engines and automatic engine fallback.
- Model quality ranking or benchmark recipes.
- Arbitrary container/runtime customization.
- DGX OS, driver, firmware, CUDA, Docker daemon, or kernel changes.
