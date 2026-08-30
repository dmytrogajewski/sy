# ROADMAP: qwen38-flash-next-sglang

Source: `specs/journeys/JOURNEY-20260828-2340.md`

Research: `specs/research/qwen38-flash-next-dgx-spark-performance/SPEC.md`

## Overview

Qualify a pinned SGLang engine for the already installed Qwen3.8-Flash-Next NVFP4 checkpoint on one DGX Spark, without replacing the current vLLM control before paired evidence exists. The end state is a generic declarative engine policy, a reproducible ARM64/SM121 image, persistent verified PLE mmap storage, complete existing gateway protocols, and a real-device comparison that leaves the final preferred-engine priority to one explicit user decision.

No step updates or tunes the DGX host OS, firmware, kernel, NVIDIA driver, CUDA installation, clocks, power, swap, or boot configuration. No step introduces model- or engine-name dispatch in Rust.

## Step 1 — Freeze the SGLang candidate supply chain

**Goal:** Define one reproducible ARM64 image input using the exact day-zero SGLang image, single-Spark recipe revision, and content-addressed patches.

**Files:** `configs/sy/spark/engines/sglang-qwen38-mmap.Dockerfile` (new), `tests/spark_release_catalog_boundary.rs` (modified)

**Tests:**

- `tests/spark_release_catalog_boundary.rs::engine_image_inputs_are_immutable` — discovers every engine Dockerfile and proves each base image is digest-pinned with no `latest` tag, without engine/model names or operational values in test code.
- Docker build syntax check — proves the pinned Dockerfile parses before a long ARM64 build begins.

**Definition of Done:**

- [x] Base manifest is `sha256:12d3392bdc8be8d35e9a95f191df6aef99c5114bdbefd41bfdc7e760e6d25ec1` with ARM64 child `sha256:14ed582518584c5c830206b5318a2c2769e68229c3422e48a28b952b3a888bd4`.
- [x] Recipe source is pinned to `04d073518ded5d0db1cddce74d9afb1cdca5eddc` and every imported file has a checked digest.
- [x] OCI source/revision and SM121 labels are present.
- [x] Tests above pass and `make lint` is green.
- [x] No host package or service change is introduced.

**Risks / unknowns:** The published tag is mutable; only the captured ARM64 child digest is acceptable.

## Step 2 — Land the file-backed PLE mmap transform

**Goal:** Replace SGLang's pinned-host PLE allocation with the recipe's exact file-backed mapping while preserving its gather and dequantization math.

**Files:** `configs/sy/spark/engines/sglang-qwen38-mmap.Dockerfile` (modified), content-addressed assets under `configs/sy/spark/engines/patches/`, `tests/spark_engine_image_contract.rs` (new)

**Tests:**

- Generic image-contract validation discovers the configured PLE patch, verifies its declared SHA-256, and fails if the upstream allocation anchor changes; Rust tests do not duplicate model-specific values.
- Image build-time AST/import check — proves patched `qwen4_exp.py` remains valid and contains exactly one mmap allocator.
- Upstream mmap gather/write checks inside the image — verify single row, batch, range failure, tensor view, and FP8 dequantization behavior before publication.

**Definition of Done:**

- [x] PLE is file-backed only when the declared mmap directory is present; no ordinary pinned-host fallback is selected by the Spark profile.
- [x] `MADV_RANDOM` is applied as a performance hint without becoming a correctness dependency.
- [x] The existing Triton gather, prefetch stream, and CUDA-graph-compatible pointer path remain intact.
- [x] Tests above pass and `make lint` is green.

**Risks / unknowns:** A fluent semantic probe cannot prove bit-exact PLE data; build-time numeric checks are mandatory.

## Step 3 — Enable the native SM121 QSA decode backend

**Goal:** Widen only the architecture gate required to use SGLang's TRTLLM sparse decode path on GB10 while retaining Triton prefill.

