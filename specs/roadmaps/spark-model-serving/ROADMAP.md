# ROADMAP: spark-model-serving — secure DGX Spark model appliance

Source: `specs/research/spark-model-serving/SPEC.md` (accepted decisions
D01–D12, technical design, user-journey sketch, and real-Spark acceptance
matrix).

## Overview

This roadmap implements the complete Spark SPEC as exactly 15 ordered vertical
increments. The end state is one workstation command surface, `sy spark
<host> ...`, talking HTTPS to an unprivileged ARM64 agent on the named DGX
Spark; the agent owns durable intent and an authenticated inference gateway,
while a closed Unix-socket protocol delegates reviewed Docker actions and the
independent memory guard to a root executor. Engines remain digest-pinned OCI
containers on one internal Docker bridge with no published ports, and
`ornith-ai/Ornith-1.5-9B` (alias `ornith-1.5:9b`) is the primary text/tool/vision
fixture. A compatible verified vLLM recipe is the visible fallback; deterministic
compatibility evidence may select a different exact recipe.

Each numbered step is a customer-usable milestone, not a horizontal component
drop. In this document **Delivered CJM** means the main customer-journey
milestone expressed as actor → action → system response → visible outcome.
Implementation uses `/implement` and micro-TDD inside each step. If a step's
diff approaches the roadmap skill's approximately 300-line/five-file review
limit, land it as a short stack of green commits in the same numbered milestone;
do not create a sixteenth feature step, weaken its E2E gate, or leave partially
wired commands in the released binary.

The dependency flow is deliberately one-way:

| Steps | Architecture boundary established | Main journey enabled |
|---:|---|---|
| 1–2 | Typed SSH bootstrap, ARM64 artifact, pinned HTTPS agent | Preview and install |
| 3–5 | Durable operations/auth, root executor, exact recipes | Safely control and explain |
| 6–7 | Verified native model cache and unified-memory safety | Acquire and preflight |
| 8–10 | Docker lifecycle, reconciliation, real vLLM fallback | Serve, inspect, recover |
| 11–13 | OpenAI Responses, Anthropic Messages, vision/embeddings | Use from Codex and Claude Code |
| 14–15 | Evidence-based selection, upgrades, recovery, hardening | Select and operate durably |

### Rules inherited by every step

Every step is incomplete until both the hermetic lane and the real-device lane
pass:

1. Start with one failing behavior test, implement only that behavior, and
   repeat. Unit tests may inject clocks, host samplers, Docker/Hub transports,
   and command runners; integration tests use the real HTTPS, SQLite, Unix
   socket, and streaming wire with fake external systems.
2. Run `make lint`, `make test`, the ordinary workstation build, and
   `cargo build --release --no-default-features --features spark-agent` for
   ARM64 on an ARM64 builder (or with an explicitly configured
   `aarch64-unknown-linux-gnu` target). Run `cargo deny check` whenever
   dependencies or release contents change. No `#[allow(dead_code)]`, unsafe
   block without a local justification, placeholder behavior, or unbounded
   queue/buffer may land.
3. Deploy the candidate to `dgx-spark` only after the local lane is green. Use
   the existing OpenSSH alias as one discrete argument. OpenSSH owns interactive
   authentication; the supplied credential must never be copied into this
   roadmap, argv, environment, a helper such as `sshpass`, shell history,
   evidence, or remote state.
4. Before and after the step-specific operation, capture and compare the
   protected-host fingerprint. The known baseline is DGX software build 7.5.0,
   Ubuntu 24.04/ARM64, kernel `6.17.0-1022-nvidia`, NVIDIA driver `580.159.03`,
   Docker `29.2.1`, and NVIDIA Container Toolkit `1.19.0`; CUDA/runtime and
   firmware identities are compared to the fresh preflight value. Any drift
   fails the step.
5. Mutations are confined to `/opt/sy-spark`, `/etc/sy`, `/var/lib/sy-spark`,
   `/run/sy-spark`, repository-owned LSM/systemd assets, and Docker objects with
   the complete `io.sy.spark.*` label set for that test run. Never update or
   reconfigure the DGX OS, kernel, driver, CUDA/runtime, Docker, container
   toolkit, firmware, bootloader, system Python, sysctls, swap, clocks, power,
   THP, or firewall.
6. Preserve a redacted evidence bundle containing timestamp, source revision,
   ARM64 artifact/manifest hashes, exact commands, operation IDs, before/after
   fingerprints, assertions, bounded journals, and cleanup result. Tokens,
   credentials, prompts, generated content, private paths, and registry auth are
   excluded. A device being unavailable or a gate being skipped is not a pass.
7. Agent/executor fault injection touches only `sy-spark` services and is
   announced before execution. Docker restart and host reboot are reserved for
   Step 15 and require an explicit maintenance-window confirmation immediately
   before the disruptive gate. Real memory exhaustion is never used to test the
   emergency path.

Steps 3–14 deploy development candidates through Step 2's signed, versioned
staging/activation primitive and retain the preceding release until their gate
passes. They do not claim the production `upgrade`/migration/automatic-rollback
contract; that command surface becomes complete only in Step 15.

The exact model commits, image digests, tokenizer/processor hashes, and upstream
recipe commits are empirical implementation inputs. They must be frozen in the
signed recipe/evidence manifest before a real-model gate runs; a moving tag or
model name alone never satisfies a step. No product choice is left open in this
roadmap: each **Risks / unknowns** entry is a fail-closed empirical gate, not a
request to reopen the SPEC's accepted decisions.

---

## Step 1 — Ship a typed, read-only SSH bootstrap preflight

**Goal:** make `sy spark dgx-spark install --dry-run --json` inspect the real
appliance and render an exact, non-mutating installation manifest before any
daemon exists.

**Architecture slice:** add the workstation-only `spark` command boundary and a
typed bootstrap adapter. The adapter invokes system `ssh`/`sftp` with discrete
argv, uploads a hash-verified ARM64 probe to an application-specific temporary
path, calls only a fixed internal `spark bootstrap inspect` entrypoint, and
strictly decodes a versioned JSON inventory. It does not accept arbitrary remote
commands or shell fragments. Host profiles resolve explicit flags over
`SY_SPARK_*` environment values over declarative defaults, and the CLI maps
usage/configuration/unreachable failures to stable exit codes 2 and 4. The
manifest enumerates every future path, identity, certificate, credential,
recipe, unit, migration, service transition, and rollback target, plus the
protected-version invariant.

**Delivered CJM:** operator → runs the dry-run against the existing SSH alias →
`sy` performs read-only discovery and diffs the proposed app-owned installation
→ operator sees a deterministic JSON/human manifest and proof that no DGX base
component will be touched.

**Files:**

- `src/main.rs:17-59,78-125,460-535` (modified) — register and dispatch the
  first `Cmd::Spark` surface without requiring a repository working directory.
- `Cargo.toml:11-13,412-535` (modified) — add the client-safe feature boundary;
  server-only dependencies remain absent from ordinary workstation builds.
- `src/spark/mod.rs` (new; SPEC-prescribed) — module wall and shared exit-code
  constants.
- `src/spark/cli.rs` (new; SPEC-prescribed) — `install --dry-run`, JSON/human
  projection, environment precedence, and complete help/examples for this
  increment.
- `src/spark/wire.rs` (new; SPEC-prescribed) — strict versioned host inventory,
  protected fingerprint, and change-manifest types.
- `src/spark/install.rs` (new; SPEC-prescribed) — injected OpenSSH/SFTP runner,
  fixed bootstrap entrypoints, hash checks, and pure plan construction, following
  the testable runner/options pattern at
  `src/power/apply/installer.rs:237-435`.

**Tests:**

- `src/spark/cli.rs::tests::install_dry_run_obeys_flag_env_default_precedence`
  — pins CLIG configuration precedence and stable JSON/stdout behavior.
- `src/spark/install.rs::tests::host_alias_and_paths_are_discrete_argv`
  — adversarial whitespace/metacharacters cannot become shell syntax.
- `src/spark/install.rs::tests::runner_has_no_password_or_arbitrary_command_input`
  — the public adapter cannot receive or persist a password or caller command.
- `src/spark/install.rs::tests::manifest_is_complete_stable_and_read_only` — a
  fake probe yields the exact planned paths/hashes and zero mutating runner calls.
- `src/spark/wire.rs::tests::inventory_rejects_unknown_or_missing_contract_fields`
  — fail closed on an incompatible bootstrap probe.

**Real `dgx-spark` E2E gate:**

- Build the ARM64 feature-minimal artifact, run the dry-run twice from this
  workstation, and require byte-equivalent normalized manifests.
- Assert the observed host matches the known baseline, reports approximately
  119 GiB visible RAM, local ext4/NVMe capacity, Docker active, one explicit LAN
  address, and the existing login user's Docker-socket denial.
- Compare remote filesystem/unit/Docker inventories before and after; only the
  ephemeral probe may appear during the run, and it must be removed by exact
  path after its hash is recorded.

**Definition of Done:**

- [x] All tests above pass and the common local quality gates are green.
- [x] Dry-run output covers every SPEC installation asset and rejected update
      class without claiming that installation occurred.
- [x] The real-device gate passes twice with unchanged protected fingerprint
      and no persistent remote change.
- [x] CLI help documents JSON, environment variables, exit codes, authentication
      behavior, and the absence of an arbitrary-command escape hatch.

**Risks / unknowns:** the initial probe must work before `/opt/sy-spark/current`
exists. The implementation must use a content-addressed temporary filename and
fixed entrypoint; falling back to constructed shell text is prohibited.

---

## Step 2 — Install a pinned-TLS, unprivileged HTTPS agent atomically

**Goal:** turn the reviewed Step-1 manifest into an idempotent, versioned
installation whose network-facing agent is authenticated and read-only-degraded
until the executor arrives.

**Architecture slice:** adopt optional `axum`/`tower`, `axum-server` with the
workspace rustls/ring provider, `rcgen`, `hmac`/`secrecy`, and the minimum
OpenAPI mechanism behind `spark-agent`. The installer stages and verifies a
signed ARM64 release under
`/opt/sy-spark/releases/<version>`, creates the static non-login `sy-spark`
identity and prescribed state/cache/TLS roots, generates a root-held local CA
and explicit-SAN leaf, atomically switches `current`, and installs one hardened
agent unit. The CA fingerprint and bootstrap token are returned only through the
existing SSH channel into mode-0600 local profile/credential files. The agent
binds only the address through which SSH reached the host, rejects plaintext
off-loopback, exposes authenticated `/api/sy.spark/v1/status`, `doctor`, and
certificate status, uses `sd_notify`/watchdog, and reports executor unavailable
rather than pretending full readiness.

