# ROADMAP: Spark agent launch

Source: [`specs/journeys/JOURNEY-20260825-0849.md`](../../journeys/JOURNEY-20260825-0849.md)

Research: [`specs/research/spark-agent-launch/SPEC.md`](../../research/spark-agent-launch/SPEC.md)

## Overview

Build a workstation-side `sy spark <host> launch` orchestrator for Codex,
Claude Code, and OpenCode. The final path resolves and readies one verified
Spark model through the existing pinned HTTPS control plane, gives the local
agent only an inference-scoped credential, and uses client-native transient or
launch-owned configuration without adding remote command execution.

Implementation note: the delivered code keeps the small resolver, state/token
transaction, remote orchestration, adapters, and child launcher together in
`src/spark/launch.rs`. This replaces the provisional submodule file split below
without changing its boundaries or Definitions of Done; focused unit sections
and the real-device gates exercise those boundaries directly.

## Step 1 — Land the CLI contract and pure launch resolution

**Goal:** Parse the complete Ollama-compatible command surface and resolve exact
models/instances deterministically without performing I/O.

**Files:**

- `src/spark/launch/mod.rs` (new) — launch request/plan types and orchestration boundary.
- `src/spark/launch/resolve.rs` (new) — exact model matching, ambiguity handling, healthy-instance preference, deterministic instance naming.
- `src/spark/cli.rs:39` (modified) — `Launch` variant, `LaunchArgs`, integration enum, flag/env/conflict grammar, dispatch hook.
- `src/spark/mod.rs` (modified) — module registration.

**Tests:**

- `src/spark/cli.rs::tests::launch_parses_ollama_compatible_surface` — integrations, model/config/restore/yes/dry-run/json and trailing args.
- `src/spark/cli.rs::tests::launch_rejects_invalid_flag_combinations` — restore/json/extra-arg conflicts.
- `src/spark/launch/resolve.rs::tests::*` — exact/absent/ambiguous model matches, saved/owned/other instance preference, ambiguity rejection, stable safe names.

**Definition of Done:**

- [x] All three integrations and `--` forwarding parse exactly as documented.
- [x] Flags override `SY_SPARK_*` environment values.
- [x] Pure resolution returns typed decisions and never contains bearer material.
- [x] Focused tests and `make lint` pass with no dead-code suppression.

**Risks / unknowns:** Clap trailing-var-arg behavior must keep every child token
byte-for-byte while still rejecting tokens placed before `--`.

## Step 2 — Add private launch state and inference-token reconciliation

**Goal:** Persist host-scoped selection metadata and a separate inference bearer
atomically, and decide when to reuse, revoke, or replace a token.

**Files:**

- `src/spark/launch/state.rs` (new) — schema, lock, mode checks, atomic state/credential transactions, ownership markers.
- `src/spark/launch/token.rs` (new) — inference-only request and reconciliation logic.
- `src/spark/client.rs:232` (modified) — narrowly expose validated host URL/CA and protected-write helpers without exposing the admin token.
- `src/spark/launch/mod.rs` (modified) — state/token integration.

**Tests:**

- `src/spark/launch/state.rs::tests::*` — v1 round-trip, malformed/schema rejection, 0600 enforcement, atomic replacement, secret absent from metadata.
- `src/spark/launch/token.rs::tests::*` — exact inference-only scope, reuse/revoke/create decisions, missing/mismatched bearer behavior.
- `src/spark/client.rs::tests::launch_material_exposes_identity_not_admin_secret` — URL/CA/credential-path projection cannot reveal bootstrap bearer content.

**Definition of Done:**

- [x] Launch metadata is host/integration scoped and contains only token ID.
- [x] Bearer file is separate, mode 0600, atomically installed, and redacted from all errors/debug output.
- [x] A launcher token has exactly `inference` scope and bounded concurrency.
- [x] Concurrent local writers serialize or converge without truncation.
- [x] Focused tests and `make lint` pass.

**Risks / unknowns:** A server token may exist after a local crash between remote
creation and local atomic commit; reconciliation must revoke a known orphan and
never fall back to the admin credential.

