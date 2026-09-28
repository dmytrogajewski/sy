# JOURNEY-spark-qualification-jobs: Run signed GPU qualification jobs safely

## Roadmap Link

- Source roadmap: Sparky `specs/roadmaps/ROADMAP-sparkflash-engine.md`, Step 49 prerequisite
- Feature: Typed, content-addressed DGX Spark qualification jobs

## 1. Journey

When **an engine developer qualifying a specific Sparky CUDA gate on an installed DGX Spark** I want to **submit a release-authority-signed immutable job manifest through `sy spark`** so I can **obtain durable, identity-bound target evidence without gaining a remote shell or disturbing managed inference engines**.

## 2. CJM

The developer has a signed OCI qualification bundle and needs target evidence from the real Spark. Today the Spark plane deliberately exposes no arbitrary file, executable, argv, or Docker surface. This feature preserves that boundary: the operator selects a signed manifest, previews the exact admitted identity, approves one typed operation, and retrieves bounded hashed output through the existing operation record.

### Phase 1: Inspect and approve

**User Intent:** Prove exactly which signed job and target would run before any mutation.

**Actions:** Run `sy spark HOST qualify --manifest FILE --signature FILE --dry-run --json`, then compare the manifest, image, operation, target, and host identities in the plan.

**Pain / Risk:** A stale manifest may target another build; a signature may not match the installed release authority; malformed or oversized material may exhaust the agent; an operator may accidentally supply both preview and approval.

**Success Signal:** The JSON plan reports an admitted, signature-verified manifest hash and exact target identity while Docker remains untouched.

### Phase 2: Execute in isolation

**User Intent:** Run the approved qualification without changing any managed engine.

**Actions:** Repeat the command with `--yes`; optionally detach and follow the returned durable operation.

**Pain / Risk:** An unsigned manifest may inject an image or operation; a job may request the wrong GPU or host; the container may gain network, writable root, privileges, capabilities, excessive resources, or a restart policy; image startup may fail after container creation.

**Success Signal:** The executor independently verifies the signed manifest, admits the configured target, starts only the fixed one-shot qualification operation with enforced resource and isolation bounds, and leaves all managed engines unchanged.

### Phase 3: Inspect, cancel, and clean up

**User Intent:** Retrieve mechanically attributable evidence or stop a stuck job safely.

**Actions:** Follow the operation, inspect its result, or use `operations cancel` with the operation identifier.

**Pain / Risk:** Cancellation may race executor registration; timeout or output overflow may orphan a container; invalid UTF-8 may corrupt transport; a non-zero exit may be mistaken for successful qualification; cleanup may target another container.

**Success Signal:** The terminal operation records immutable manifest/image/output hashes and exact exit outcome; every completion, failure, cancellation, timeout, and output-limit path removes only the exact qualification container.

### Friction and Opportunity

| Friction | Phase | Opportunity |
|----------|-------|-------------|
| No safe remote execution surface | Inspect | Admit only manifests signed by the installed release authority |
| Docker authority is root-only | Execute | Reuse typed agent-to-executor IPC with independent verification |
| Target evidence can be fabricated or misattributed | Inspect/result | Bind manifest, image, host, target, operation, and output hashes |
| GPU jobs can wedge or leak resources | Execute/cleanup | Enforce one-shot lifecycle, hard limits, cancellation, and exact cleanup |
| Existing inference is production state | Execute | Use qualification-only labels/network and never lifecycle-manage engine containers |

### North Star Summary

One public command previews and runs one immutable signed qualification operation. Neither the laptop nor the network-facing agent can supply executable text or argv. The root executor independently validates identity and policy, isolates the GPU container, records bounded hashed output, and transactionally removes it without entering the managed-engine lifecycle.

## 3. UX Implementation and Assessment

### Time to First Value
- [x] One dry-run command returns the complete admission identity.
- [x] The approved command returns a durable operation identifier immediately or follows it.

### Onboarding Clarity
- [x] `sy spark HOST qualify --help` documents the signed inputs and examples.
- [x] Signature, identity, target, and resource errors name the failed invariant.