**Delivered CJM:** operator → approves `install --yes` after reviewing the same
manifest → `sy` stages a signed app-owned release and SSH-pinned identity →
operator can run `sy spark dgx-spark status` over authenticated HTTPS and sees a
truthful read-only degraded state, with no SSH needed for normal reads.

**Files:**

- `Cargo.toml:15-89,412-535` (modified) — optional HTTP/TLS/OpenAPI/secret
  dependencies collected only by `spark-agent`; the default desktop/client
  dependency graph stays free of them.
- `src/spark/client.rs` (new; SPEC-prescribed) — host-profile loading, pinned-CA
  rustls client, protected token loading, bounded safe-read retry, and problem to
  exit-code mapping.
- `src/spark/agent.rs` (new; SPEC-prescribed) — TLS listener, auth middleware,
  strict status/doctor/cert routes, graceful drain, notify, and watchdog.
- `src/spark/cli.rs` and `src/spark/wire.rs` (modified) — authenticated
  status/doctor/cert-status command and shared strict response/problem schemas.
- `src/spark/install.rs` (modified) — stage/fsync/verify/atomic-activate,
  identity/directories/permissions, CA/leaf/bootstrap credential, and idempotent
  reapply/rollback metadata.
- `configs/sy/spark/agent.toml` (new; SPEC-prescribed) — explicit bind/CIDR,
  loopback-HTTP rule, concurrency, 8 GiB memory reserve/floor, 100 GiB disk
  reserve, sampling intervals, and retention policy.
- `configs/systemd/system/sy-spark-agent.service` (new; SPEC-prescribed) —
  unprivileged `Type=notify` unit with empty capabilities, `NoNewPrivileges`,
  private devices/tmp, protected host paths, bounded processes/files/RSS, exact
  writable paths, credentials, watchdog, and restart backoff.
- `configs/apparmor.d/sy-spark-agent` or the detected SELinux equivalent (new;
  exact file selected by Step-1 inventory) — enforce the same filesystem,
  capability, device, and network boundary; an unknown active LSM blocks install.

**Tests:**

- `src/spark/install.rs::tests::first_install_then_reapply_is_atomic_and_idempotent`
  — temp roots prove hashes, modes, fsync ordering, symlink activation, and zero
  second-run drift.
- `src/spark/install.rs::tests::protected_update_commands_are_unrepresentable`
  — the installer action enum cannot express package/driver/runtime updates.
- `src/spark/client.rs::tests::wrong_pin_plaintext_lan_and_missing_token_fail_closed`
  — identity/auth failures map to exit 4 without bearer leakage.
- `src/spark/agent.rs::tests::authenticated_status_is_degraded_without_executor`
  — real loopback rustls server/client returns `sy.spark.status/v1` and a typed
  degraded reason.
- `src/spark/agent.rs::tests::unknown_fields_routes_and_cors_are_rejected` —
  strict request parsing and the initial route allowlist hold.
- Unit-string tests run `systemd-analyze verify` and assert the explicit
  hardening directives in `sy-spark-agent.service`.

**Real `dgx-spark` E2E gate:**

- Compare `install --dry-run --json` with the approved mutating manifest, run
  `install --yes`, then re-run install and require an idempotent no-change plan.
- Verify the leaf SAN is the configured Spark address/hostname, the client
  trusts only the SSH-delivered CA pin, wrong-pin and tokenless calls fail, and
  plain HTTP on the LAN never succeeds.
- Verify the unit is `active`, watchdog/live status is present, the process is
  the `sy-spark` user with no effective capabilities, its writable paths match
  policy, and neither that identity nor the login user can open Docker's socket.
- Require the before/after protected host fingerprints to be identical; no
  engine image or model may have been pulled.

**Definition of Done:**

- [x] Local TLS, installer, unit, permission, and negative-path tests pass.
- [x] Ordinary workstation and ARM64 server builds prove the feature boundary.
- [x] Real install/reapply/authentication gates pass with the agent intentionally
      reporting only the missing executor as degraded.
- [x] The bootstrap secret is present only in protected credential stores and
      absent from processes, logs, JSON, manifests, and journals.

**Risks / unknowns:** active AppArmor/SELinux mode is discovered in Step 1 and
must be enforced in the same installation transaction. Step 15 audits and
finalizes the policy, but this step cannot defer an active LSM boundary or
replace it with instructions to disable host security.

---

## Step 3 — Add durable operations, database state, and scoped tokens

**Goal:** make all future mutations asynchronous, idempotent, reconnectable, and
authorized against crash-safe state before adding Docker authority.

**Architecture slice:** adopt one `rusqlite` connection on a dedicated bounded
database-actor thread, `rusqlite_migration`, and `governor`; reuse the Step-2
`hmac`/`secrecy` boundary and existing `ArcSwap`. The actor enables WAL,
`synchronous=FULL`, foreign keys, and a bounded busy timeout; embeds immutable
checksummed migrations; performs a
verified backup before migration; and owns models, aliases, instances,
operations, idempotency, benchmarks, token metadata, and audit tables. The HTTP
layer gains the versioned operation resource, resumable SSE with monotonic IDs
and polling fallback, cancellation, `application/problem+json`, and token
create/list/revoke. Active HMAC verifiers and route/token policy are immutable
snapshots, so inference-path auth never queries SQLite. CLI mutations wait and
render stderr progress by default; `--detach` returns after durable acceptance.

**Delivered CJM:** administrator → creates a narrow token and follows its
operation, disconnects/reconnects, lists it, then revokes it → the same durable
operation and monotonic events resume → the token is effective/revoked at the
documented response boundary without ever displaying its verifier or entering
SQLite as plaintext.

**Files:**

- `src/spark/state.rs` (new; SPEC-prescribed) — single DB actor, migrations,
  backup/checksum policy, state machines, operation/idempotency repository, and
  audit append.
- `src/spark/wire.rs` (modified) — operation/status/problem/token schemas,
  additive enum handling, canonical request hashing, and SSE event shape.
- `src/spark/agent.rs` (modified) — operation/token routes, rate/concurrency
  middleware, snapshot publication, SSE/polling, and cancellation dispatch.
- `src/spark/client.rs` (modified) — idempotency keys, bounded retry preserving
  canonical bytes, event resume/poll fallback, and terminal exit-code mapping.
- `src/spark/cli.rs` (modified) — `operations`, `operations cancel`, `token
  create/list/revoke`, `status`, `doctor`, and `cert status`, each with JSON,
  dry-run where mutating, environment flags, and complete help.
- `Cargo.toml:15-89,412-535` (modified) — narrowly feature-gated SQLite,
  migration and rate-limiter dependencies; HMAC/secrecy remain shared from
  Step 2.

**Tests:**

- `src/spark/state.rs::tests::actor_sets_wal_full_foreign_keys_and_owns_one_connection`
  — verifies pragmas, bounded queue, clean shutdown, and no handler connection.
- `src/spark/state.rs::tests::migrations_are_valid_checksummed_and_n_minus_one_readable`
  — fresh/current/preceding snapshots and backup-before-migrate are enforced.
- `src/spark/state.rs::tests::terminal_operations_and_transitions_are_immutable`
  — every legal/illegal operation transition and cancellation race is covered.
- `src/spark/agent.rs::tests::same_idempotency_request_reuses_operation_changed_body_conflicts`
  — one token/kind/key/body maps to one operation; changed canonical bytes get
  `409`.
- `src/spark/client.rs::tests::sse_resume_then_poll_reaches_same_terminal_result`
  — dropped streams use `Last-Event-ID` and never duplicate terminal output.
- Token tests cover scope, CIDR, expiry, HMAC constant-time verification,
  revocation publication, generic auth errors, limiter cardinality, and secret
  redaction.
- Generated OpenAPI is normalized against a committed fixture; request unknown
  fields reject while response clients ignore additive fields.

**Real `dgx-spark` E2E gate:**

- Create a short-lived least-privilege read token with `--detach`, follow the
  operation through a forced client disconnect/reconnect, and assert one
  operation ID and strictly increasing event IDs.
- Use it successfully for its allowed read, prove a write and admin route are
  denied, revoke it with the bootstrap admin credential, and prove the first
  subsequent request is rejected. Inspect redacted journal/audit output by token
  ID only.
- Restart no host service in this step; verify SQLite pragmas, backup validity,
  mode 0600, bounded resident queue behavior, and unchanged protected stack.

**Definition of Done:**

- [x] State/auth/operation tests and normalized OpenAPI fixture pass.
- [x] Every exposed mutation uses durable acceptance, idempotency, progress,
      cancellation policy, and terminal error semantics; no route bypasses them.
- [x] The real token/SSE/revocation journey passes with no secret in evidence.
- [x] `status` reports database/WAL/backup/auth health truthfully while executor
      remains the only expected degraded dependency.

**Risks / unknowns:** SQLite is the one accepted native dependency boundary.
Queue saturation must return a typed bounded-overload problem rather than spawn
another connection/task or block the HTTP runtime indefinitely.

---

## Step 4 — Introduce the peer-authorized root executor

**Goal:** establish the final privilege boundary: the HTTPS agent can request
only typed Spark operations from one root Unix-socket service and never receives
generic Docker or shell authority.

**Architecture slice:** generalize `sy-ipc::Server` with an injected
`PeerAuthorizer` while preserving its current same-eUID default. Adopt Bollard
pipe-only at this boundary for a fixed read-only `/version` negotiation. Spark
supplies an exact numeric-UID authorizer that admits only the installed
`sy-spark` user;
root, the login user, and group membership alone do not pass. The root executor
listens at `/run/sy-spark/executor.sock`, accepts a strict request enum with
deadlines/cancellation, initially implements only health, protected-host
snapshot, and read-only Docker capability/version inspection,
and returns redacted typed errors. Its system unit has `AF_UNIX` only, no network
listener/home access, read-only system/recipe views, exact application write
roots, Docker-socket access, notify/watchdog, and an independently supervised
event/guard heartbeat. Agent mutations fail `503` while it is unavailable;
authenticated reads remain available.

**Delivered CJM:** operator → runs `status` and `doctor` after installation →
agent authenticates remotely, executor authenticates the agent locally, and the
two reports agree on host/Docker identity → operator sees both services healthy
without granting Docker access to either human login or network parser.