## Step 3 — Orchestrate model readiness through existing Spark operations

**Goal:** Reuse a healthy exact instance or start the deterministic launch-owned
instance through existing admission/serve/follow/revalidation calls.

**Files:**

- `src/spark/launch/remote.rs` (new) — model/instance reads, dry-run admission, serve/follow, post-start identity check, token API adapter.
- `src/spark/launch/mod.rs` (modified) — compose state, resolution, and remote readiness.
- `src/spark/client.rs:362` (modified only if a narrow helper is required) — reuse existing models/instances/admission/serve/token methods.
- `tests/spark_launch_e2e.rs` (new) — pinned-TLS fake control plane.

**Tests:**

- `tests/spark_launch_e2e.rs::reuses_healthy_exact_instance_without_mutation`.
- `tests/spark_launch_e2e.rs::cold_model_uses_admission_serve_follow_and_revalidation`.
- `tests/spark_launch_e2e.rs::dry_run_admits_without_token_state_or_child_side_effects`.
- `tests/spark_launch_e2e.rs::policy_tls_auth_and_stale_identity_fail_with_stable_codes`.

**Definition of Done:**

- [x] Healthy reuse performs no lifecycle mutation.
- [x] Cold readiness uses exact existing APIs and revalidates model ID/name/health/endpoint.
- [x] Missing and ambiguous models fail with immutable download remediation.
- [x] Dry-run performs no remote/local mutation.
- [x] Exit 3/4 mapping remains identical to other Spark commands.
- [x] Focused tests and `make lint` pass.

**Risks / unknowns:** The fixture must exercise durable SSE/poll completion
without duplicating `SparkClient` internals or weakening TLS pin checks.

## Step 4 — Implement Claude Code and OpenCode adapters

**Goal:** Generate exact secret-safe local argv/environment for Claude Code and
OpenCode and launch fake binaries with inherited terminal/cwd behavior.

**Files:**

- `src/spark/launch/adapter.rs` (new) — adapter trait, executable discovery, install-confirmation policy, common child plan.
- `src/spark/launch/claude.rs` (new) — Anthropic route, tier/subagent mappings, CA and nonessential-traffic settings.
- `src/spark/launch/opencode.rs` (new) — inline provider/model JSON and CA/credential environment.
- `src/spark/launch/process.rs` (new) — inherited I/O/cwd, secret-safe spawn, child status mapping.
- `src/spark/launch/mod.rs` (modified) — adapter dispatch.

**Tests:**

- `src/spark/launch/claude.rs::tests::*` — exact argv/env, inherited API-key removal, no bearer in debug/JSON.
- `src/spark/launch/opencode.rs::tests::*` — path-prefixed provider, selected model, env-backed key, no user config write.
- `src/spark/launch/process.rs::tests::*` — fake agent captures cwd/argv/allowed env and exit status; secret value never reaches process arguments/output.
- `src/spark/launch/adapter.rs::tests::*` — PATH/fallback discovery and interactive/headless installation policy.

**Definition of Done:**

- [x] Both adapters use only native client configuration mechanisms.
- [x] The agent receives only the inference credential and pinned CA.
- [x] Extra args remain exact argv tokens and never pass through a shell.
- [x] Numeric child exit status is propagated.
- [x] Existing user/project agent configs are unchanged.
- [x] Focused tests and `make lint` pass.

**Risks / unknowns:** OpenCode provider schema differs across major versions;
the installed version probe and golden config must fail clearly on unsupported
formats rather than guessing.

## Step 5 — Implement the Codex profile, catalog, conflicts, and restore

**Goal:** Launch Codex through a sy-owned Responses profile/catalog and safely
remove only those owned artifacts with `--restore`.

**Files:**

- `src/spark/launch/codex.rs` (new) — version check, provider profile, model catalog, defensive overrides, conflict checks, ownership validation, restore.
- `src/spark/launch/mod.rs` (modified) — Codex configure/launch/restore dispatch.
- `src/spark/launch/process.rs` (modified) — Codex child environment/status.

**Tests:**