### Production-Ready Defaults
- [x] Mutation requires `--yes`; preview requires `--dry-run`.
- [x] Isolation and resource bounds come from signed policy plus executor hard ceilings.

### Golden Path Quality
- [x] Fake-runtime daemon/CLI e2e proves preview, execution, and terminal result.
- [x] Result contains exact input, target, output, and exit identities.

### Decision Load
- [x] The caller chooses only signed manifest/signature paths and detach behavior.
- [x] The caller cannot choose an image, executable, argv, mount, network, privilege, or limit.

### Progressive Complexity
- [x] The normal path is dry-run followed by `--yes`.
- [x] Existing operation follow/cancel surfaces provide advanced control.

### Error Quality
- [x] Hostile signature, schema, digest, target, and resource inputs fail closed.
- [x] Runtime failures retain an attributable terminal operation.

### Failure Safety
- [x] Every runtime terminal path cleans the exact qualification container.
- [x] Managed engines are outside the qualification container namespace and labels.

### Runtime Transparency
- [x] Operation progress exposes execution, and no terminal state is emitted before cleanup acknowledgement.
- [x] Output is bounded, encoded losslessly, byte-counted, and hashed.

### Debuggability
- [x] Manifest and image hashes make every result reproducible.
- [x] Exit code/outcome and stdout/stderr hashes remain inspectable.

### Cross-Surface Consistency
- [x] Human and JSON CLI use the same HTTPS operation.
- [x] Cancellation uses the existing durable operations API.

### Workflow Consistency
- [x] Requests require idempotency and use the existing operation record.
- [x] Agent and executor both reject unknown fields and untrusted manifests.

### Change Safety
- [x] Dry-run reaches executor admission without starting Docker work.
- [x] Approval cannot silently substitute a different manifest identity.

### Experimentation Safety
- [x] Qualification containers have no network and a read-only root.
- [x] Jobs are one-shot with no restart policy.

### Interaction Latency
- [x] Admission avoids image start and returns bounded JSON.
- [x] Detached execution returns before the qualification completes.

### Developer Feedback Speed
- [x] Focused unit and fake-runtime e2e tests cover each policy branch.
- [x] Operation progress and terminal result expose the failure boundary.

### Team Scale
- [x] Signed manifests and OCI image digests can be reviewed and shared.
- [x] Fixed operation identifiers prevent per-user command drift.

### System Scale
- [x] A bounded operation enum can add reviewed runners without an argv surface.
- [x] Exact QSA qualification has a dedicated finite operation and fixed arguments.
- [x] Output and concurrency remain bounded independently of manifest size.

### Right Behavior by Default
- [x] Missing signature, authority key, or target evidence rejects the job.
- [x] Cancellation and failure clean up before reporting terminal state.

### Anti-Bypass Design
- [x] Executor independently verifies signature, schema, identity, and bounds.
- [x] No shell, executable path, argv, mount, network, privilege, or engine lifecycle field exists on the public request.

## 4. Tests

### TC-01: Signed preview is non-mutating

**Given** a manifest signed by the installed release authority.
**When** the operator requests a dry-run.
**Then** the exact identity is admitted and the container runtime receives no start call.

### TC-02: Unsigned and altered manifests fail closed

**Given** an invalid signature or a byte changed after signing.
**When** preview or execution is requested.
**Then** both agent and executor reject before runtime mutation.

### TC-03: Caller cannot inject runtime behavior

**Given** hostile unknown manifest or request fields.
**When** they are decoded.
**Then** schema validation rejects them; no executable or argv field is accepted.

### TC-04: Target and resource admission are exact

**Given** a signed manifest for the wrong architecture, GPU, compute capability, host fingerprint, or excessive resource bound.
**When** executor admission runs.
**Then** it rejects before image or container operations.

### TC-05: Runtime isolation is enforced

**Given** an admitted manifest.
**When** its container spec is constructed.
**Then** it names the exact digest, GPU device, fixed runner operation, no network, read-only root, no privileges/capabilities, hard memory/PID/CPU/output/deadline limits, and no restart.