**Files:**

- `crates/sy-ipc/src/server.rs:20-71` (modified) — injectable peer credential
  policy plus backward-compatible same-eUID constructor.
- `crates/sy-ipc/src/lib.rs:12-34` (modified) — export the authorizer contract
  without changing existing consumers.
- `src/spark/executor.rs` (new; SPEC-prescribed) — strict executor wire/handler,
  UDS lifecycle, UID policy, host/Docker read probes, notify/watchdog, and
  redacted error boundary.
- `src/spark/agent.rs` (modified) — bounded executor client, health merge, and
  typed degraded behavior.
- `configs/systemd/system/sy-spark-executor.service` (new; SPEC-prescribed) —
  root `AF_UNIX`-only hardened service ordered after Docker and before agent.
- `configs/systemd/system/sy-spark-agent.service` (modified) — `Wants`/`After`
  executor while retaining read-only degraded availability.
- `configs/apparmor.d/sy-spark-executor` or the detected SELinux equivalent
  (new) — enforce UDS-only networking, Docker socket access, immutable recipe
  roots, and exact application write roots.
- `Cargo.toml:15-89,412-535` (modified) — optional Bollard with only `pipe`, no
  remote discovery/TLS/SSH/BuildKit/WebSocket/generic attach features.

**Tests:**

- `crates/sy-ipc/src/server.rs::tests::default_authorizer_still_accepts_only_same_euid`
  — protects every existing IPC consumer.
- `crates/sy-ipc/src/server.rs::tests::injected_authorizer_sees_kernel_peer_credentials`
  — no caller-supplied UID field influences admission.
- `src/spark/executor.rs::tests::spark_authorizer_accepts_exact_service_uid_only`
  — reject root, login UID, other group member, and missing credentials.
- `src/spark/executor.rs::tests::unknown_action_field_or_oversized_frame_fails_closed`
  — no arbitrary argv/image/mount/path/URL can cross the protocol.
- `src/spark/agent.rs::tests::executor_loss_preserves_reads_and_rejects_mutations`
  — the remote problem is stable/redacted and recovery restores health.
- Unit-string and local systemd tests assert `AF_UNIX`, Docker ordering,
  watchdog, filesystem restrictions, and socket owner/mode.

**Real `dgx-spark` E2E gate:**

- Upgrade the app-owned candidate, verify both units and watchdogs are healthy,
  compare agent and executor host/Docker API fingerprints, and run authenticated
  `status --json`/`doctor --json` from the workstation.
- As the login user and `sy-spark` identity, prove Docker-socket access remains
  denied. Prove only the agent UID can complete a framed executor health call;
  no Docker mutation is performed.
- Inspect listening sockets and require exactly the configured agent HTTPS
  address plus the root-owned UDS—no executor TCP/UDP listener—and then assert
  protected-version equality.

**Definition of Done:**

- [x] Existing `sy-ipc` consumers remain green with unchanged default origin
      semantics.
- [x] Executor protocol, peer, size/deadline/cancel, and unit-hardening tests pass.
- [x] Real status/doctor agree across the two trust zones, and unauthorized
      Docker/UDS access fails.
- [x] No executor request variant contains caller-provided Docker JSON, argv,
      environment, mount, device, URL, or host path.

**Risks / unknowns:** Bollard API negotiation with Docker 29.2.1 is still an
empirical input; this step may inspect `/version` but must not add a Docker CLI
parser or remote-socket fallback if negotiation fails.

---

## Step 5 — Make exact, root-owned recipes explainable and selectable

**Goal:** convert model/host/image compatibility into strict immutable data and
make recipe selection deterministic before any model or engine can be started.

**Architecture slice:** implement a strict TOML recipe catalog owned and parsed
by the executor, signed as part of the release, and exposed to the agent only as
validated capability/evidence documents. Recipes bind architecture, GB10/SM121,
DGX build/driver/toolkit constraints, full model commit and artifact/parser
hashes, engine image digest/source commit, fixed tokenized argv, allowed bounded
substitutions, isolation, resource envelope, health/semantic probes, gateway
methods, license/provenance, and evidence. Unknown fields, paths outside fixed
roots, mutable tags, writable model mounts, shell strings, unreviewed remote
code, and unsupported engine features fail closed. Selection order is named
compatible recipe → exact non-expired tuned winner → compatible verified vLLM
fallback → actionable rejection. `recipes` reports local/upstream/experimental/
disabled status and every mismatch; it never promotes evidence on startup alone.

**Delivered CJM:** operator → runs `sy spark dgx-spark recipes
ornith-ai/Ornith-1.5-9B` → executor evaluates the exact real host and signed
catalog → operator sees why each engine recipe is compatible, unsupported,
experimental, or disabled and sees verified vLLM as the untuned fallback,
without starting a container.

**Files:**

- `src/spark/recipe.rs` (new; SPEC-prescribed) — strict schema, fingerprints,
  bounded substitutions, provenance, compatibility, status, and deterministic
  selection.
- `src/spark/executor.rs` (modified) — root-owned catalog load/signature/digest
  validation and sanitized query result; launch remains unavailable.
- `src/spark/agent.rs` and `src/spark/wire.rs` (modified) — `/recipes` control
  route and stable compatibility/evidence schemas.
- `src/spark/cli.rs` (modified) — `recipes [model] [--json]`, including host,
  model, image, evidence, and remediation projections.
- `configs/sy/spark/recipes/ornith-vllm.toml` (new; exact digest/commits frozen
  during implementation) — the vLLM fallback candidate for
  `ornith-ai/Ornith-1.5-9B`.
- `configs/sy/spark/recipes/qwen3-embedding.toml` (new; exact digest/commits
  frozen during implementation) — the embedding acceptance candidate; no
  capability is advertised until its later real gate passes.

**Tests:**

- `src/spark/recipe.rs::tests::strict_schema_rejects_every_unbounded_or_mutable_field`
  — unknown keys, tags, shell argv, bad digest/path/substitution/mount/remote-code
  policy all reject.
- `src/spark/recipe.rs::tests::full_fingerprint_changes_on_every_identity_input`
  — model/image/host/recipe/parser/corpus/objective changes invalidate evidence.
- `src/spark/recipe.rs::tests::selection_uses_tuned_winner_then_verified_vllm_fallback`
  — deterministic tie and no hidden engine/precision/revision fallback.
- `src/spark/recipe.rs::tests::experimental_requires_exact_name_and_acknowledgement`
  — experimental Rust-native or other engines never become implicit defaults.
- `src/spark/executor.rs::tests::sqlite_cannot_supply_runtime_argv_or_mounts`
  — executor derives launch material solely from the validated root catalog.
- `src/spark/cli.rs::tests::recipes_explains_all_mismatches_without_mutation` —
  human and JSON output project the same wire document.

**Real `dgx-spark` E2E gate:**

- Install the signed catalog, run `recipes --json` for both acceptance models,
  and match every host constraint against the current protected fingerprint.
- In an isolated app-owned test catalog, alter one digest, host constraint,
  unknown field, mount root, and signature in turn; each must block catalog
  activation without affecting the active catalog or starting/pulling anything.
- With no tune result present, require selection preview to name only the exact
  compatible verified vLLM recipe or return the precise missing evidence; it
  must never silently select another engine.

**Definition of Done:**

- [x] Schema/fingerprint/selection/adversarial tests pass.
- [x] Real catalog is signed, digest-reported, immutable to the agent, and
      explainable from the workstation.
- [x] No model, image, container, compile cache, or GPU allocation occurs in
      this step.
- [x] Protected host fingerprint remains unchanged and empirical pins are
      recorded by full commit/digest, not names/tags.

**Risks / unknowns:** a researched engine family may have no exact compatible
Ornith artifact on this host. That is a valid explicit unsupported result, not
permission to invent a conversion, change model revision, or weaken the recipe.

---

## Step 6 — Acquire immutable models in the native Hugging Face cache

**Goal:** deliver `download`, `ls`, `show`, and reference-safe `rm` with resumable
Rust-first transfer and independent verification, without allocating GPU memory.

**Architecture slice:** add a model-acquisition service in the unprivileged
agent. It resolves a repository/ref once to a full commit, plans logical/unique/
temporary bytes, enforces the 100 GiB disk reserve, persists the operation and
commit before transfer, and uses async `hf-hub` against the common native
blob/snapshot cache. A snapshot is invisible to `ls` until tree/files,
`.incomplete` absence, symlink containment, sizes, and recipe hashes verify.
Canonical identity is repository+commit; aliases such as `ornith-1.5:9b` move
only with explicit `--update-alias`. A classified Xet transport/integrity/stall
failure may invoke the release-pinned official Python client exactly once with
fixed argv, `HF_HUB_DISABLE_XET=1`, a read-only credential descriptor, and the
same cache; Rust verification still decides promotion. Auth, policy, 403/404,
disk, and caller cancellation never trigger fallback. `rm` plans against HF and
`sy` references, refuses active data, and deletes only explicitly confirmed
unreferenced blobs.

**Delivered CJM:** operator → downloads
`ornith-ai/Ornith-1.5-9B --alias ornith-1.5:9b`, disconnects or cancels once,
then repeats → transfer resumes the exact immutable commit and verifies it →
operator sees one complete deduplicated model in `ls`, full provenance in
`show`, and no GPU allocation.

**Files:**