**Files:** `configs/sy/spark/engines/sglang-qwen38-mmap.Dockerfile` (modified), content-addressed assets under `configs/sy/spark/engines/patches/`, `tests/spark_engine_image_contract.rs` (modified)

**Tests:**

- Generic image-contract validation discovers the configured QSA patch and verifies its content address; the image definition owns exact upstream-anchor and postcondition checks.
- Image build-time import check — resolves `is_sm120_supported` and the TRTLLM sparse decode function without importing an unsupported prefill path.

**Definition of Done:**

- [x] Prefill remains explicitly Triton and decode explicitly `trtllm_mha`.
- [x] The patch changes the SM gate only and fails closed when upstream context differs.
- [x] Experimental `qsa_ring_width.py` is absent from the image and release inputs.
- [x] Tests above pass and `make lint` is green.

**Risks / unknowns:** FlashInfer or SGLang package movement can relocate the target; the build must reject rather than guess a new path.

## Step 4 — Make the transformed PLE artifact durable

**Goal:** Populate the 47.7 GiB PLE backing file once, bind it to the exact checkpoint and transform, then reuse it without a full rewrite on warm starts.

**Files:** `configs/sy/spark/engines/sglang-qwen38-ple-persist.py` (new), `configs/sy/spark/engines/sglang-qwen38-mmap.Dockerfile` (modified), `tests/spark_sglang_image_contract.rs` (modified)

**Tests:**

- Generic image-contract validation discovers the configured persistence transformer and verifies its content address; the image self-test owns its data-shape and publication semantics.
- Image self-test with a small synthetic table — exercises temporary creation, complete verification, atomic publish, read-only reuse, cancellation cleanup, and corruption regeneration.

**Definition of Done:**

- [x] First population writes a temporary artifact and publishes only after verification.
- [x] Warm start maps a completed source-bound artifact without copying every PLE shard again.
- [x] Interrupted or corrupt state is never trusted and is recoverable without deleting the model snapshot.
- [x] Runtime logs distinguish `created`, `verified`, `reused`, and `rejected` without exposing model contents.
- [x] Tests above pass and `make lint` is green.

**Risks / unknowns:** Mapping the final artifact read-only may require a small source-local allocator change because `torch.from_file(shared=True)` normally opens a writable shared mapping.

## Step 5 — Make image qualification fail at build time

**Goal:** Ensure the custom image cannot be produced when its patches, imports, architecture contract, non-root paths, or launch module are invalid.

**Files:** `configs/sy/spark/engines/sglang-qwen38-mmap.Dockerfile` (modified), `tests/spark_sglang_image_contract.rs` (modified)

**Tests:**

- Catalog boundary validation discovers every contracted image and engine profile, then checks generic offline, non-root, writable-cache, entrypoint, and private-network invariants without naming an engine or model.
- Docker image self-test — imports SGLang/Qwen4-exp modules, reports CUDA architecture support, parses the final launch argv, and runs synthetic PLE tests without loading the real model.

**Definition of Done:**

- [x] Every patch is applied during image construction, never by mutating site-packages at container start.
- [x] Final runtime has no patch downloader, package manager, mutable source checkout, or undeclared build tooling; only its OCI-declared offline JIT toolchain remains.
- [x] The image can run as UID 65534 with only declared writable mounts.
- [x] Tests above pass and `make lint` is green.

**Risks / unknowns:** Some base-image initialization may assume root-owned `$HOME`; the final profile must prove its actual non-root launch path. Import-only checks cannot establish that runtime compilers are removable, so real-device qualification must execute the declared JIT paths before narrowing the toolchain.

## Step 6 — Declare the SGLang long-context engine profile

**Goal:** Add one ordinary engine TOML containing all runtime arguments, resources, routes, health, parsers, sampling, and isolation policy.

**Files:** `configs/sy/spark/engines/sglang-qwen38-mmap.toml` (new), `tests/spark_release_catalog_boundary.rs` (modified)

**Tests:**