### TC-06: Successful result is attributable

**Given** deterministic stdout, stderr, and exit zero from the fake runtime.
**When** the durable operation completes.
**Then** result hashes and byte counts match the exact bytes and the outcome is `passed`.

### TC-07: Non-zero exit cannot pass

**Given** a fixed runner that exits non-zero.
**When** execution finishes.
**Then** the operation fails with the exact exit code and hashed output retained.

### TC-08: Cancellation is transactional

**Given** a running qualification operation.
**When** the existing operations cancellation endpoint is invoked.
**Then** the executor cancellation token fires, the exact container is stopped and removed, and the operation becomes cancelled.

### TC-09: Timeout and output overflow are transactional

**Given** a runner that exceeds its signed deadline or output cap.
**When** the bound is crossed.
**Then** the exact container is stopped and removed, no result is promoted as passed, and the failure is durable.

### TC-10: Managed engines are preserved

**Given** existing managed engine observations in the fake runtime.
**When** qualification succeeds, fails, or is cancelled.
**Then** no engine prepare/start/stop/restart/reconcile method is invoked and engine state is byte-for-byte unchanged.

## Traceability

### SparkFlash CUDA lifecycle qualification extension

The finite `spark_flash_cuda_lifecycle_v1` operation selects only
`/opt/sparky/bin/sparky-qualification cuda-lifecycle-target --json`. It uses the
precompiled no-JIT isolation and the existing signed manifest, fresh resource
admission, exclusive qualification slot, transition lease, bounded output and
acknowledged cleanup. No caller-selected program, argument, fault or cycle count
crosses the control boundary. The signed runner owns those bounded cases.

Public CLI, signed daemon and exact container-spec tests cover the new variant.
These deterministic tests do not attest GPU execution or actual poison exit.

### SparkFlash kernel catalog qualification extension

Sparky `specs/sparkflash-engine/ROADMAP.md` Step 15 requires its own finite
`spark_flash_kernel_catalog_v1` operation, fixed to the qualification entrypoint
and `kernel-catalog-target --json`. The precompiled loader smoke uses no JIT
environment or executable cache; general temporary storage remains no-exec.
Acceptance requires fixed wire/command tests, unchanged isolation, signed daemon
preview/execution, public CLI transport, and default plus headless release gates.
Stressors: unknown mode, changed signature, stale host, mutable image, memory
pressure, disk pressure, concurrent lease, argv injection, JIT escalation,
output overflow, failed cleanup, and protected-instance drift remain fail-closed.

### SparkFlash MoE qualification extension

Step 29 requires an isolated installed-runtime backend capture and native MoE
execution. Add only `spark_flash_moe_v1`, mapped to the fixed qualification
entrypoint with `moe-target --json`; reuse signed target admission, exclusive
GPU lease, bounded JIT cache, non-root isolation, and exact cleanup. No shell,
caller-defined arguments, mount, or managed-service access is added.

Acceptance: signed MoE preview and execution traverse the public daemon and
executor; fixed-command and bounded-cache tests pass; all existing rejection,
resource, cancellation, and cleanup gates remain green. Deployment uses the
signed public release path with protected-service identity preserved. Backend
precision is an output of the isolated reference capture, not an assumption.

Stressors: unknown operation, altered signature, stale host fingerprint,
mutable image, insufficient memory, insufficient disk, concurrent GPU lease,
unbounded JIT output, executable injection, failed startup, cancellation race,
and protected-engine identity drift retain their existing fail-closed behavior.

- Roadmap item: Sparky Roadmap Step 49 shared prerequisite
- Implementation files: `src/spark/qualification.rs`, `src/spark/cli.rs`,
  `src/spark/client.rs`, `src/spark/agent.rs`, `src/spark/executor.rs`, and
  `src/spark/mod.rs`; normalized API contract:
  `specs/openapi/sy-spark-control-v1.json`
- Test files: qualification-focused unit and fake-daemon tests in the modules
  above, plus `tests/spark_qualification_e2e.rs`
