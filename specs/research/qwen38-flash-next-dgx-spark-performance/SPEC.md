# SPEC: Qwen3.8-Flash-Next performance on one DGX Spark

Date: 2026-08-28

## 1. Request

Improve the effective performance of the exact installed
`RadixArk/Qwen3.8-Flash-Next-NVFP4@7b719225242aacd3dbd3f9407468c2ee9a9d2594`
checkpoint on one 128 GB DGX Spark without reducing its 262,144-token
capability, reasoning, tool use, streaming correctness, or appliance stability.

This is a research and qualification specification. It does not implement or
deploy a replacement engine.

Actor: a workstation user launching Codex, Claude, or OpenCode through
`sy spark`.

Surface: existing declarative Spark model and engine configuration, the
existing authenticated sy gateway, and reproducible real-device benchmark
evidence.

Success is observable as lower time to first token on growing coding-agent
conversations, higher sustained useful decode throughput, shorter completed
agent journeys, and no regression in reasoning, tool calls, streams, context,
or service health.

## 2. Executive conclusion

The installed vLLM profile is already close to the best published
single-Spark vLLM recipe. Its warm prefill reaches 1,743–2,565 tokens/s, which
matches the recipe's 2,000–2,600 tokens/s range. More generic flag tuning is
therefore unlikely to transform perceived agent performance.

Two remaining costs dominate:

1. The vLLM PLE mmap implementation performs a host-to-device synchronization
   in every decode step. The published recipe reports about 17 tokens/s without
   MTP and typically 25–28 tokens/s with MTP=2; the current real agent workload
   has recently varied from about 7 to 20 tokens/s.
2. vLLM prefix caching is unsafe for this exact hybrid GDN/QSA model on GB10.
   Growing multi-turn prefixes can crash the engine, so every agent turn must
   prefill the prior conversation again.

The strongest evidence-backed performance candidate is a second, declarative
SGLang engine profile for the same immutable NVFP4 checkpoint. A public
single-GB10 reproduction measured 41.5 tokens/s median on code with the normal
QSA ring, native MTP, and a decode-only TRTLLM attention backend. More
importantly for coding agents, its hybrid radix cache reduced repeated 128K
prefill from 183 seconds to 0.6 seconds and repeated 240K prefill from 195.6
seconds to 1.7 seconds. Those figures are third-party results and must be
reproduced with sy's exact protocol and agent journeys before changing any
default.

The recommended research action is therefore to qualify SGLang alongside the
current vLLM control, not to replace vLLM immediately. All engine and model
policy remains in TOML and pinned image/patch assets; Rust must not contain a
Qwen-, SGLang-, or vLLM-specific branch.

## 3. Exact deployed baseline

### 3.1 Immutable identities

| Component | Deployed identity |
|---|---|
| Checkpoint | `RadixArk/Qwen3.8-Flash-Next-NVFP4@7b719225242aacd3dbd3f9407468c2ee9a9d2594` |
| Model alias | `qwen3.8:flash-next` |
| Engine | `vllm-qwen38-mmap-arm64` |
| Engine source patch | `blazux/qwen3.8-Flash-DGX@d2854bfff0a0b6f46984b0941ed1db6010031295` |
| Engine image | `sy-spark/vllm-qwen38-mmap@sha256:ae03e2a6feecd27520d2598f28dde37c0f7c85c59631d8c488b5803331a6753d` |
| Context | 262,144 tokens |
| Speculation | native MTP, two speculative tokens |
| KV allocation | 12 GiB, BF16/auto |
| Concurrency ceiling | 8 sequences |
| Prefix caching | disabled |

The current configuration is in
`configs/sy/spark/engines/vllm-qwen38-mmap.toml`; model identity and artifact
hashes are in `configs/sy/spark/models.toml`.

### 3.2 Live evidence from `dgx-spark`

The healthy instance inspected during this research was
`launch-qwen3-8-flash-next-f1df29da`:

| Observation | Value |
|---|---:|
| Reported cold startup | 753,385 ms |
| Warm prompt throughput in recent logs | 1,743–2,565 tokens/s |
| Recent real-agent generation throughput | about 7–20 tokens/s |
| Latest observed MTP mean accepted length | 2.66 of 3 total tokens |
| Latest observed draft acceptance | 82.9% |
| KV capacity | 433,944 tokens |
| KV use during recent requests | 8–10% |
| Waiting requests | 0 |
| Host memory available | 14,593,351,680 bytes |
| Full memory PSI, 10-second average | 2.6% |
| Restart failures | 0 |

The decode figures are scheduler-window counters from real reasoning and tool
traffic, not a controlled 400-token generation benchmark. They establish what
the user experiences, but they cannot be compared directly with a published
microbenchmark. The qualification plan below creates an apples-to-apples
control.

### 3.3 Why the checkpoint barely fits

Qwen describes Flash-Next as a 125B-parameter sparse model with 6B parameters
active per token plus a 51B-parameter n-gram embedding table. The checkpoint
contains approximately:

| Component | Approximate resident size before offload |
|---|---:|
| Routed experts in NVFP4 | 63.3 GiB |
| PLE n-gram table in FP8 | 47.7 GiB |
| BF16 attention, GDN, residual, head, vision, and MTP tensors | 14.9 GiB |
| Total checkpoint | 126.0 GiB |

The Spark exposes about 121.6 GiB usable unified memory. Moving PLE from GPU
memory to ordinary pinned host memory does not help because both allocations
consume the same physical pool. Both viable single-Spark implementations keep
PLE file-backed on NVMe and let the GB10's pageable-memory support service the
sparse gathers. This is consistent with Qwen's architecture: the official
description explicitly makes the PLE table suitable for off-accelerator
storage and asynchronous prefetching.

Sources:

- [Qwen3.8-Flash-Next repository and architecture](https://github.com/QwenLM/Qwen3.8-Flash-Next)
- [Qwen3.8-Flash-Next technical report](https://github.com/QwenLM/Qwen3.8-Flash-Next/blob/main/tech_report.pdf)
- [RadixArk NVFP4 checkpoint](https://huggingface.co/RadixArk/Qwen3.8-Flash-Next-NVFP4)
- [DGX Spark hardware specification](https://docs.nvidia.com/dgx/dgx-spark/hardware.html)

## 4. Where time is spent

```text
agent turn
   |
   +-- tokenize and gateway overhead                  small
   |
   +-- prefill all uncached prior conversation       dominant at long context
   |      `-- vLLM cache disabled by correctness bug
   |
   +-- decode target token
   |      +-- NVFP4 MoE and hybrid-attention kernels
   |      +-- PLE sparse mmap gather and synchronization
   |      `-- verify MTP draft tokens
   |
   `-- stream reasoning/text/tools to the client      must remain correct
```

The correct optimization target depends on the journey:

- A new, short request is mostly decode-bound.
- The first request over a large repository is prefill-bound.
- A multi-turn coding session is prefix-reuse-bound. Recomputing 50K–200K
  tokens every turn can dominate any gain from adding several decode tokens
  per second.
- Predictable code receives more benefit from MTP than prose or unconstrained
  reasoning. Throughput must be reported separately for both.

## 5. Engine comparison

| Engine and license | Single-Spark fit | Measured evidence | Main limitation | Role |
|---|---|---|---|---|
| Patched vLLM, Apache-2.0 | Proven with file-backed PLE; currently deployed | 2.0–2.6K prefill; typically 25–28 code decode with MTP=2 | unsafe growing-prefix cache; one PLE transfer synchronization per decode step | Known-good control |
| Patched SGLang, Apache-2.0; single-Spark recipe MIT | Proven by one public GB10 reproduction with file-backed PLE | 41.5 code, 22.8 prose; repeated 128K prefix in 0.6 s | day-zero patches and upstream churn; long-prefill memory pressure | Qualification candidate |
| llama.cpp, MIT | Fits as IQ4_XS GGUF | about 22 decode in the vLLM comparison | about 540 prefill, no native QSA/MTP acceleration in that comparison; immature qwen4_exp support | Operational fallback |
| TokenSpeed | Official model syntax exists | No immutable single-GB10 NVFP4/PLE result found | official example is TP4; no proven single-Spark memory route | Research watch item |
| FreeToken, Apache-2.0 | Current Flash-Next checkpoint cannot fit without specialized PLE mmap | None for this architecture | current registry lacks `qwen4_exp`, QSA, PLE mmap, gated residual, and MTP support | Not a candidate for this model now |

Primary sources:

- [Patched vLLM single-Spark recipe](https://github.com/blazux/qwen3.8-Flash-DGX)
- [Patched SGLang single-Spark recipe](https://github.com/hashd1ve/qwen38-flash-next-one-dgx-spark)
- [Official Qwen engine examples](https://github.com/QwenLM/Qwen3.8-Flash-Next/blob/main/README.md)
- [vLLM](https://github.com/vllm-project/vllm)
- [SGLang](https://github.com/sgl-project/sglang)
- [llama.cpp](https://github.com/ggml-org/llama.cpp)
- [FreeToken model matrix](https://github.com/FlashML-org/FreeToken/blob/main/docs/models.md)

## 6. Current vLLM: useful changes and hard limits

### 6.1 Keep these baseline choices

- Keep NVFP4. It is the only supplied quantization that uses GB10's native FP4
  path while leaving enough room for the non-expert weights and runtime.
- Keep PLE file-backed on NVMe. Native vLLM CPU offload deadlocks TP1 warmup on
  GB10 in an open report, while ordinary host offload cannot create physical
  memory on a unified-memory machine.
- Keep PIECEWISE CUDA graphs. Full compile/capture crosses unsupported hybrid
  operations.
- Keep prefix caching disabled until the open growing-prefix failure is fixed
  and reproduced on this exact model and SM121.
- Keep at least the configured 8 GiB appliance reserve. Recent live availability
  is only about 13.6 GiB, and the public recipe reports an OOM at a more
  aggressive memory fraction during a long prefill.
- Keep reasoning and native MTP enabled. Disabling reasoning for the engine is
  not an acceptable performance technique.

Relevant defects:

- [vLLM growing-prefix cache crash on Qwen3.8-Flash-Next/GB10](https://github.com/vllm-project/vllm/issues/54173)
- [vLLM native PLE CPU-offload deadlock on TP1/GB10](https://github.com/vllm-project/vllm/issues/53960)
- [vLLM prefix/MTP recomputation issue](https://github.com/vllm-project/vllm/issues/53670)
- [vLLM long-context GDN/MTP acceptance regression report](https://github.com/vllm-project/vllm/issues/52873)

### 6.2 Safe, bounded vLLM experiments

These are experiments, not proposed defaults:

1. Enable `VLLM_PLE_MMAP_PREWARM=1` in a separate profile. The source recipe
   reports about ten extra startup seconds and a steadier first request, but no
   steady-state speed gain. It is useful only if cold first-token variance is a
   user problem.
2. Compare MTP=2 with MTP=3 using identical code, prose, long-context, streamed
   tool, and reasoning traces. More speculative tokens help only if acceptance
   offsets target-verification overhead. Open MTP correctness reports make a
   blind default change unsafe.
3. Compare `max-num-batched-tokens=8192` with 16384 for cold prefill. Reject the
   larger value if available memory, full PSI, or time to first token worsens.
4. Prototype pinned staging or a bounded hot-row cache inside a separately
   pinned engine image. The current recipe identifies removal of its per-step
   PLE transfer synchronization as the remaining decode optimization. This
   requires kernel-level evidence and cannot be expressed as an unverified
   flag.

None of these addresses vLLM's largest multi-turn cost: safe prefix reuse.

### 6.3 Changes explicitly rejected

- Do not enable vLLM prefix caching based on a repeated identical prompt. The
  reported failure appears on growing multi-turn prefixes after several turns,
  even though exact replay can look dramatically faster.
- Do not use native PLE CPU offload, FP8/NVFP4 KV cache, FULL CUDA graphs, or a
  higher memory fraction without exact-model evidence.
- Do not lower the host reserve, add swap as model capacity, change clocks, or
  update the DGX OS, kernel, driver, CUDA stack, or firmware.
- Do not optimize only a short deterministic code completion. It overstates MTP
  benefit and omits the dominant agent prefix cost.

## 7. SGLang candidate

### 7.1 Evidence-backed configuration

The compared recipe is pinned at
`hashd1ve/qwen38-flash-next-one-dgx-spark@04d073518ded5d0db1cddce74d9afb1cdca5eddc`.
Its current published image tag resolves to the multi-architecture manifest
`sha256:12d3392bdc8be8d35e9a95f191df6aef99c5114bdbefd41bfdc7e760e6d25ec1`
and ARM64 image
`sha256:14ed582518584c5c830206b5318a2c2769e68229c3422e48a28b952b3a888bd4`.
The tag is mutable and is not an acceptable sy input; implementation must pin
the ARM64 digest and record the engine source revision and applied patch hashes.

The evidence-backed long-context profile is:

- prefill attention: Triton;
- decode attention: `trtllm_mha` after widening the QSA SM gate to SM120/121;
- quantization: `modelopt_fp4`;
- PLE: file-backed mmap through the existing Triton gather, with random-access
  advice;
- context: 262,144;
- static memory fraction: 0.79, not the short-context 0.85 profile;
- chunked prefill: 1,024 tokens;
- maximum running requests: one for a full-context session, with two available
  only after a measured smaller-context gate;
- hybrid cache: mamba radix `extra_buffer`;
- speculation: NEXTN, three steps, top-k one, four draft tokens, and explicitly
  unquantized BF16 draft tensors;
- explicit Qwen reasoning and tool parsers.

The split attention backend is material. The recipe measured code decode rising
from 31.5 to 41.5 tokens/s when decode moved to TRTLLM while prefill remained on
Triton. A single all-phase TRTLLM setting is rejected on SM121 because its
prefill path is gated differently.

### 7.2 Published measurements

One real GB10 reproduction reports:

| Measurement | Result |
|---|---:|
| Code decode, five-run median | 41.5 tokens/s, range 40.3–42.3 |
| Prose decode, five-run median | 22.8 tokens/s, range 21.2–25.5 |
| No-speculation decode | 17.8 tokens/s with TRTLLM decode |
| MTP mean accepted per iteration | 2.77 tokens |
| GSM8K | 192/200, 96.0% with a different harness from the checkpoint reference |
| Repeated 8K prefix | 14.1 s first, 0.2 s cached |
| Repeated 128K prefix | 183.0 s first, 0.6 s cached |
| Repeated 240K prefix | 195.6 s first, 1.7 s cached |
| Long-context decode at 8K / 128K / 240K | 27.3 / 24.7 / 21.7 tokens/s |
| Cold startup | about 9 minutes, including about 2.5 minutes rewriting PLE |

These numbers establish feasibility, not sy acceptance. The public long-context
prefill series reused related prefixes, and a later attempt to collect several
disjoint cold long-prefills at a 0.85 memory fraction made the host
unresponsive. That is why the candidate uses 0.79, smaller prefill chunks, and
one full-context request.

### 7.3 Prefix caching is the main agent optimization

SGLang's `extra_buffer` hybrid radix policy retains prefix state for the
attention and recurrent portions of the model. This is structurally better
matched to agents than raising raw decode alone:

- repository instructions and tool schemas are stable across turns;
- conversation history grows by a suffix;
- Codex, Claude, and OpenCode resend most of the same prompt;
- the cache pays the large prefix once and computes only the new suffix.

Qualification must use growing prefixes, not only an identical replay. The vLLM
failure proves why exact-replay cache checks are insufficient.

### 7.4 Experimental ring widening is not the default candidate

The recipe's separate QSA ring-width patch raises code throughput from 42.2 to
49.8 tokens/s with seven speculative steps, but reduces its reported prose
throughput from 25.6 to 16.4 tokens/s. It also alters a model correctness
boundary beyond an architecture-enablement patch. Coding agents produce prose,
reasoning, JSON tool calls, and code in the same turn, so this is not a safe
general profile.

It may be qualified later as an explicitly selected code-throughput profile
only after broader agent-quality evidence. It is excluded from the initial
candidate.

### 7.5 Durability improvement

The SGLang recipe rewrites the 47.7 GiB PLE backing file on every boot, adding
about 2.5 minutes and unnecessary NVMe writes. sy should treat the transformed
PLE file as a deterministic derived artifact:

1. Produce it once from the exact checkpoint revision.
2. Record source snapshot fingerprint, transform revision, size, and SHA-256.
3. Atomically publish only after full verification.
4. Mount/map it read-only on later engine starts.
5. Rebuild it only when its source or transform identity changes.

This must first pass bit-exact FP8 dequantization and gather tests. It is a
cold-start and durability improvement, not a claimed steady-decode gain.

### 7.6 Maturity and supply-chain risk

Generic Qwen3.8-Flash-Next support and the relevant SM121/NVMe changes are still
moving through SGLang pull requests. The candidate therefore cannot track
`latest`, `main`, or an unpinned Docker tag. It needs:

- exact SGLang source and recipe commits;
- exact patch contents and hashes, applied with context verification;
- digest-pinned CUDA base and ARM64 runtime image;
- captured Python package lock and OCI metadata/SBOM;
- offline runtime against the immutable model snapshot;
- no DGX host-stack update.

Upstream context:

- [Qwen3.8-Flash-Next support PR](https://github.com/sgl-project/sglang/pull/36497)
- [SGLang attention backend documentation](https://github.com/sgl-project/sglang/blob/main/docs_new/docs/advanced_features/attention_backend.mdx)
- [SGLang speculative decoding documentation](https://github.com/sgl-project/sglang/blob/main/docs_new/docs/advanced_features/speculative_decoding.mdx)

## 8. Proposed sy architecture

No new end-user command or protocol is needed. The existing commands remain:

```text
sy spark dgx-spark serve qwen3.8:flash-next
sy spark dgx-spark launch codex --model qwen3.8:flash-next
sy spark dgx-spark launch claude --model qwen3.8:flash-next
sy spark dgx-spark launch opencode --model qwen3.8:flash-next
```

The additional engine is ordinary configuration:

```text
configs/sy/spark/models.toml
        | exact checkpoint, hashes, capabilities, selected profile
        v
configs/sy/spark/engines/sglang-qwen38-mmap.toml
        | image digest, argv, environment, resources, routes, health
        v
generic Spark executor
        | selects by artifact traits and explicit engine profile
        v
SGLang container on sy-spark-internal
        |
        v
existing sy gateway -> OpenAI / Responses / Anthropic adapters
```

Architecture rules:

- Model and engine selection, arguments, parsers, resource envelopes, and
  sampling remain declarative.
- Rust may gain only generic schema/runtime support proven necessary for any
  engine. It must not inspect model aliases or engine family names to inject
  behavior.
- vLLM remains independently selectable and serves as the qualification
  control.
- Only one memory-heavy model runs during single-Spark qualification.
- Direct engine ports remain internal. Clients continue through the existing
  authenticated gateway.
- The candidate runs non-root with a read-only model snapshot and derived PLE
  artifact, scoped writable compile cache, bounded shared memory, PID limit,
  seccomp, and no runtime network egress.

There is no storage-schema or public wire migration. No backward-compatibility
adapter is required.

## 9. Qualification matrix on the real Spark

Every comparison uses the same checkpoint revision, prompts, sampling,
reasoning budget, output limit, gateway, and otherwise idle appliance. Results
are retained as JSON plus raw redacted engine logs.

### 9.1 Controlled engine measurements

For current vLLM and candidate SGLang:

1. Record immutable checkpoint, image, source, patches, argv, host stack,
   startup time, available memory, PSI, and restart count.
2. Run one warmup and ten measured 400-token completions for:
   - deterministic code continuation;
   - unconstrained technical prose;
   - reasoning plus a structured tool call.
3. Report median, range, p95 time to first token, prompt throughput, decode
   throughput, accepted draft length/rate, and final output validity.
4. Run cold disjoint prompts at 8K, 32K, and 128K once each. Do not chain
   multiple 240K cold-prefill runs on the memory-constrained appliance.
5. Run a ten-turn growing-prefix trace at 8K, 32K, 64K, and 128K endpoints.
   Record cache hit/reused tokens and per-turn time to first token.
6. Run one 240K needle retrieval, then one cached suffix turn. Confirm the
   answer, time to first token, memory floor, and health before and after.
7. Measure concurrency one, two, and four only with the 8K trace; concurrency
   above one is not a full-context requirement.

This is a bounded qualification matrix. It has no endurance or overload gate.

### 9.2 Protocol and semantic correctness

Both engines must pass through the real sy gateway:

- OpenAI Chat non-streaming and SSE;
- OpenAI Responses non-streaming and SSE;
- Anthropic Messages non-streaming and SSE;
- reasoning deltas separated from answer content;
- one and two parallel tool calls with valid JSON arguments;
- usage accounting and correct finish reasons;
- mid-stream cancellation followed by a healthy request;
- growing multi-turn conversation beyond 32K;
- exact model identity presented to all three client adapters.

MTP is disabled only in an A/B measurement, never in the candidate's semantic
acceptance run. A faster result fails if reasoning disappears, tool JSON is
corrupted, streamed text diverges from non-streaming output, or acceptance
collapses after a long prefix.

### 9.3 Fresh coding-agent journeys

Use three clean directories under `~/sources/testbed` for each engine and each
agent. Codex, Claude, and OpenCode independently receive the same task to build
a browser Tetris game. Each run must start with no prior agent session state.

Capture:

- wall-clock time to first useful response and completed runnable game;
- engine prompt/decode/MTP/cache metrics per turn;
- number of turns, tool calls, retries, and protocol failures;
- generated files and each agent's own checks;
- an actual browser run confirming controls, rotation, collision, scoring,
  line clearing, game over, and restart;
- final engine health and restart count.

The candidate is useful only if the end-to-end agent journey improves, not
merely the synthetic code completion.

### 9.4 Acceptance gates

The candidate is eligible to become the preferred profile only when all of the
following are true on the same-device comparison:

- median code decode is at least 35 tokens/s and at least 30% above the vLLM
  control;
- prose and reasoning decode do not fall below the vLLM control by more than
  10%;
- a cached growing-prefix turn reduces time to first token by at least 80% at
  32K, 64K, and 128K;
- cold time to first token at 8K, 32K, and 128K is no more than 15% worse than
  the vLLM control;
- all three fresh Tetris journeys complete, and at least two finish at least
  25% faster than their matching vLLM controls;
- reasoning, tools, streaming, cancellation, usage, 262K context, and the 240K
  needle check all pass;
- no engine crash, restart, quarantine, OOM, or post-run health failure occurs;
- available host memory remains above the configured 8 GiB emergency floor,
  and the run does not introduce sustained swap growth;
- a warm restart reuses the verified PLE artifact and is faster than the
  measured current 753,385 ms startup.

Thresholds combine published feasibility with the minimum change a user should
notice. They are qualification gates, not a promise that third-party benchmark
numbers will reproduce exactly.

## 10. Observability and evidence

Engine-neutral evidence should include:

- `engine_id`, engine fingerprint, image digest, model fingerprint, exact argv,
  and profile fingerprint;
- startup phase durations: PLE preparation, weight load, compile, graph capture,
  semantic probe, and healthy transition;
- per-request input, cached, and output tokens;
- time to first token, prompt/decode throughput, request latency, queue depth,
  and KV use;
- speculative draft, accepted tokens, acceptance rate, and mean accepted
  length;
- prefix-cache hits and reused tokens;
- host available memory, memory full PSI, swap deltas, container exit/restart,
  and health state;
- protocol route, stream completion/cancellation, finish reason, tool-call
  validity, and usage consistency.

Benchmark tooling may be a repository script and fixture set. It must emit a
machine-readable artifact and avoid adding noisy default output to `sy spark
ls` or `sy spark ps`.

## 11. User journey sketch

1. The user selects `qwen3.8:flash-next`; sy resolves the exact checkpoint and
   explicitly selected engine profile from configuration.
2. sy validates capacity, mounts the immutable checkpoint and verified PLE
   artifact, then starts the pinned non-root engine on the internal network.
3. The user launches Codex, Claude, or OpenCode without knowing engine-specific
   flags; the client receives its normal OpenAI or Anthropic-compatible URL.
4. The first repository turn performs cold prefill; later turns reuse the
   stable prefix and stream only newly computed reasoning, tools, and text.
5. `sy spark ps` remains compact, while bounded logs and structured benchmark
   artifacts expose detailed performance when requested.
6. If candidate health or semantics fail, the user can stop it and select the
   unchanged vLLM profile without changing or redownloading the model.

Friction points:

- cold start remains several minutes because approximately 81 GiB of resident
  weights must load and CUDA graphs must capture;
- a first 128K–240K prefill remains slow even with sparse attention;
- day-zero SGLang patches can regress across upstream changes, hence immutable
  engine identity and real-device qualification;
- a full-context single-Spark session has little room for concurrency;
- cache reuse depends on clients preserving stable prompt prefixes.

## 12. Risks and mitigations

| Risk | Effect | Mitigation |
|---|---|---|
| SGLang support is day-zero and partly out of tree | silent quality loss or engine crash | pin every source/image/patch; bit-exact PLE test; protocol, quality, long-prefix, and agent gates |
| Long prefill competes with PLE page cache in unified memory | host becomes unresponsive | memory fraction 0.79, chunk 1024, one full-context request, 8 GiB floor, one bounded 240K check |
| MTP favors code but harms unpredictable prose | synthetic win, worse agents | separate code/prose/reasoning metrics; cap initial candidate at the normal ring and 3-step/4-token MTP |
| Prefix cache appears correct only on identical replay | failure in real conversations | ten-turn growing-prefix traces and real agents, not replay alone |
| PLE mmap transform is wrong | fluent but semantically degraded output | verify source hash, byte layout, FP8 dequantization, sparse gathers, GSM8K/code checks, and 240K retrieval |
| Mutable day-zero container changes underneath sy | irreproducible behavior | ARM64 manifest digest, source revision, dependency lock, SBOM, offline runtime |
| Startup rewriting wears NVMe and wastes time | slow restarts and unnecessary writes | deterministic verified derived PLE artifact, atomic publish, read-only reuse |
| Optimizer leaks into Rust model branches | recurring hardcoded design | generic engine schema and matcher only; boundary test rejects model/engine-name dispatch |
| Published results use different harnesses | incorrect performance conclusion | same-device, same-prompt paired control and raw evidence |

## 13. Decisions supported by the evidence

The research supports these conclusions without changing deployed behavior:

- Current vLLM is the correct control and remains a supported engine.
- The next high-value qualification target is SGLang with native SM121 QSA
  decode, normal QSA ring, MTP 3/4, and hybrid prefix caching.
- Prefix reuse is more important to long-running coding agents than pursuing a
  small additional vLLM decode gain.
- The 0.79 long-context memory profile is safer than the public short-context
  0.85 profile on one unified-memory Spark.
- PLE should be a verified persistent derived artifact rather than rewritten on
  every engine start.
- llama.cpp remains a compact fallback; TokenSpeed and FreeToken lack current
  evidence for this exact single-Spark checkpoint.
- No host OS, firmware, kernel, driver, CUDA, power, or clock change is required
  or proposed.
- No model- or engine-specific behavior belongs in the sy Rust binary.

One operator decision remains after the paired qualification: whether the
SGLang profile's measured agent improvement and day-zero maintenance risk are
good enough to make it preferred over vLLM. The benchmark evidence should be
presented before asking that single decision.

## 14. Non-goals

- Updating or tuning the protected DGX Spark host stack.
- Reducing reasoning, context length, tool support, or protocol completeness to
  inflate throughput.
- Replacing the generic multi-engine architecture with embedded model recipes.
- Making the experimental wide QSA ring a default.
- Treating concurrency throughput as the primary result for a single coding
  agent.
- Adding a public direct engine endpoint or bypassing the sy gateway.
- Claiming a benchmark improvement before a fresh paired real-Spark run.

## 15. Hand-off

Next, use `/journey` to capture the paired vLLM/SGLang qualification from the
user's point of view. If the user approves that journey, `/roadmap` should
decompose immutable SGLang packaging, generic declarative engine integration,
persistent PLE derivation, protocol qualification, paired benchmarks, and the
three fresh agent runs. Implementation follows only after that decision.