- Generic catalog contract validation discovers every engine TOML and proves that matcher profiles, runtime arguments, resources, routes, health, sampling, and isolation policy parse from configuration.
- Production/config boundary validation derives engine and model identifiers from shipped configuration and fails if production Rust uses one for dispatch.

**Definition of Done:**

- [x] SGLang initially has lower priority than the deployed vLLM control and matches the same existing artifact profile only through generic traits.
- [x] The profile declares Triton prefill, TRTLLM decode, `modelopt_fp4`, PLE mmap, language-only mode, radix `extra_buffer`, MTP NEXTN 3/4/top-k-one/unquantized draft, and explicit Qwen parsers.
- [x] Resource values include image, persistent PLE/compile storage, startup, steady state, shared memory, PID, and startup deadline.
- [x] The engine binds only internal health and OpenAI-compatible routes through `sy-spark-internal`.
- [x] Tests above pass and `make lint` is green; no operational value is duplicated in Rust test code.

**Risks / unknowns:** Exact steady/startup envelopes must be replaced with measured Spark values before final selection.

## Step 7 — Preserve generic catalog packaging and reversible selection

**Goal:** Ship the additional TOML automatically through the existing signed inventory and prove vLLM remains the selected control until an explicit priority decision.

**Files:** `scripts/package-spark-release.sh` (modified only if a generic defect is found), `src/spark/install.rs` (tests only), `tests/spark_release_catalog_boundary.rs` (modified)

**Tests:**

- `tests/spark_release_catalog_boundary.rs::all_engine_tomls_are_signed_without_family_names` — packages a fixture release and verifies every TOML is hashed without listing engine filenames in Rust or shell.
- `src/spark/install.rs::tests::multi_engine_inventory_activates_and_rolls_back_atomically` — proves a new engine file is installed from the signed inventory and the prior catalog remains recoverable.
- A configuration-derived selection fixture verifies deterministic generic priority selection without embedding candidate names or priorities in Rust.

**Definition of Done:**

- [x] Packaging remains directory-driven; no SGLang filename is added to production installer or package logic.
- [x] The signed release validates both engine declarations without ambiguity.
- [x] Candidate qualification can use a temporary signed priority change and restore the previous release afterward.
- [x] Tests above pass and `make lint` is green.

**Risks / unknowns:** A temporary candidate release must not become the durable preferred release before Step 15.

## Step 8 — Build and import the exact ARM64/SM121 image

**Goal:** Build the candidate on the real Spark, retain reproducibility evidence, and replace placeholder identity/resource values with measured facts.

**Files:** `configs/sy/spark/engines/sglang-qwen38-mmap.toml` (modified), `specs/runs/qwen38-sglang-qualification.md` (new), `configs/sy/spark/engines/sglang-qwen38-mmap.Dockerfile` (modified only for reproduced defects)

**Tests:**

- Real-device image self-test — verifies architecture `aarch64`/SM121, imports, patch identities, non-root UID, offline launch dependencies, and PLE synthetic checks.
- Rebuild comparison — records Docker content identity, installed package freeze, OCI labels, layer inventory, and SBOM; unexplained digest drift fails the step.

**Definition of Done:**

- [x] The local image content digest is recorded in engine TOML and the run document.
- [x] Exact source, base image, patch hashes, package freeze, build command, and SBOM are retained.
- [x] Image size and build-time temporary disk use preserve the configured 100 GiB disk reserve.
- [x] No Spark host software is updated.
- [x] `make lint && make test` remains green after recorded identities change.

**Risks / unknowns:** A source image may contain architecture-specific binary wheels not represented by Python package metadata; import and first-kernel tests on GB10 are required.

## Step 9 — Serve the candidate through the real sy lifecycle

**Goal:** Start SGLang via the same signed admission, executor, internal network, health, semantic probe, and instance state machine used by every engine.