- `src/spark/model.rs` (new; the SPEC's model-acquisition boundary) — canonical
  identities/aliases, Hub tree plan, transfer/fallback classification,
  verification, cache reference accounting, and removal planning.
- `src/spark/state.rs` (modified) — model/alias records, download progress,
  completion promotion, and reference-safe transactional removal.
- `src/spark/agent.rs` and `src/spark/wire.rs` (modified) — model routes,
  operation progress, credential isolation, and stable model/removal documents.
- `src/spark/cli.rs` (modified) — complete `download`, `ls`, `show`, and `rm`
  surfaces including revision/alias/detach/dry-run/yes/JSON/env behavior.
- `src/spark/install.rs` (modified) — hash-locked fallback venv under `/opt`,
  systemd HF credential wiring, and explicit refusal to mutate system Python.
- `Cargo.toml:15-89,412-535` (modified) — optional async `hf-hub` and only the
  required transfer features under `spark-agent`.

**Tests:**

- `src/spark/model.rs::tests::repository_revision_and_alias_validation_resists_traversal`
  — example then property tests cover arbitrary name/path bytes and ambiguity.
- `src/spark/model.rs::tests::partial_snapshot_never_promotes_and_resume_reuses_blobs`
  — fake Hub interruption leaves resumable bytes but no complete model.
- `src/spark/model.rs::tests::fallback_runs_once_only_for_classified_xet_failures`
  — auth/not-found/disk/policy/cancel bypass it; helper exit zero without valid
  bytes remains failure.
- `src/spark/model.rs::tests::verification_descriptor_resolves_every_symlink_inside_repo_cache`
  — traversal, swapping, missing blobs, `.incomplete`, size/hash mismatches fail.
- `src/spark/model.rs::tests::remove_plan_counts_unique_bytes_and_refuses_active_snapshot`
  — shared blobs and aliases cannot cause over-delete.
- Hermetic HTTPS E2E covers `download → ls → show → rm` through real operations,
  SQLite, auth, and cache with fake Hub/fallback executables.

**Real `dgx-spark` E2E gate:**

- Freeze the exact Ornith commit, start a detached Rust download, wait for
  durable byte progress, cancel at a safe boundary, prove `ls` omits the partial
  snapshot, reissue the same request, and prove transferred blobs are reused and
  final verification succeeds under alias `ornith-1.5:9b`.
- Record logical versus unique bytes and compare to the native cache. Confirm
  Docker/GPU process/memory state did not change during acquisition.
- On Spark hardware in an isolated test root, inject one classified Xet failure
  and prove the hash-locked HTTP helper shares the cache and cannot promote
  corrupt success; inject auth, missing revision, and reserve errors and prove
  no fallback is attempted.
- Exercise `rm --dry-run`/`rm --yes` only on a small pinned test repository;
  retain the expensive Ornith snapshot for later steps and confirm unrelated
  cache blobs remain.

**Definition of Done:**

- [x] Acquisition/verification/fallback/alias/removal tests pass, including
      real wire-level E2E.
- [x] The real Ornith snapshot is complete, immutable, provenance-reported, and
      reusable by commit; no partial model appears in inventory.
- [x] Python is contained to the signed venv and fixed subprocess contract;
      system Python and the DGX stack are unchanged.
- [x] Every transfer path preserves disk reserve, bounded progress/watchdog,
      cancellation, redaction, and exact protected fingerprint.

**Risks / unknowns:** Spark's observed Xet failure window must be measured. Tune
the no-progress threshold conservatively from evidence; never classify a slow
download as success or implement a custom Hub/Xet protocol.

---

## Step 7 — Enforce aggregate unified-memory admission and emergency shedding

**Goal:** make unsafe starts unrepresentable and protect host availability even
if the network agent or Docker control path is unhealthy.

**Architecture slice:** implement overflow-safe admission from one fresh host
snapshot: aggregate cold-start peaks for every desired-running instance plus the
candidate must fit `MemTotal - 8 GiB`; live `MemAvailable` after incremental
peak must retain 8 GiB; full-memory PSI, swap-in, disk reserve, compatibility,
and one high-memory transition gates must all pass. Missing/stale telemetry fails
closed. The root executor owns an independent sampler—500 ms during start/tune,
2 s steady—that fsyncs restart suppression before selecting the newest
transitional managed engine, then the most recently started growing managed
engine, after three floor breaches or full PSI avg10 ≥2%. A validated
PID/start-time/cgroup-v2 `cgroup.kill` path is last-resort defense only if Docker
times out. It never targets unlabeled or mismatched work. Cgroup ceilings and
OOM scores are defense in depth; CUDA free memory, `dmem`, swap, and engine
percentages remain diagnostics rather than admission authority.

**Delivered CJM:** operator → previews serving a model → `sy` evaluates every
existing desired instance, live pressure, swap, disk, and exact recipe envelope
→ operator gets either an auditable safe-capacity plan or a precise refusal
before Docker/GPU side effects, while `doctor` shows the fixed 8 GiB policies.

**Files:**

- `src/spark/resources.rs` (new; SPEC-prescribed) — host sampler trait,
  overflow-safe admission, transition semaphore, guard state machine, victim
  ordering, cgroup identity validation, and resource report.
- `src/spark/executor.rs` (modified) — root sampler/guard loop, fsynced emergency
  journal, suppression action contract, watchdog health, and exact managed
  cgroup actuator.
- `src/spark/agent.rs` (modified) — serialized admission coordinator, fresh
  executor snapshot requirement, dry-run result, and emergency-record import.
- `src/spark/state.rs` (modified) — declared/measured envelopes, transition
  leases, suppression/cause, and audit persistence.
- `src/spark/wire.rs` and `src/spark/cli.rs` (modified) — resource/admission/
  doctor schemas and `serve --dry-run` projection with actionable remediation.
- `configs/sy/spark/agent.toml` (modified) — explicit 8 GiB reserve/floor,
  three samples, 2% PSI, intervals, and 100 GiB disk reserve; lowering requires
  the accepted explicit acknowledgement and audit path.

**Tests:**

- `src/spark/resources.rs::tests::aggregate_reboot_and_live_envelopes_hold_at_boundaries`
  — example then property tests cover sums near integer limits and exactly 8 GiB.
- `src/spark/resources.rs::tests::missing_stale_psi_or_swap_telemetry_fails_closed`
  — no optimistic admission from partial observations.
- `src/spark/resources.rs::tests::only_one_high_memory_transition_can_hold_lease`
  — concurrent starts/tunes cannot race the snapshot.
- `src/spark/resources.rs::tests::guard_orders_transitional_then_recent_growing_victim`
  — deterministic choice across arbitrary event/read interleavings.
- `src/spark/resources.rs::tests::cgroup_kill_requires_label_pid_start_time_and_path_match`
  — PID reuse/mismatch aborts without signaling anything.
- `src/spark/agent.rs::tests::emergency_record_suppresses_restart_and_fails_operation`
  — agent recovery imports exact root evidence and never auto-restarts victim.

**Real `dgx-spark` E2E gate:**

- Run positive Ornith `serve --dry-run` and an isolated signed oversized-recipe
  negative case; assert one fresh real host snapshot, aggregate calculation,
  8 GiB/100 GiB floors, zero image/container/GPU side effects, and stable
  problem codes.
- Run the production guard logic on Spark with injected low readings and a
  bounded app-owned test cgroup/actuator. Prove exact victim/suppression/journal
  behavior and that a concurrently visible unmanaged process/container is never
  selected. Do not lower real `MemAvailable` or create swap/PSI pressure.
- Verify the executor watchdog becomes unhealthy when sampling is faulted and
  that new admission fails until restored; compare protected fingerprints.

**Definition of Done:**

- [x] Arithmetic, telemetry, concurrency, victim, and cgroup identity tests pass.
- [x] Real positive/negative dry-runs and synthetic guard gate pass without
      exhausting memory or touching unmanaged work.
- [x] Agent cannot authorize a start without a healthy executor guard and a
      fresh complete snapshot.
- [x] Safety thresholds remain declarative, dry-run visible, audited, and never
      silently rewritten by tuning.

**Risks / unknowns:** GB10 CUDA allocations may not be completely accounted by
cgroup `memory` or `dmem`. The roadmap therefore keeps measured host-wide
`MemAvailable`/PSI/swap authoritative until real evidence proves otherwise.

---

## Step 8 — Run one isolated, typed Docker engine lifecycle

**Goal:** deliver `serve`, `ps`, `logs`, and `stop` end to end with a harmless
ARM64 fixture before putting a large model or CUDA engine behind the executor.

**Architecture slice:** extend the Step-4 Bollard pipe-only adapter behind a narrow
`ContainerRuntime` trait. The executor negotiates Docker 29.2.1, ensures one
user-defined `--internal` bridge, pulls only recipe digest-pinned images, creates
one exact generation with complete `io.sy.spark.*` labels, no published ports,
no Docker socket/secrets/home, read-only repository-scoped model mount, bounded
tmpfs/cache, dropped capabilities, seccomp, PIDs/cgroup resource policy, and
restart `no`. After native plus semantic fixture health, it changes restart to
`unless-stopped`; failure disables restart, captures bounded logs, and removes
only that generation. The agent reaches the executor-observed container IP and
recipe port through one fixed, bounded upstream adapter. `stop` first persists
stopped intent, closes new traffic, drains, disables restart, then removes the
exact container while retaining model/cache. `ps` reports desired and observed
state; `logs` is bounded, cursored, scoped, and redacted.

**Delivered CJM:** operator → serves a signed harmless fixture, inspects `ps`
and logs, then stops it → `sy` creates exactly one internal managed engine,
shows intent versus reality, drains/removes only it, and gives an already-absent
instance idempotent stop semantics → operator sees Ollama-shaped lifecycle
semantics without Docker access.

**Files:**

- `src/spark/executor.rs` (modified) — `ContainerRuntime`, Bollard adapter,
  network/image/container/log actions, labels, isolation validation, restart
  transitions, and exact cleanup.
- `src/spark/upstream.rs` (new; SPEC-prescribed) — fixed container-address
  connector, method/path/header/body/timeout bounds, streaming health probe, and
  no caller URL.
- `src/spark/agent.rs` and `src/spark/state.rs` (modified) — transactional
  serve/stop workers, instance generations, desired/observed projections, logs,
  and compensation.
- `src/spark/wire.rs` and `src/spark/cli.rs` (modified) — complete
  `serve`/`ps`/`stop`/`logs` contracts, JSON, dry-run, detach, timeout, follow,
  objectives, names, recipe acknowledgement, and stable problems.
- `Cargo.toml:15-89,412-535` (modified) — Bollard with only the local pipe
  feature and no TLS/SSH/BuildKit/attach discovery surface.
- `tests/fixtures/spark-engine/` (new fixture assets) — tiny signed
  ARM64-compatible streaming/health image input with no GPU requirement and a
  frozen resulting image digest.

**Tests:**

- `src/spark/executor.rs::tests::container_spec_is_fully_recipe_derived_and_locked_down`
  — snapshot exact image/argv/labels/network/mount/device/cap/seccomp/PID config.
- `src/spark/executor.rs::tests::health_failure_disables_restart_and_compensates_generation`
  — no unattended crash loop or orphan route/container.
- `src/spark/upstream.rs::tests::connector_cannot_target_caller_url_or_forbidden_route`
  — only executor-observed address plus recipe allowlist is representable.
- Hermetic E2E: duplicate concurrent serve creates one operation/container;
  conflicting same name gets `409`; `ps`, bounded/redacted logs, and stop
  traverse real HTTPS/SQLite/UDS with fake Docker/engine. A state test covers
  already-absent stop idempotency.
- Docker integration E2E verifies one shared internal bridge, host-to-engine and
  accepted peer-engine reachability, no published ports/external egress,
  read-only mounts, restart transitions, event/log bounds, and label-scoped
  cleanup.

**Real `dgx-spark` E2E gate:**

- Install the signed non-GPU fixture recipe/image, serve one then two named
  instances, and verify exactly one shared internal bridge, complete labels,
  digest pin, no host port, no external egress, accepted peer reachability, and
  host-agent health access. `doctor` must disclose the accepted peer-lateral
  residual risk.
- Confirm `ps --json` matches Docker inspection without exposing the internal IP
  or Docker detail remotely. Exercise bounded logs and forbidden route/method
  rejection.
- Stop one instance twice; prove its model/cache persists, the second remains,
  and no unlabeled/unrelated Docker object changes. Stop the test remainder by
  exact labels and verify clean state plus unchanged protected fingerprint.

**Definition of Done:**

- [x] Fake-runtime, wire-level, and real Docker integration tests pass.
- [x] The real fixture lifecycle works entirely through HTTPS and the typed UDS;
      no CLI/Docker-shell fallback is present.
- [x] Network, mounts, capabilities, labels, restart policy, logs, and cleanup
      satisfy the SPEC and affect only managed test objects.
- [x] `serve`, `ps`, `logs`, and `stop` help/JSON/exit-code contracts are complete.

**Risks / unknowns:** a Docker internal bridge intentionally permits managed
engine-to-engine port reachability. Tests must report this accepted D04a tradeoff
accurately; they must not claim per-instance network isolation.

---

## Step 9 — Reconcile desired intent with Docker reality across crashes

**Goal:** make durable running/stopped intent survive client, agent, executor,
and partial side-effect failures without duplicate containers or unsafe restart
loops.

**Architecture slice:** reconciliation runs at agent startup, on validated Docker
events, after mutations, and on periodic full scans. It matches stable instance
ID, monotonically increasing generation, role, image/recipe fingerprint, and
exact bridge attachment against SQLite; names and labels alone are untrusted.
The state machine resumes a matching create/warm, republishes a healthy desired
generation, removes a stopped desired generation, and marks missing/broken
running intent degraded. Five failures in ten minutes fsync restart suppression,
disable/stop the engine, and leave desired-running/observed-failed visible until
a new explicit serve generation. Duplicate, future-generation, malformed, or
wrong-network containers are restart-disabled and quarantined for `doctor`, not
silently adopted/deleted. Kill points around every DB/executor side effect are
covered. Cancelling a pre-health serve removes its generation; cancelling an
old completed serve cannot stop a healthy instance.

**Delivered CJM:** operator → starts the fixture, loses/restarts only the `sy`
control services, reconnects, then stops it → the same instance, operation
history, container ID, and endpoint intent are recovered without engine restart
→ operator sees explicit degraded/quarantine evidence instead of duplicates or
hidden fallback.

**Files:**

- `src/spark/state.rs` (modified) — desired/observed transition table,
  generations, restart window/suppression, operation recovery, and durable kill
  point ordering.
- `src/spark/executor.rs` (modified) — Docker event stream plus full label scan,
  validated observations, quarantine/suppression actions, and emergency-record
  replay boundary.
- `src/spark/agent.rs` (modified) — startup/event/periodic reconciliation,
  readiness publication, operation resumption, and degraded route behavior.
- `src/spark/resources.rs` (modified) — persistent desired set is revalidated
  against aggregate reboot envelope before restart policy is enabled.
- `src/spark/wire.rs` (modified) — truthful desired/observed/generation/restart/
  quarantine fields in instance/status/doctor documents.
- `tests/spark_reconcile_e2e.rs` (new) — deterministic fake-clock/fake-Docker
  crash/event/recovery matrix through the real client/agent/executor protocol.

**Tests:**

- Table tests cover every desired/observed/operation transition and kill point
  before/after DB commit, container create/start/health, restart enable, route
  publish, stopped commit, drain, and remove.
- `tests/spark_reconcile_e2e.rs::duplicate_idempotent_serve_after_disconnect_creates_one_generation`
  — client retry and agent recovery converge.
- `tests/spark_reconcile_e2e.rs::missed_event_is_closed_by_full_scan_without_name_adoption`
  — stale endpoint/wrong network/label spoofing do not route.
- `tests/spark_reconcile_e2e.rs::five_failures_suppress_restart_and_require_new_serve`
  — bounded crash loops cannot thrash indefinitely.
- `tests/spark_reconcile_e2e.rs::stop_commit_survives_agent_death_and_retains_model`
  — restart stays disabled and cleanup resumes idempotently.
- Cancellation/property tests permute events/generations and prove no two active
  containers or routes exist for one instance generation.

**Real `dgx-spark` E2E gate:**

- With the harmless fixture healthy, record instance/operation/container IDs.
  After explicit confirmation for these app-owned service faults, restart agent
  then executor independently; each time prove the fixture container ID/restart
  count is unchanged and the same route/intent returns after health.
- In isolated labeled fixtures, inject a missed event, stale generation, wrong
  bridge, duplicate, and bounded crash loop. Verify quarantine/suppression and
  `doctor` evidence; do not delete ambiguous evidence until the test verifies
  exact ownership.
- Stop the fixture, restart the agent, and prove no recreation occurs while the
  downloaded model/cache remains. Do not restart Docker or the host in this
  step. Assert protected stack equality.

**Definition of Done:**

- [x] Transition, interleaving, kill-point, and black-box recovery tests pass.
- [x] Real agent/executor restart recovery preserves healthy engine identity and
      exact durable state; stopped intent stays stopped.
- [x] Crash-loop and ambiguous-container behavior is bounded, suppressed,
      visible, and never becomes implicit fallback/deletion.
- [x] Every reconciler input is bounded and untrusted until checked against
      SQLite plus the root catalog.

**Risks / unknowns:** Docker event delivery is lossy across disconnects. The
periodic full scan is mandatory correctness closure, not an optional fallback;
event delivery alone can never be the durable observation model.

---

## Step 10 — Serve the frozen Ornith model through verified vLLM fallback

**Goal:** prove the real primary model can start safely on GB10 using the exact
verified vLLM fallback and expose a minimal authenticated streaming inference
route before client-specific adapters are added.

**Architecture slice:** freeze the exact Ornith commit, processor/tokenizer
hashes, vLLM source/version and ARM64 image digest that pass SM121/driver checks.
Mount only that repository's immutable snapshot read-only plus its isolated
fingerprinted compile cache; enable offline/local-files-only behavior and only
recipe-fixed arguments. `serve` with no valid tune result must select vLLM,
admit its measured cold-start envelope, pull/create with restart `no`, run
engine-native and semantic one-token/model-identity probes, then enable
durability and publish a route snapshot atomically. The first gateway slice uses
the bounded typed internal generation-event model and exposes authenticated
`GET /openai/<instance>/v1/models` and `POST .../v1/completions` only; all engine
health/debug/metrics/tokenizer/admin routes remain unreachable. Streaming is
backpressured and disconnect-cancellable, never fully buffered.

**Delivered CJM:** operator → runs `serve ornith-1.5:9b` without tuning → `sy`
selects the visible verified vLLM fallback, passes aggregate admission, warms and
semantically verifies the exact model, then publishes it → operator receives a
stable authenticated endpoint and truthful `ps` data rather than engine shell
instructions.

**Files:**

- `configs/sy/spark/recipes/ornith-vllm.toml` (modified) — replace all empirical
  inputs with the frozen locally verified commit/digest/hashes, measured
  envelope, fixed argv, capability limits, and evidence.
- `src/spark/recipe.rs` (modified) — verified-vLLM fallback and evidence expiry
  enforcement using the full current host/model/image fingerprint.
- `src/spark/executor.rs` (modified) — NVIDIA runtime/device request derived
  solely from recipe, repository-scoped mounts, compile-cache promotion, and
  exact vLLM health/identity observation.
- `src/spark/upstream.rs` (modified) — vLLM completion stream decoding into the
  protocol-neutral bounded event model and semantic probe.
- `src/spark/gateway.rs` (new; SPEC-prescribed) — immutable healthy route
  snapshot, auth/limits/header stripping, public identity rewrite, models plus
  streaming completions, drain/disconnect propagation, and strict route deny.
- `src/spark/agent.rs` (modified) — publish/drain route only after semantic
  readiness and reflect startup measurements in operations/`ps`.

**Tests:**

- `src/spark/recipe.rs::tests::untuned_ornith_selects_only_exact_verified_vllm`
  — no hidden engine, precision, context, or model fallback.
- `src/spark/gateway.rs::tests::route_is_absent_until_semantic_identity_passes`
  — warming/rebooting yields bounded `503 Retry-After`, never stale routing.
- `src/spark/gateway.rs::tests::only_models_and_completions_are_public_in_this_increment`
  — every alternate vLLM/debug/health/admin/SSRF-shaped route rejects.
- `src/spark/upstream.rs::tests::slow_stream_is_bounded_and_client_disconnect_cancels_upstream`
  — agent memory does not grow with output and no zombie generation remains.
- `tests/spark_vllm_e2e.rs` (new, real-Spark gated) — exact model identity,
  deterministic completion, stop/start, reserve, forbidden-route, and logs.

**Real `dgx-spark` E2E gate:**

- With no tuned winner in SQLite, serve `ornith-1.5:9b`, capture the full selected
  fingerprint, and require vLLM specifically. Assert admission's measured peak,
  no swap-in/PSI breach, 8 GiB floor, exact image digest/model identity, one
  container, one shared bridge, and no published engine port.
- Send deterministic non-streaming and slow streamed completions through the
  agent TLS endpoint, disconnect one stream, and verify correctness, bounded
  agent RSS, cancellation, public identity rewrite, and denial of every
  non-allowlisted vLLM route.
- Stop and re-serve explicitly; verify model and successful compile cache remain,
  generation increments, stale routes do not, and protected versions are
  unchanged. Leave one healthy instance for Steps 11–15 only if its measured
  state remains above the safety floor.

**Definition of Done:**

- [x] Exact recipe, gateway, streaming, and real-vLLM tests pass.
- [x] The recipe may be marked `local-verified` only with attached real-host
      correctness/safety/identity evidence; startup alone is insufficient.
- [x] Untuned `serve` visibly chooses vLLM and never downloads, tunes, converts,
      or retries another recipe implicitly.
- [x] The real model is usable only through authenticated agent routes; engine
      ports, credentials, Docker details, and alternate APIs remain inaccessible.

**Risks / unknowns:** exact Ornith support on the frozen vLLM/GB10 stack is an
empirical hard gate. Failure blocks this step with compatibility evidence; it
does not authorize substituting an older or OpenAI-specific model.

---

## Step 11 — Implement normative OpenAI Responses and prove Codex use

**Goal:** make the Ornith instance a real Codex custom provider through the
Responses API, including bounded SSE and client-side tool calls rather than a
name-only compatibility shim.

**Architecture slice:** expand the OpenAI adapter to strict
`POST /v1/responses`, chat completions, and the already delivered models/
completions surface. Requests decode into the shared typed internal event model;
responses encode text items, function/custom-tool calls and outputs, usage,
incomplete/error states, stateless continuation, and the exact SSE event order.
Unsupported OpenAI-hosted tools reject explicitly. The gateway enforces token
scope, recipe context/output and body/header limits, per-instance concurrency,
idle timeout, forwarding/hop-by-hop header stripping, disconnect propagation,
and immutable healthy-generation routing. `client-config --client codex`
renders a user-level custom-provider TOML fragment, CA environment guidance, and
secret environment-variable name—never the secret. Current official OpenAI
documentation requires custom-provider `base_url`, `env_key`, and
`wire_api = "responses"`; `responses` is the only supported wire value and
direct `experimental_bearer_token` is discouraged
([OpenAI Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)).

**Delivered CJM:** coding agent operator → asks `sy` for a Codex config and
exports the scoped token from its protected file → pinned Codex connects to the
Ornith Responses endpoint, streams a tool call, receives the local tool result,
and finishes the task → user sees a normal Codex run backed by the named Spark,
with no OpenAI-hosted tool silently substituted.

**Files:**

- `src/spark/gateway.rs` (modified) — strict OpenAI route table, request limits,
  auth, response/SSE encoder, errors, usage, continuation, and generation drain.
- `src/spark/upstream.rs` (modified) — vLLM chat/tool stream translation into
  protocol-neutral typed events with bounded buffers and parser limits.
- `src/spark/wire.rs` (modified) — strict OpenAI Responses/chat/tool/usage/error
  types and public-model identity; image variants are represented but remain
  capability-rejected until Step 13.
- `src/spark/agent.rs` (modified) — immutable route/auth snapshots, independent
  inference semaphore/rate metrics, and no database access on request hot path.
- `src/spark/cli.rs` and `src/spark/client.rs` (modified) — `client-config
  <instance> --client codex [--json]` and endpoint discovery without token
  printing or config-file mutation.
- `tests/spark_openai_e2e.rs` (new) — fake-engine protocol matrix plus a gated
  exact Codex binary journey.

**Tests:**

- Response schema tests cover string/structured input, output items, function
  and custom-tool call/output pairing, usage, incomplete/error, continuation,
  unknown fields, and exact SSE lifecycle order.
- `src/spark/gateway.rs::tests::openai_allowlist_rejects_hosted_tools_and_engine_routes`
  — file/web/computer-hosted tools and every non-public upstream path fail in an
  OpenAI-shaped response.
- `src/spark/gateway.rs::tests::headers_limits_auth_and_generation_are_enforced_before_upstream`
  — no forwarding/authorization header or stale route crosses the boundary.
- `tests/spark_openai_e2e.rs::slow_tool_stream_disconnect_and_retry_are_bounded`
  — fake engine disconnects mid-token; retry/cancel semantics and memory bounds
  remain correct.
- `tests/spark_openai_e2e.rs::client_config_uses_responses_env_key_and_never_secret`
  — normalized output matches the current official Codex custom-provider keys.

**Real `dgx-spark` E2E gate:**

- Pin and record the exact Codex binary version. Generate config into a temporary
  user-level `CODEX_HOME`, point its provider to
  `/openai/<instance>/v1`, trust the pinned CA through a protected environment
  path, and supply a narrow inference token only through the named environment
  variable.
- Run a deterministic streamed task requiring exactly one local fixture tool
  call and result round trip, then a stateless continuation. Assert terminal
  text hash/structure, tool arguments, event ordering, usage, model identity,
  and no attempt at a hosted tool. Evidence records only redacted hashes.
- Run concurrent requests and a disconnected stream while observing the 8 GiB
  floor, no swap-in/thermal throttle, bounded agent RSS, `ps/status/metrics`, and
  unchanged protected fingerprint.

**Definition of Done:**

- [x] Responses/chat/tool/SSE/security tests and OpenAPI fixture pass.
- [x] The exact pinned Codex binary completes the real streamed tool journey
      through the generated config and scoped token.
- [x] Client config matches current official provider keys, is user-level, and
      never prints/writes a token or enables unsupported web search/WebSockets.
- [x] Gateway exposes only implemented OpenAI capabilities and meets bounded
      streaming/cancellation semantics.

**Risks / unknowns:** Codex's custom-provider contract can evolve. Pin the tested
binary and configuration schema in evidence; a newer binary is a new
compatibility input, not assumed backward compatibility.

---

## Step 12 — Implement Anthropic Messages and prove Claude Code use

**Goal:** expose the same healthy Ornith generation through an independently
implemented Anthropic Messages adapter and complete a real Claude Code streamed
tool task.

**Architecture slice:** add only `POST /anthropic/<instance>/v1/messages` and
`POST .../v1/messages/count_tokens`. Decode system content, text user/assistant
blocks, client-side `tool_use`/`tool_result`, stop controls, and later-gated image
blocks into the shared internal model; encode Anthropic-native message/content-
block SSE events, usage, stop reasons, and errors directly rather than
translating public OpenAI JSON. Accept `x-api-key` or bearer presentation only as
the same scoped `sy` token, strip both upstream, and reject provider-hosted tools
and beta features unless explicitly implemented. Count tokens with the exact
recipe tokenizer and identity. `client-config --client claude-code` prints the
base URL above `/v1`, CA guidance, model/instance, and secret environment names
for the pinned Claude Code contract without revealing or persisting the token.

**Delivered CJM:** coding agent operator → requests the Claude Code config →
pinned Claude Code sends native Messages traffic to the same Ornith instance,
streams `tool_use`, consumes `tool_result`, and completes → user can choose
Codex or Claude Code without changing model lifecycle or exposing engine APIs.

**Files:**

- `src/spark/gateway.rs` (modified) — Anthropic routes, auth presentation,
  strict request/response/SSE/error adapter, token count, limits, and deny list.
- `src/spark/upstream.rs` (modified) — shared generation/tool event production
  sufficient for both adapters without public-protocol cross-translation.
- `src/spark/wire.rs` (modified) — Anthropic Messages/content/tool/usage/error/
  stream types; image blocks remain capability-rejected until Step 13.
- `src/spark/cli.rs` and `src/spark/client.rs` (modified) — Claude Code
  client-config projection and endpoint discovery with secret-name-only output.
- `tests/spark_anthropic_e2e.rs` (new) — exact protocol fixture suite plus gated
  pinned Claude Code binary journey.

**Tests:**

- Message tests cover system/text blocks, multiple tool calls/results, stop
  reasons, usage, count_tokens, malformed pairing, oversized content, and exact
  Anthropic SSE message/content-block ordering.
- `src/spark/gateway.rs::tests::anthropic_and_openai_adapters_share_events_not_json`
  — each public protocol round-trips native semantics through typed internals.
- `src/spark/gateway.rs::tests::anthropic_hosted_tools_beta_and_unknown_routes_reject`
  — no silent forwarding or engine extension.
- `tests/spark_anthropic_e2e.rs::equivalent_tool_task_preserves_stop_and_usage`
  — equivalent OpenAI/Anthropic fixture tasks preserve content/tool semantics
  while returning protocol-native shapes.
- `tests/spark_anthropic_e2e.rs::client_config_places_base_above_v1_and_omits_secret`
  — output matches the pinned Claude Code environment contract.

**Real `dgx-spark` E2E gate:**

- Pin and record the exact Claude Code binary. Generate a temporary config/env
  against `/anthropic/<instance>`, use the same SSH-pinned CA and an inference-
  only token, and run the deterministic streamed one-tool task used in Step 11.
- Assert exact tool arguments/result consumption, native event ordering, stop
  reason, count_tokens consistency, usage, public model identity, and rejection
  of one hosted/beta tool. Store only redacted hashes and IDs.
- Run pinned Codex and Claude Code concurrently against the same instance; prove
  independent rate/concurrency accounting, stable `ps/status/metrics`, no safety
  floor/swap/thermal breach, and unchanged protected stack.

**Definition of Done:**

- [x] Anthropic request/SSE/tool/count/error and cross-adapter tests pass.
- [x] Pinned Claude Code completes the real streamed tool journey through its
      generated native base-URL config.
- [x] Neither adapter converts from the other's public JSON or exposes hosted/
      beta/engine-only functionality.
- [x] Concurrent real Codex/Claude use remains bounded, authenticated, correctly
      attributed, and resource-safe.

**Risks / unknowns:** Claude Code environment and gateway behavior are pinned
compatibility inputs. If its tested release requires an undocumented route or
provider-hosted feature, reject that release rather than proxying an unreviewed
surface.

---

## Step 13 — Gate and verify vision plus text embeddings

**Goal:** complete the accepted capability scope with local image understanding
through both public adapters and deterministic text embeddings through OpenAI.

**Architecture slice:** capability advertisement is recipe-derived and exact.
Promote Ornith vision only after its frozen processor/image path passes on GB10;
accept bounded inline/local image content in OpenAI Responses and Anthropic
Messages, normalize it into the typed image-part model, and reject remote URLs,
files, oversized/unsupported formats, or text-only recipes before upstream work.
Download and serve frozen `Qwen/Qwen3-Embedding-0.6B` under its own exact recipe,
resource envelope, instance, and immutable route; implement
`POST /openai/<instance>/v1/embeddings` with input batching limits, dimensions,
usage, normalization contract, public identity, and no generation routes on an
embedding-only instance. Desired-state/admission/reconciliation semantics remain
identical for both capabilities.

**Delivered CJM:** user/coding agent → sends a fixed local image to Ornith from
either client protocol and text to the embedding instance → gateway validates
the declared capabilities and exact limits → user receives the expected visual
answer and reproducible vectors, while unsupported remote media/routes fail
before reaching an engine.

**Files:**

- `configs/sy/spark/recipes/ornith-vllm.toml` (modified) — frozen processor
  identity, accepted image formats/bytes/count, VLM health probe, and verified
  vision evidence.
- `configs/sy/spark/recipes/qwen3-embedding.toml` (modified) — exact model/image/
  tokenizer/dimension/normalization/batch/resource/health evidence.
- `src/spark/gateway.rs` and `src/spark/wire.rs` (modified) — bounded image
  decoding/validation in both adapters and strict OpenAI embeddings shapes.
- `src/spark/upstream.rs` (modified) — exact VLM and embedding upstream adapters
  into typed image/event/vector representations.
- `src/spark/recipe.rs` (modified) — capability-specific route/limit/health and
  incompatible-method rejection.
- `tests/spark_modalities_e2e.rs` (new) — local image and embedding fixtures,
  negative route/media matrix, plus real-Spark gated acceptance.

**Tests:**

- Image tests cover OpenAI and Anthropic native shapes, format/magic validation,
  byte/count/dimension limits, remote URL/file/traversal denial, cancellation,
  and text-only capability rejection.
- Embedding tests cover string/list inputs, ordering, dimensions, finite values,
  normalization tolerance, deterministic cosine-similarity ranking, usage,
  batch/body limits, and generation-route denial.
- `src/spark/recipe.rs::tests::advertised_routes_equal_verified_capabilities`
  — no route appears because an upstream engine happens to implement it.
- `tests/spark_modalities_e2e.rs::both_public_adapters_preserve_same_local_image_semantics`
  — typed internal representation does not erase protocol-native responses.
- `tests/spark_modalities_e2e.rs::embedding_instance_survives_stop_restart_with_same_identity`
  — state/cache/reconciliation behavior matches text generation.

**Real `dgx-spark` E2E gate:**

- Through pinned Codex-compatible Responses traffic and pinned Claude Code
  Messages traffic, send the same deterministic repository-owned image fixture
  to the frozen Ornith instance and assert expected structured visual facts,
  model identity, native usage/events, and zero outbound media fetch.
- Download the frozen Qwen embedding commit, serve its named instance, request a
  fixed sentence set via `/v1/embeddings`, and verify exact dimension,
  normalization tolerance and expected similarity order.
- Send remote URL, oversized image, VLM request to embedding, and generation
  request to embedding negatives; all fail at the gateway. Run both instances
  within aggregate admission, inspect `ps/status/metrics`, stop/restart the
  embedding instance, and compare protected fingerprints.

**Definition of Done:**

- [x] Image/embedding/capability/negative tests and OpenAPI fixture pass.
- [x] Both adapters answer the real local-image fixture through Ornith with no
      remote fetch, and the real Qwen instance returns verified embeddings.
- [x] Every capability, limit, identity, resource envelope, and health probe is
      exact-recipe data; engines cannot self-advertise public routes.
- [x] Concurrent VLM/embedding operation remains above safety floors with no
      hidden conversion, download, or unrelated media support.

**Risks / unknowns:** an Ornith revision may advertise vision while a chosen
engine build lacks the exact processor/parser behavior. Text success cannot
promote VLM; capability stays disabled until this step's two-adapter gate passes.

---

## Step 14 — Evaluate bounded engine candidates and persist a compatible winner

**Goal:** make “best supported on this Spark” a reproducible functional decision
for one exact fingerprint/objective, while preserving verified vLLM as the
visible fallback.

**Architecture slice:** add a resident-free compatibility evaluator. `bench`
evaluates one exact candidate and `tune` evaluates a finite recipe-declared set
one candidate at a time across vLLM, SGLang, TensorRT-LLM, llama.cpp, NIM opt-in,
and Rust-native families only where each has frozen locally verified evidence.
Unsupported and uninstalled families are reported, never fabricated or
downloaded; Candle remains watchlist until an exact safe recipe exists. Every
eligible candidate must prove exact host/model/image/parser identity, declared
API capabilities and forbidden routes, semantic correctness, admission and
emergency-safety compatibility, isolation, health, restart durability, and
atomic fingerprint-isolated compile-cache promotion. Selection prefers the
highest recipe-declared capability tier, then the simplest compatible launch
profile, then recipe ID. Raw functional evidence and the full fingerprint are
durable; any identity change invalidates selection. Exact verified vLLM remains
the visible fallback rather than a winner declared by policy.

**Delivered CJM:** operator → runs `tune ornith-1.5:9b --objective agent` → `sy`
evaluates only installed, exact, safe candidates and explains unsupported ones →
the deterministic compatible winner and its evidence are persisted → the next
ordinary `serve` chooses that winner; deleting or invalidating it visibly
returns to the verified vLLM fallback.

**Files:**

- `src/spark/bench.rs` (new; extracted compatibility boundary) — functional
  evidence gates, explicit unsupported-family reporting, deterministic scoring,
  and bounded candidate evaluation.
- `src/spark/recipe.rs` (modified) — finite tuning axes, eligible engine-family
  evidence, winner selection/invalidation, and vLLM fallback preservation.
- `src/spark/state.rs` (modified) — functional evidence, objective/full
  fingerprint, selected flag, invalidation reason, and compile-cache references.
- `src/spark/agent.rs` and `src/spark/wire.rs` (modified) — compatibility/tuning
  operations, progress, results, metrics, cancellation, and stable schemas.
- `src/spark/cli.rs` (modified) — complete `bench` and `tune` JSON/dry-run/
  detach/objective/recipe surfaces.
- `configs/sy/spark/recipes/*.toml` (modified or new only after exact preflight)
  — digest-pinned eligible candidate recipes. Before editing, amend this Files
  entry with each concrete filename/digest; unsupported candidates remain
  explicit evidence, never placeholder launch recipes.

**Tests:**

- `src/spark/bench.rs::tests::functional_failures_cannot_rank_or_promote`
  — API, identity, capability, safety, isolation, health, and durability failures
  are explicit and cannot become selected evidence.
- `src/spark/bench.rs::tests::capability_simplicity_and_recipe_id_order_is_deterministic`
  — exact finite ordering and tie breaking are stable.
- `src/spark/recipe.rs::tests::any_fingerprint_change_invalidates_but_retains_audit`
  — model/image/host/driver/recipe/parser/objective changes all fall back.
- `src/spark/bench.rs::tests::cancellation_cleans_generation_without_promotion`
  — no partial evidence/cache promotion or orphan engine.
- Fake-engine E2E drives `bench → tune → serve`, proves winner selection and
  invalidation-to-vLLM behavior, raw persistence, and bounded queues.

**Real `dgx-spark` E2E gate:**

- Freeze every eligible candidate and run compatibility, health, semantic,
  model-identity, forbidden-route, resource, isolation, and restart checks.
  Explicitly record each unsupported or uninstalled engine family and why; do
  not make it launchable or download it merely to fill a matrix.
- Persist the complete functional result for each eligible candidate and prove
  deterministic capability/simplicity/recipe-ID ordering. If mistral.rs is
  compatible, evaluate it only as experimental with UI/download/media/file/
  code/shell/Python/agent/MCP/multi-model features disabled and tested; status
  alone cannot select it.
- Serve normally and prove the tuned winner is used. Invalidate a copy of its
  fingerprint in isolated state and prove normal serve visibly returns to the
  exact verified vLLM fallback. Preserve active real state and protected stack.

**Definition of Done:**

- [x] Compatibility/scoring/invalidation/cancellation tests pass and evidence is
      durable, complete, and reproducible.
- [x] Every real compatible candidate passes correctness and safety;
      unsupported/experimental states remain explicit and enforced.
- [x] The real `agent` winner satisfies every functional gate and the
      deterministic capability/simplicity/recipe-ID selection rule.
- [x] Normal serve uses a valid exact winner and otherwise the verified vLLM
      fallback—never an implicit engine/precision/revision change.

**Risks / unknowns:** lack of a second installed, locally verified engine may
leave vLLM as both fallback and only eligible result without weakening
acceptance. No unsupported family is downloaded or promoted to manufacture a
comparison.

---

## Step 15 — Harden release, upgrade/rollback, and full durability acceptance

**Goal:** close every operational, security, observability, supply-chain, and
recovery requirement and prove the complete non-disruptive journey on
`dgx-spark`. Docker restart and host reboot are optional operator maintenance
gates, recorded as not run unless separately authorized, and do not block this
step.

**Architecture slice:** finish side-by-side signed releases and commands for
`upgrade`/`rollback`; preflight exact changes, protected versions, active recipe
representability, N/N-1 schema compatibility, and verified online backup;
stage/fsync/validate, atomically switch `current`, restart only the two control
services, reconcile healthy engines without restarting them, semantically probe
every route, then commit or automatically restore symlink/units/DB backup. Add
SSH-only `cert rotate` leaf/CA rotation with overlap/re-pin,
`sy-spark.target`, final AppArmor or SELinux policy selected from detected
enforceable LSM, and systemd security
assertions. Complete structured/redacted journald/audit, authenticated
Prometheus metrics via the existing observability mechanism, bounded engine
logs, backup/corruption read-only recovery, and actionable status/doctor.
Release CI/build metadata includes ARM64 feature-minimal artifact, resolved
features/licenses/native/build-script/unsafe inventory, cargo-deny policy,
auditable metadata, signatures, recipe/fallback-wheel hashes, and OpenAPI. The
required real-host matrix validates non-disruptive functional process recovery;
Docker and host recovery appear only as separately authorized optional
maintenance evidence.

**Delivered CJM:** operator → upgrades the control plane while models are
serving, validates Codex/Claude use, recovers through control-service restarts,
optionally validates Docker/host recovery in an authorized maintenance window,
rolls back safely, rotates identity, and finally stops instances →
desired state and exact endpoints return only after health, no base-stack drift
or model loss occurs, and every action is diagnosable from stable CLI/JSON.

**Files:**

- `src/spark/install.rs` (modified) — `upgrade`, `rollback`, backup/schema/active-
  recipe gates, atomic switch/health/automatic rollback, cert rotation/re-pin,
  signed manifest, and protected post-assertion.
- `src/spark/cli.rs`, `src/spark/client.rs`, and `src/spark/wire.rs` (modified) —
  complete `upgrade`, `rollback`, and `cert rotate` dry-run/yes/JSON/env/exit-code
  contracts plus SSH maintenance transport and overlap manifests.
- `src/spark/agent.rs`, `src/spark/executor.rs`, and `src/spark/gateway.rs`
  (modified) — final recovery modes, hot cert/route reload, metrics, structured
  audit/log redaction, watchdog coverage, graceful shutdown/drain, and budgets.
- `configs/systemd/system/sy-spark.target` (new; SPEC-prescribed) and both
  `sy-spark-*.service` files (modified) — final ordering, restart backoff,
  credentials, hardening, resource limits, OOM scores, and recovery behavior.
- `configs/apparmor.d/sy-spark-*` and/or the selected `configs/selinux/sy-spark/*`
  assets (modified from Steps 2/4) — finalized repository-owned enforceable
  agent/executor boundary; installer manifests and rolls it back atomically.
- `deny.toml` and `.github/workflows/spark-release.yml` (new) — license/advisory/
  ban/source policy and signed ARM64/auditable/resolved-feature release gates.
- `tests/spark_release_e2e.rs` (new) — upgrade/rollback/corruption/recovery/
  redaction matrix; real reboot cases remain explicit ignored gates.
- `README.md:34-195,346-413` (modified) — architecture, install/recovery,
  complete command/API/client examples, safety policy, exit codes, and no-update
  promise.

**Tests:**

- Installer tests cover dry-run parity, failed stage, active-recipe incompatibility,
  N/N-1 schema, backup verification, switch crash points, semantic failure
  automatic rollback, explicit rollback blockers, cert overlap/re-pin, and
  protected-version mismatch.
- Systemd/LSM tests run syntax and explicit directive/rule assertions plus
  `systemd-analyze security`; agent cannot reach Docker, executor cannot bind
  Internet sockets, and both have only declared writable paths.
- Recovery E2E covers agent loss, executor loss, Docker loss, truncated WAL,
  corrupt DB read-only mode and verified-backup listing, missing/corrupt recipe,
  expired token/cert, healthy-engine preservation, and exact stopped intent.
- Observability/security tests cover log/event/metric cardinality and bounds,
  prompt/generated text/token/registry/header/path redaction, authenticated
  metrics, bounded log cursoring, and actionable problem remediation.
- Release tests cover OpenAPI drift, ordinary/minimal/ARM64 builds, cargo deny,
  auditable metadata, duplicate crypto/native/unsafe/build-script inventory,
  manifest/signature verification, and rollback artifact completeness.
- Resource tests enforce declared memory floors and bounded buffers; durability
  tests enforce SQLite committed-transition crash RPO.

**Real `dgx-spark` E2E gate:**

- With Ornith and embedding instances healthy and pinned Codex/Claude journeys
  passing, upgrade to a new signed release. Prove containers retain identity and
  restart count, gateway routes remain health-gated, state/recipes/guard/TLS
  reconcile, semantic probes pass, and protected fingerprints match. Inject one
  candidate health failure and prove automatic rollback without stopping a
  healthy engine; then perform an explicit safe rollback/forward upgrade.
- Rotate the leaf under the pinned CA and prove hot reload/overlap; exercise CA
  rotation only over SSH with explicit client re-pin and no private key leaving
  Spark. Test expired/revoked credentials via isolated copies.
- After immediate explicit maintenance-window confirmation, restart Docker and
  prove desired-running containers/routes return only after aggregate-safe
  restart and semantic health. After a second explicit confirmation, reboot the
  host and prove the same, including early executor guard, database/WAL recovery,
  identical instance IDs, and no route during warming. Absence of either test is
  recorded as not run, never as pass.
- Run one bounded final Codex/Claude/VLM/embedding functional matrix and assert
  no swap-in, thermal throttle, safety-floor breach, unbounded memory, or
  logical RPO.
- Validate the Rust/HTTP fallback shared cache with one immutable fixture in an
  isolated cache and retain the fallback unless a separately approved removal
  change proves it unnecessary. Stop selected instances, prove endpoints
  disappear and model/compile caches remain,
  clean only exact test-labeled objects, and capture the final protected-stack
  equality report.

**Definition of Done:**

- [x] Upgrade/rollback/cert/LSM/systemd/recovery/observability/release tests pass.
- [x] Non-disruptive process recovery proves desired services return only after
      health, stopped ones do not return, and healthy engines survive
      control-plane upgrade/rollback. Docker-restart and host-reboot evidence is
      recorded as pass only when separately authorized, otherwise as not run.
- [x] The full client/capability matrix, security negatives, resource safety,
      supply-chain checks, and protected-version assertions pass.
- [x] README, CLI help, OpenAPI, JSON schemas, exit codes, environment variables,
      installation/recovery procedures, and recipe evidence are complete.
- [x] No secret or prompt is present in artifacts, state, logs, metrics, Docker
      metadata, process arguments, or release manifests.

**Risks / unknowns:** Docker restart and host reboot are genuinely disruptive
optional maintenance gates. They require separate confirmation; absence is
recorded as not run and does not block completion.

---

## Requirement traceability

| SPEC contract | Owning roadmap steps |
|---|---|
| Workstation host profiles, SSH bootstrap, no arbitrary command | 1, 2, 15 |
| Direct pinned HTTPS, TLS/token lifecycle, rate/concurrency policy | 2, 3, 11–13, 15 |
| Split unprivileged agent/root executor and typed UDS | 2, 4, 15 |
| SQLite WAL/FULL desired state, operations, idempotency, backups | 3, 6, 8, 9, 14, 15 |
| Root-owned recipes, exact fingerprints, vLLM fallback | 5, 10, 14 |
| Native HF cache, verified resume/fallback, inventory/removal | 6, 15 |
| 8 GiB admission/emergency floors and 100 GiB disk reserve | 7–10, 13–15 |
| Internal shared bridge, managed containers, serve/ps/logs/stop | 8–10 |
| Durable desired-running reconciliation and restart suppression | 9, 15 |
| Real Ornith text/reasoning/tool serving | 10–12 |
| OpenAI Responses/Codex compatibility | 11, 13–15 |
| Anthropic Messages/Claude Code compatibility | 12–15 |
| Ornith vision and Qwen embeddings | 13–15 |
| Functional bench/tune objectives, candidates, caches, deterministic winner | 14, 15 |
| Atomic upgrade/rollback, LSM/systemd, observability, supply chain | 15 |
| Complete required CLI command and stable JSON/exit-code surface | 1–15 |

## Cross-cutting Definition of Done

- [x] All 15 step DoDs are checked with local tests and redacted real
      `dgx-spark` evidence; no skipped device gate is presented as complete.
- [x] The ordinary workstation binary remains lightweight, while the signed
      ARM64 `--no-default-features --features spark-agent` artifact contains the
      agent/executor stack and passes the full release inventory.
- [x] On a clean client profile and installed Spark, this complete journey works:

  ```text
  sy spark dgx-spark install --dry-run --json
  sy spark dgx-spark install --yes
  sy spark dgx-spark status --json
  sy spark dgx-spark doctor --json
  sy spark dgx-spark token list --json
  sy spark dgx-spark cert status --json
  sy spark dgx-spark download ornith-ai/Ornith-1.5-9B --alias ornith-1.5:9b
  sy spark dgx-spark download Qwen/Qwen3-Embedding-0.6B --alias qwen3-embedding:0.6b
  sy spark dgx-spark ls --json
  sy spark dgx-spark show ornith-1.5:9b --json
  sy spark dgx-spark recipes ornith-1.5:9b --json
  sy spark dgx-spark bench ornith-1.5:9b --json
  sy spark dgx-spark tune ornith-1.5:9b --objective agent --json
  sy spark dgx-spark serve ornith-1.5:9b --name ornith
  sy spark dgx-spark serve qwen3-embedding:0.6b --name embeddings
  sy spark dgx-spark ps --json
  sy spark dgx-spark client-config ornith --client codex --json
  sy spark dgx-spark client-config ornith --client claude-code --json
  sy spark dgx-spark operations --json
  sy spark dgx-spark logs ornith --json
  sy spark dgx-spark stop ornith
  sy spark dgx-spark stop embeddings
  sy spark dgx-spark ls --json
  ```

- [x] Exact pinned Codex and Claude Code binaries each complete deterministic
      streamed tool tasks; both public adapters pass the same local-image VLM
      fixture; the embedding route passes dimension/normalization/similarity
      fixtures.
- [x] `serve` chooses a valid tuned winner only for its exact full fingerprint;
      otherwise it visibly uses the exact verified vLLM fallback and never
      downloads, converts, tunes, or changes engine/model/precision implicitly.
- [x] Agent and executor recovery meet desired-state, route-readiness,
      restart-suppression, stop/retain, and zero logical RPO contracts. Optional
      Docker/host maintenance gates are recorded as pass or not run.
- [x] Buffer bounds, memory/swap/thermal safety, capability quality, and
      cache/fallback acceptance gates pass on the real Spark.
- [x] Every before/after report proves the DGX software build, OS/kernel, driver,
      CUDA/runtime, Docker, toolkit, firmware, system Python, and protected host
      configuration are unchanged.
- [x] `make lint`, `make test`, format, OpenAPI drift, cargo deny, ARM64/minimal
      builds, auditable metadata, dependency/native/unsafe inventory, unit/LSM
      checks, documentation, and signed release manifest all pass.

## Out of Scope

- Arbitrary remote shell/command execution, Docker API/CLI passthrough,
  caller-controlled image/argv/environment/mount/device/path/URL, and adding any
  human or agent identity to the Docker group.
- DGX OS/kernel/driver/CUDA/runtime/Docker/toolkit/firmware/system-Python update,
  automatic firewall/sysctl/swap/THP/governor/clock/power/bootloader change, or
  automatic reboot.
- Ollama HTTP compatibility or MCP. The lifecycle CLI is Ollama-shaped; remote
  inference protocols are OpenAI Responses and Anthropic Messages.
- Multi-host scheduling, distributed training, model fine-tuning, implicit
  conversion/quantization, or public-Internet serving.
- Image/video generation, speech/audio serving, or other media generation.
  Vision here means image understanding; embeddings are text embeddings.
- An embedded inference library in the agent/executor. vLLM, TensorRT-LLM,
  SGLang, llama.cpp, NIM, mistral.rs, and any future Rust engine remain isolated,
  exact, digest-pinned OCI recipes that must pass the same real-host gates.
