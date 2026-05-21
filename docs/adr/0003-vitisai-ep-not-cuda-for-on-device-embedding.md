# 0003 — VitisAI EP, not CUDA, for on-device embedding

- Status: accepted

> Template: [MADR 4.0](https://adr.github.io/madr/).
> Sources: `README.md` §aiplane + §NPU one-time setup + §Knowledge
> hardware tier; `AGENTS.md` §NPU-specific norms.

## Context and Problem Statement

`sy knowledge` runs a semantic search index over local files. The
hot path is text embedding: chunk the file, embed the chunk,
store the vector in qdrant. The embedding model
(`intfloat/multilingual-e5-base`, 768-dim) is run by the
`aiplane` daemon's `embed` workload.

The target machine is a Fedora 43 laptop with an AMD Ryzen AI
Phoenix CPU. The CPU carries an integrated XDNA / XDNA2 NPU
addressable as `/dev/accel/accel0` once the `amdxdna` DKMS
module is loaded. The user may also have a discrete GPU (NVIDIA
or AMD) and may have CUDA-capable Python wheels already
installed under `~/.local/lib/python*/site-packages/nvidia/`.

ONNX Runtime supports many execution providers (EPs): CPU,
CUDA, TensorRT, DirectML, ROCm, OpenVINO, CoreML, the AMD
Vitis AI EP. The decision is *which EP the aiplane daemon
declares as its preference for the embed workload* — and,
specifically, why CUDA is not in the fallback chain.

## Decision Drivers

- **"Free GPU" on idle silicon**: the NPU is otherwise unused
  on a coding laptop. Routing embeddings to it leaves the dGPU
  free for LLM inference and the CPU free for the IDE / agent.
- **Zero VRAM footprint**: a one-shot CLI invocation should not
  spin up CUDA driver context + 1 GiB of VRAM just to embed a
  query string. The cost is paid even on a single search.
- **Hardware reality on the target platform**: AMD Ryzen AI
  Phoenix is reached via the VitisAI EP. CUDA is NVIDIA-only;
  DirectML is Windows-only; ROCm covers discrete AMD GPUs, not
  the XDNA NPU.
- **Single-context device discipline**: `/dev/accel/accel0` is
  single-context. One process owns it; everyone else delegates
  over IPC. The EP choice has to align with that constraint
  (which the AMD Ryzen AI 1.7.1 venv layout already encodes).
- **Reproducibility**: the AMD venv at `/opt/AMD/ryzenai/venv`
  ships a tested matrix of ONNX Runtime + VAIP + flexml + VOE +
  XRT versions. Pinning to that matrix via the re-exec dance
  is more reproducible than letting `pip` resolve transitive
  CUDA wheels at runtime.
- **Cancellation budget**: per-request CLI invocations need
  fast teardown. CUDA driver init is measured in hundreds of
  milliseconds; the NPU plane is always-warm under
  `sy.target`.

## Considered Options

- **Option 1: VitisAI EP only, with a CPU fallback** — the
  workload declares `EpPreference::Vitisai | EpPreference::Cpu`.
  The session pool loads VitisAI if `/opt/AMD/ryzenai/venv` is
  detected, otherwise CPU. CUDA is intentionally absent from
  the chain.
- **Option 2: VitisAI EP, CUDA EP, then CPU** — three-way
  fallback. NPU is preferred but a CUDA-equipped machine can
  fall through to GPU before CPU.
- **Option 3: CUDA EP as primary** — the original fastembed
  shape: pip-installed `onnxruntime-gpu==1.24.*`, CUDA libs
  picked up from the `nvidia/*/lib` site-packages tree, no NPU
  use at all.
- **Option 4: DirectML EP** — vendor-neutral over DX12, listed
  for completeness.

## Decision Outcome

Chosen option: **Option 1, VitisAI EP with CPU fallback. CUDA
is excluded from the chain.**

Reasons, in priority order:

1. **CUDA is the wrong vendor.** The target machine's NPU is
   AMD XDNA / XDNA2 on Ryzen AI Phoenix. CUDA cannot address
   it. A CUDA fallback would not be a fallback for the same
   workload; it would be a different code path running on a
   different chip with different latency and a different
   memory footprint.
2. **CUDA spins up GPU VRAM for one-shot CLI invocations.**
   `sy knowledge search` is invoked many times per minute by a
   working agent. Paying CUDA driver init + ~1 GiB of VRAM on
   each invocation defeats the "ambient on-device inference"
   premise.
3. **The NPU is otherwise idle.** Routing embeddings to it
   leaves the dGPU free for LLM inference (where the VRAM is
   actually load-bearing) and the CPU free for the IDE and
   agent processes.
4. **AMD's tested matrix is what the daemon ships against.**
   The Ryzen AI 1.7.1 venv pins ORT + VAIP + flexml + VOE +
   XRT versions that have been validated together. The
   re-exec dance is built around that pinning. Adding CUDA
   would force us to also pin `onnxruntime-gpu` against a
   compatible CUDA toolkit + driver matrix the project has no
   commitment to maintain.
5. **CPU stays in the chain.** Machines without
   `/opt/AMD/ryzenai/venv` (CI hosts, non-Phoenix Ryzen,
   contributors on Intel) still work; embedding throughput
   drops but the binary does not break.

DirectML is rejected because it is Windows-only.

This is consistent with `AGENTS.md`'s NPU-specific norms:
"Workloads declare their EP preference (`Vitisai | Cpu`), not a
fallback chain. The session pool decides what to load based on
what's available; CUDA is intentionally not in the chain because
it spins up GPU VRAM for one-shot CLI invocations that should be
free."

## Consequences

- **Good**: zero VRAM cost for `sy knowledge` operations on a
  Ryzen AI laptop; the dGPU is free for the LLM that consumes
  the search results.
- **Good**: per-request init cost stays low — the aiplane
  daemon owns one warm VitisAI session for the embed model.
- **Good**: one tested ONNX Runtime + EP matrix to support
  (AMD's), not two.
- **Good**: deterministic device ownership — one process per
  `/dev/accel/accel0`, no second ORT session contention.
- **Neutral**: the existing `cuda` row in the README's "Knowledge
  hardware tier" table remains as a *legacy* fastembed code path
  for machines without the NPU but with an NVIDIA GPU. It is
  not the recommended path and is not part of the aiplane
  daemon's EP fallback chain.
- **Bad**: the project depends on AMD's bundled venv at
  `/opt/AMD/ryzenai/venv`, which is supplied out-of-tree via
  the [`ryzenai-rpm`](https://github.com/dmytrogajewski/ryzenai-rpm)
  companion repository. The build is not a single `cargo build`
  on a stock Fedora — NPU one-time setup is a documented
  prerequisite (`README.md` §NPU one-time setup).
- **Bad**: the re-exec dance (`aiplane::reexec`) is load-bearing.
  `LD_LIBRARY_PATH`, `ORT_DYLIB_PATH`, and the `RYZEN_AI_*`
  variables must be set before any thread spawns. Adding a new
  dep to that path requires a corresponding test in
  `aiplane::reexec`.
- **Bad**: dropping from `multilingual-e5-large` (1024-dim) to
  `multilingual-e5-base` (768-dim) was the price of the VitisAI
  EP 1.7.1 2 GiB ModelProto serialisation cap. MTEB quality
  cost is roughly 6 % (64.2 → 60.5 avg). Acceptable trade for
  the free-GPU outcome but worth recording.

## Pros and Cons of the Options

### Option 1 — VitisAI EP + CPU fallback

- Good: zero VRAM, always-warm, matches AMD's tested matrix.
- Good: aligns with the single-context `/dev/accel/accel0`
  discipline and the one-process-per-NPU rule.
- Neutral: requires the AMD venv to be present for the NPU
  path; CPU fallback covers everything else.
- Bad: dependency on the AMD-supplied venv and on the
  `amdxdna` kernel module (DKMS).

### Option 2 — VitisAI EP + CUDA EP + CPU

- Good: maximises hardware utilisation across heterogeneous
  hosts.
- Bad: pulls CUDA toolkit + driver matrix into the
  maintenance surface for ~zero real benefit on the target
  platform.
- Bad: each invocation pays CUDA init cost on machines where
  the NPU is *almost* available but transiently not. Footgun.

### Option 3 — CUDA EP as primary

- Good: the original fastembed code path; well-understood.
- Bad: leaves the NPU idle on a machine that paid for it.
- Bad: spins up CUDA context + VRAM on every `sy knowledge`
  invocation; defeats the "ambient inference" premise.
- Bad: ties the project's hot path to NVIDIA-specific
  hardware in an Agentic-OS-for-Fedora design that is
  otherwise vendor-neutral.

### Option 4 — DirectML EP

- Bad: Windows-only. `sy` is a Fedora 43 OS layer.

## Links

- README rationale: `README.md` §`aiplane` — on-device NPU
  inference, §NPU one-time setup, §Knowledge hardware tier,
  §"Why `multilingual-e5-base`, not `-large`?".
- AGENTS rationale: `AGENTS.md` §NPU-specific norms (the
  "EP preference (`Vitisai | Cpu`), not a fallback chain" rule;
  the one-process-per-NPU rule; the re-exec dance).
- Companion repository for the system-level NPU stack:
  [`ryzenai-rpm`](https://github.com/dmytrogajewski/ryzenai-rpm).
- Upstream EP references: [ONNX Runtime — Vitis AI EP](https://onnxruntime.ai/docs/execution-providers/Vitis-AI-ExecutionProvider.html),
  [ONNX Runtime — CUDA EP](https://onnxruntime.ai/docs/execution-providers/CUDA-ExecutionProvider.html),
  [ONNX Runtime — DirectML EP](https://onnxruntime.ai/docs/execution-providers/DirectML-ExecutionProvider.html).
- Related decision: [ADR-0001 — Use ADRs](0001-use-adrs.md).
- Related decision: [ADR-0002 — Virtual workspace with `sy-core` vocabulary](0002-virtual-workspace-with-sy-core-vocabulary.md).
- Audit row: `specs/docs-audit/AUDIT-full.md#r-adr-01--should`.
- Roadmap item: `specs/docs-audit/PLAN-full.md` Item 16.