- User documentation: `docs/reference/cli.md`, `docs/reference/spark.md`, and
  `docs/how-to/develop-spark.md`

## Step 18 stable-memory qualification implementation

The finite `spark_flash_memory_admission_v1` wire value maps only to
`/opt/sparky/bin/sparky-qualification memory-admission-target --json`. The
precompiled operation retains signed host admission, exclusive high-memory
lease, no-network/read-only/non-root/cap-drop isolation, fixed resource/output
bounds, and acknowledged exact-container cleanup. No resource policy broadens.
The existing public CLI scenario first rejected the new response as incompatible
JSON; its regression now includes memory, as do signed fake-daemon preview/yes
and exact container-isolation checks. These host scenarios do not attest GPU
execution. Modified: qualification enum, executor/agent tests, public CLI test,
Makefile and qualification documentation. OpenAPI derives the enum from source;
its normalized fixture lists schema names only and needs no change.

## Step 19 graph-catalog qualification extension

The finite `spark_flash_graph_catalog_v1` wire value maps only to
`/opt/sparky/bin/sparky-qualification graph-catalog-target --json`. It retains
the signed host checks, exclusive high-memory lease, precompiled isolation,
watchdog, acknowledged cleanup, and existing resource/output bounds.
The public CLI scenario reproduced `Spark returned an incompatible JSON document`
before this variant was implemented. The same regression now includes graph
catalog, alongside signed daemon preview/execution and exact executor-isolation
checks. These host fixtures do not attest GPU execution or graph correctness.

## Step 20 session qualification extension

The finite `spark_flash_session_v1` wire value maps only to
`/opt/sparky/bin/sparky-qualification session-target --json`. It retains signed
host admission, exclusive high-memory lease, precompiled isolation, watchdog,
bounded output, and acknowledged exact-container cleanup. No policy broadens.
The public CLI first rejected the new value as incompatible JSON; its existing
regression now includes session, as do signed daemon preview/execution and exact
container-spec assertions. These host fixtures do not attest GPU transactions.
Stressors remain fail-closed: unknown operation, altered signature, stale host,
mutable image, memory pressure, disk pressure, concurrent lease, injected argv,
JIT escalation, output overflow, failed cleanup, and protected-instance drift.
Evidence: `target/march-step20-session-operation.a8CD12` retains the public red
and green replay, focused signed/isolated tests, and successful default/headless
lint/test/fmt gates. The default gate exposed a separate crash-fixture selector
defect, repaired under [BUG-plugin-crash-fixture-ownership](../bugs/BUG-plugin-crash-fixture-ownership.md).

## Checkpoint model-prefix qualification extension

Add only `spark_flash_model_prefix_v1`, fixed to
`/opt/sparky/bin/sparky-qualification model-prefix-target --json`. The signed
image owns the checkpoint-prefix scenario; neither arguments nor a selectable
mode cross the public boundary. Preserve existing signature, target/resource
admission, exclusive lease, no-JIT isolation, watchdog and acknowledged cleanup.

Acceptance: public CLI transport, signed fake-daemon preview/execution, exact
executor spec and closed-schema regressions pass in headless and default builds.
Unknown operations, injected arguments, signature drift and existing resource/
cleanup failures stay rejected. This extension does not qualify model numerics
or GPU execution. Deploy matching client/agent support before submitting this
new operation; no upgrade or workload mutation is part of the source change.

The public CLI reproduced `Spark returned an incompatible JSON document` before
the four-line enum/mapping change, then passed. The signed fake-daemon scenario,
whole-container-spec equality, closed wire representation and generated OpenAPI
enum regressions also pass. `make test-spark-qualification`, `make test-spark`,
`make lint-spark`, default Spark client tests, `make lint`, `make test` and
`cargo fmt --all -- --check` pass. Raw host logs are retained under
`target/model-prefix-operation.gOzldb`; existing manual-hardware ignores are
unchanged. The normalized OpenAPI fixture contains names only and needs no edit.