**Files:** `configs/sy/spark/engines/sglang-qwen38-mmap.toml` (measured resource updates), `specs/runs/qwen38-sglang-qualification.md` (modified), `tests/spark_reconcile_e2e.rs` (modified only for a generic lifecycle defect)

**Tests:**

- `sy spark dgx-spark serve qwen3.8:flash-next` under the temporary candidate-priority release — reaches `Healthy` through HTTP and one-token semantic readiness.
- Stop/start/reuse/cancel sequence — verifies one managed generation, exact image/model/engine fingerprints, bounded cancellation, verified PLE reuse, and no orphan container.
- Resource observation — records cold/warm startup phases, available memory, full PSI, swap delta, disk, restart count, and the 8 GiB floor.

**Definition of Done:**

- [x] Startup succeeds non-root on `sy-spark-internal` with no public direct port and no runtime network download.
- [x] First and warm startup are measured separately; warm start performs no full PLE rewrite.
- [x] The candidate remains healthy after one real generation and one stop/start cycle.
- [x] Measured startup/steady envelopes replace estimates in TOML.
- [x] The previous signed vLLM release is restored after the bounded candidate check.
- [x] `make lint && make test` is green.

**Risks / unknowns:** At 0.79 the engine may report an artificial SGLang memory shortfall even when Linux has free memory; adjust only declarative resource/profile values backed by measurements.

## Step 10 — Complete protocol qualification through the gateway

**Goal:** Prove SGLang produces the exact OpenAI and Anthropic semantics already promised to Codex, Claude, and OpenCode.

**Files:** `tests/spark_openai_e2e.rs` (modified), `tests/spark_anthropic_e2e.rs` (modified), `tests/spark_vllm_e2e.rs` (renamed/generalized only if its assertions are engine-neutral), `specs/runs/qwen38-sglang-qualification.md` (modified)

**Tests:**

- OpenAI Chat and Responses, streaming and non-streaming — reasoning deltas, text, usage, finish reason, cancellation, follow-up, and one/two tool calls.
- Anthropic Messages, streaming and non-streaming — thinking, signatures, text blocks, tool-use/input JSON, tool-result continuation, usage, stop reason, and token counting.
- Post-cancellation health request — proves the engine and gateway remain usable.

**Definition of Done:**

- [x] All protocol cases pass against the real SGLang route through authenticated sy HTTPS.
- [x] Streamed output is semantically equal to non-streaming output for deterministic fixtures.
- [x] Reasoning remains enabled and separate from final content.
- [x] No SGLang-specific protocol branch is added to the gateway.
- [x] `make lint && make test` is green.

**Risks / unknowns:** SGLang may emit subtle tool or usage differences; fix generic upstream normalization only when the documented wire contract requires it.

## Step 11 — Add an engine-neutral paired benchmark harness

**Goal:** Capture comparable TTFT, prefill, decode, MTP, prefix reuse, resource, and lifecycle evidence without adding noise to ordinary `ls` or `ps` output.

**Files:** `scripts/benchmark-spark-engine.py` (new), `tests/fixtures/spark-benchmark/` (new fixtures), `specs/runs/qwen38-sglang-qualification.md` (modified)

**Tests:**

- Harness fixture server test — parses SSE fragmentation, reasoning, usage, tool calls, cancellation, and terminal frames into deterministic JSON.
- Metric validation test — rejects missing model/engine fingerprints, mixed sampling, non-monotonic timing, inconsistent token counts, and unpaired runs.
- Dry fixture run — emits stable JSON without contacting Spark.

**Definition of Done:**

- [x] One command accepts an authenticated base URL plus immutable run metadata and emits raw samples and summary JSON.
- [x] Prompts, sampling, output length, warmup count, and ten measured samples are fixed in versioned fixtures.
- [x] Code, prose, reasoning/tool, cold-prefix, and growing-prefix workloads remain separately reported.
- [x] Secrets and generated model content are redacted from retained evidence.
- [x] `make lint && make test` is green.