- `src/spark/launch/codex.rs::tests::profile_uses_responses_path_and_env_key`.
- `src/spark/launch/codex.rs::tests::catalog_names_exact_model_and_capabilities`.
- `src/spark/launch/codex.rs::tests::managed_overrides_reject_profile_model_provider_and_catalog_conflicts`.
- `src/spark/launch/codex.rs::tests::restore_removes_only_owned_valid_files`.
- `src/spark/launch/codex.rs::tests::missing_or_old_codex_has_actionable_error`.

**Definition of Done:**

- [x] Main `~/.codex/config.toml` is never modified.
- [x] Profile/catalog writes are validated and atomic.
- [x] Bearer is env-only and absent from profile/catalog/argv/output.
- [x] Conflicting forwarded routing options fail before spawn.
- [x] Restore refuses unowned/drifted content and removes only owned files.
- [x] Focused tests and `make lint` pass.

**Risks / unknowns:** Codex's separate-profile discovery is versioned; defensive
inline overrides must preserve the selected owned profile without permitting a
forwarded override to escape Spark routing.

## Step 6 — Ship docs and verify the complete journey locally and on DGX Spark

**Goal:** Prove the installed command end to end for all three clients and leave
the repository, workstation, and Spark in a healthy durable state.

**Files:**

- `README.md:69` (modified) — launch examples and security behavior.
- `docs/how-to/serve-a-model-on-spark.md` (modified) — one-command agent journey and remediation.
- `docs/reference/spark.md` (modified) — full flags, state, credentials, restore, exits.
- `docs/reference/cli.md` (modified) — generated/manual CLI reference update.
- `specs/research/spark-agent-launch/SPEC.md` (modified if implementation evidence changes a decision).
- `specs/journeys/JOURNEY-20260825-0849.md` (modified) — tick evidence.
- `specs/roadmaps/spark-agent-launch/ROADMAP.md` (modified) — close all DoDs.

**Tests / verification:**

- `cargo test spark::launch --all-features` — all launch unit tests.
- `cargo test --test spark_launch_e2e --all-features` — black-box control/child path.
- `make lint` and one complete `make test`.
- Fresh release build/install to `~/.local/bin/sy` and checksum equality.
- Real `sy spark dgx-spark launch <integration> --model ornith-1.5:9b --dry-run --json` for each integration.
- Real bounded Codex Responses, Claude Messages, and OpenCode launch request against one healthy exact model.
- `sy spark dgx-spark ps --json`, `status --json`, and `doctor --json` remain healthy afterward.

**Definition of Done:**

- [x] Every journey acceptance criterion has automated or recorded real-device evidence.
- [x] All three installed local agent versions are compatible and launch through the pinned Spark routes.
- [x] Local `sy` is rebuilt and installed from the final source.
- [x] Spark model remains healthy; Docker restart, host reboot, and DGX software update are not run.
- [x] Docs match the actual CLI/help/exit behavior.
- [x] `make lint` and `make test` pass with zero warnings/flakes.

**Risks / unknowns:** A real coding agent can perform broad local work; device
verification must use a bounded prompt or version/config path and must not grant
additional permissions merely to prove routing.

## Cross-cutting Definition of Done

- [x] All step DoDs are satisfied.
- [x] `sy spark dgx-spark launch codex --model ornith-1.5:9b` opens Codex in the current project and sends a bounded Responses request through Spark.
- [x] Equivalent Claude Code and OpenCode launches use their native compatible routes.
- [x] No bootstrap/admin bearer appears in any child environment, argv, generated client file, log, JSON, or test artifact.
- [x] Missing model, admission rejection, TLS/auth failure, stale state, token drift, missing client, conflicting args, and non-zero child status match documented behavior.
- [x] `sy spark dgx-spark ps/status/doctor --json` are healthy after verification.
- [x] Full local lint/test gates are green and local installed `sy` matches the final release artifact.

## Out of Scope

- Arbitrary local integration names or any remote command execution.
- Implicit mutable Hugging Face downloads.
- Rewriting primary agent configuration.
- Ollama integrations beyond Codex, Claude Code, and OpenCode.