**Risks / unknowns:** Engine scheduler log windows are not request metrics; client timings and engine counters must be labeled separately rather than merged.

## Step 12 — Run the paired short-context performance comparison

**Goal:** Measure vLLM and SGLang on identical code, prose, reasoning/tool, and cold-prefill workloads and reject a synthetic-only improvement.

**Files:** `specs/runs/qwen38-sglang-qualification.md` (modified), `configs/sy/spark/engines/sglang-qwen38-mmap.toml` (modified only for evidence-backed profile corrections)

**Tests:**

- One warmup plus ten 400-token samples per workload for each engine, using the same checkpoint, gateway, prompts, sampling, output limits, and otherwise idle Spark.
- MTP acceptance and no-MTP diagnostic A/B — confirms the chosen 3/4 profile improves useful throughput without corrupting prose, reasoning, or tools.
- Cold 8K/32K/128K prompt runs — compare TTFT and prefill without prefix contamination.

**Definition of Done:**

- [ ] SGLang median code decode is at least 35 tokens/s and at least 30% above paired vLLM.
- [x] Prose and reasoning decode are no more than 10% below paired vLLM.
- [ ] Cold TTFT is no more than 15% worse at 8K, 32K, and 128K.
- [ ] Every result includes range, median, p95 TTFT, prompt/decode throughput, MTP acceptance, memory, and engine identity.
- [x] Profile changes are configuration-only and the complete paired run is repeated after any change.

**Risks / unknowns:** Published 41.5 tokens/s is a five-sample code median and may not reproduce under sy's gateway or prompt distribution.

## Step 13 — Verify growing-prefix reuse and native long context

**Goal:** Demonstrate that the main agent benefit—safe reuse of growing prefixes—works through sy up to the model's native context without exhausting the appliance.

**Files:** `scripts/benchmark-spark-engine.py` (modified), `tests/fixtures/spark-benchmark/` (modified), `specs/runs/qwen38-sglang-qualification.md` (modified)

**Tests:**

- Ten-turn growing-prefix traces ending at 8K, 32K, 64K, and 128K — record reused tokens, per-turn TTFT, correctness, MTP acceptance, and health.
- One 240K disjoint-corpus needle retrieval and one cached suffix turn — verify answer, 262K capacity, TTFT, decode, memory floor, and post-run health.
- Concurrency one/two/four at 8K only — record aggregate/per-request behavior without making full-context concurrency a requirement.

**Definition of Done:**

- [ ] Cached growing-prefix TTFT improves by at least 80% at 32K, 64K, and 128K.
- [ ] The 240K needle and cached suffix both answer correctly without crash, restart, OOM, quarantine, or malformed stream.
- [ ] Available memory never crosses 8 GiB and the bounded run causes no sustained swap growth.
- [ ] No exact-replay-only cache claim is used as evidence.
- [ ] The previous signed vLLM release is restored after the candidate run.

**Risks / unknowns:** Several consecutive cold 240K prefills made the public recipe's host unresponsive; this roadmap performs one bounded retrieval and does not chain cold full-context runs.

## Step 14 — Complete fresh Codex, Claude, and OpenCode journeys

**Goal:** Prove the optimized engine improves real coding-agent work rather than only benchmark prompts.

**Files:** `specs/runs/qwen38-sglang-qualification.md` (modified), `README.md` (modified only after journeys pass), `docs/reference/spark.md` (modified only after journeys pass)

**Tests:**

- Fresh Codex Tetris run under `~/sources/testbed/qwen38-{engine}-codex` for both engines.
- Fresh Claude Tetris run under `~/sources/testbed/qwen38-{engine}-claude` for both engines.
- Fresh OpenCode Tetris run under `~/sources/testbed/qwen38-{engine}-opencode` for both engines.
- Browser verification — controls, rotation, collision, scoring, line clearing, game over, and restart.

**Definition of Done:**

- [ ] All six agent runs start without prior session state and use the exact same task and checkpoint.
- [ ] All three SGLang results are runnable and pass browser behavior checks.
- [ ] At least two SGLang journeys finish at least 25% faster than their paired vLLM runs.
- [ ] Per-turn TTFT, cache reuse, decode, tools, retries, protocol errors, wall time, and engine health are retained.
- [ ] Reasoning is enabled for every agent; no client-specific engine workaround is introduced.
- [ ] The previous signed vLLM release is restored after the candidate runs.

**Risks / unknowns:** Agent wall time contains tool execution and client behavior; report engine and journey timing together but do not conflate them.

## Step 15 — Present evidence and apply the selected declarative priority

**Goal:** Give the user one complete vLLM/SGLang comparison, ask the only remaining decision, and encode the answer in configuration and documentation.

**Files:** `configs/sy/spark/engines/sglang-qwen38-mmap.toml` (modified if selected), `configs/sy/spark/engines/vllm-qwen38-mmap.toml` (modified only if priority changes), `specs/runs/qwen38-sglang-qualification.md` (completed), `README.md` (modified), `docs/reference/spark.md` (modified)

**Tests:**

- `src/spark/engine.rs::tests::qwen38_preferred_engine_is_the_unique_priority_winner` — verifies the user-selected declarative winner and an unambiguous compatible control.
- Signed release dry activation and rollback — verifies both the selected release and immediate recovery release before deployment.
- Final real-device launch — `sy spark dgx-spark launch codex --model qwen3.8:flash-next` reports the chosen engine and completes a streamed reasoning/tool turn.

**Definition of Done:**

- [ ] The user receives paired startup, short-context, growing-prefix, long-context, quality, protocol, resource, and three-agent evidence.
- [ ] The user explicitly chooses SGLang preferred or vLLM preferred; no default is inferred from a microbenchmark.
- [ ] Engine priority changes only in TOML, and the unselected engine remains in the signed catalog as a recoverable control.
- [ ] The selected signed release is deployed to `dgx-spark`; rollback is verified without a model redownload.
- [ ] Local `sy` and Spark control plane use the same completed release/configuration.
- [ ] README and Spark reference document the chosen behavior, exact limitations, recovery, and measured evidence.
- [ ] `make lint && make test` is green with zero warnings and flakes.

**Risks / unknowns:** Day-zero upstream maintenance cost may outweigh measured speed; the evidence and immutable pin make that tradeoff explicit.

## Cross-cutting Definition of Done

- [ ] All 15 step DoDs are satisfied and checked in this roadmap.
- [ ] No production Rust or package script selects behavior by Qwen, SGLang, vLLM, checkpoint, or artifact filename.
- [ ] The exact same installed checkpoint serves through both candidate and control without redownloading model blobs.
- [ ] OpenAI Chat, OpenAI Responses, and Anthropic Messages pass streaming, non-streaming, reasoning, tools, usage, cancellation, and follow-up tests.
- [ ] Native 262K capacity, persistent verified PLE reuse, bounded memory, and semantic readiness are proven on the real `dgx-spark`.
- [ ] Fresh Codex, Claude, and OpenCode Tetris journeys complete with runnable artifacts and paired timing evidence.
- [ ] The user makes the preferred-engine decision only after evidence; the answer is represented entirely by signed configuration.
- [ ] The selected engine survives stop/start and the alternative is recoverable through signed rollback.
- [ ] `make lint && make test` is green and documentation matches deployed behavior.

## Out of Scope

- Host OS, firmware, kernel, NVIDIA driver, CUDA installation, clock, power, swap, or boot changes.
- Reduced reasoning, context, gateway protocol, or model quality to raise throughput.
- The experimental wide QSA ring and seven-step speculative profile.
- Full-context concurrency above one on a single 128 GB Spark.
- Direct public SGLang access outside the authenticated sy gateway.
- TokenSpeed or FreeToken support for this checkpoint until they have a proven single-Spark PLE path.
- Any engine or model recipe embedded in the sy binary.
