# SPEC: Qwen3.8-Flash-Next and FreeToken on DGX Spark

## 1. Summary

Add Qwen3.8-Flash-Next as an immutable, declaratively tuned Spark model, prove
it through fresh Claude, Codex, and OpenCode coding journeys, then add FreeToken
as an independent engine without coupling either model or engine to Rust code.

The research supports three implementation tracks:

1. Serve the native NVFP4 checkpoint with the source-pinned
   `blazux/qwen3.8-Flash-DGX` vLLM PLE-mmap patch. This is the primary path
   because it retains QSA sparse attention and MTP.
2. Retain the qualified IQ4_XS llama.cpp path as a simpler fallback.
3. Build FreeToken from a pinned source revision in a CUDA 13 ARM64 image, use
   its native OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages
   routes, and select Spark-specific runtime behavior in its engine profile.

Request type: research followed by implementation and real-device validation.
Actor: a user launching Claude, Codex, or OpenCode through `sy spark`.
Device: one stock DGX Spark, GB10 `sm_121`, 128 GB unified memory.
Success: each client independently creates a working Tetris game from a clean
directory; streaming, reasoning, tools, cancellation, and post-run health are
correct; the chosen engine/model identities remain reproducible after restart.

### Primary runtime decision

Use `RadixArk/Qwen3.8-Flash-Next-NVFP4` at revision
`7b719225242aacd3dbd3f9407468c2ee9a9d2594` with
`blazux/qwen3.8-Flash-DGX` revision
`d2854bfff0a0b6f46984b0941ed1db6010031295`. The patch serves the approximately
44 GiB PLE/n-gram table through read-only NVMe mmap rather than keeping it in
the unified memory pool. The current checkpoint tree is 135,253,622,894 bytes
(125.97 GiB), while resident weights are approximately 76 GiB after PLE mmap.

Initial qualification uses native 262,144-token context, eight scheduler
sequences, an explicit 12 GiB KV cache, two-token MTP, piecewise CUDA graphs,
prefix caching disabled for the documented GB10 GDN issue, 32 mmap workers,
and a 114,000,000,000-byte container envelope. Percentage-based KV sizing is
rejected on unified memory: a measured 17.08 GiB auto-sized cache left only the
8 GiB emergency floor and the first Claude request triggered the pressure
guard. The engine has a bounded 30-minute cold-start deadline because PLE mmap
plus CUDA initialization measured 11m05s and exceeded the former 15-minute
limit in one run. YaRN at 500K remains opt-in. Source and independent reproduction evidence:
<https://github.com/blazux/qwen3.8-Flash-DGX>.

## 2. Current device evidence

The live Spark inventory on 2026-08-28 reports ARM64, DGX software build 7.5.0,
Ubuntu 24.04.4, kernel `6.17.0-1022-nvidia`, driver 580.159.03, CUDA 13.0,
GB10 compute capability 12.1, 128,427,978,752 bytes of memory, and approximately
654 GB of free model storage. These protected components must remain unchanged.

The current Qwen3.8-27B Q4_K_M plus Q4_0 MTP baseline completed a fresh
OpenAI Responses stream at 26.99 decode tokens/s with 75% draft acceptance.
The first real request returned HTTP 200 and the container restart count stayed
zero. This is the protocol and lifecycle baseline for Flash-Next.

## 3. Model research

### Qwen3.8-Flash-Next architecture

Qwen describes Flash-Next as a multimodal sparse model and a preview of its
Qwen4 architecture. It contains a 125B main model, 51B n-gram embeddings, and a
4B MTP component, with 6B parameters active per token. Its Gated DeltaNet plus
Qwen Sparse Attention design reduces long-context attention work, while the
n-gram table is designed for host-memory offload:
<https://github.com/QwenLM/Qwen3.8-Flash-Next>.

The official model revision is
`Qwen/Qwen3.8-Flash-Next@de4b8e4d43b917e7706784d8bb445c9af86a3540`.
The initial open-weight serving frameworks are Transformers Serve, llama.cpp,
SGLang, vLLM, and TokenSpeed. The official vLLM command requires model support
that is still under review upstream, while llama.cpp merged support on
2026-08-27:
<https://github.com/ggml-org/llama.cpp/pull/27742>,
<https://github.com/vllm-project/vllm/pull/53896>.

### Spark-sized quantization

The immutable Unsloth GGUF revision is
`unsloth/Qwen3.8-Flash-Next-GGUF@83cadfda58d30be06c110518208d1bb918b33f10`.
Its available quantized weight sizes are:

| Quantization | Bytes | Single-Spark assessment |
|---|---:|---|
| UD-Q4_K_XL | 111,334,654,784 | Insufficient reliable runtime and OS headroom |
| UD-IQ4_XS | 93,682,584,224 | Qualification artifact; better long-horizon control than IQ3 while retaining more headroom than Q4_K_XL |
| UD-Q3_K_XL | 89,986,353,824 | More headroom at a larger quality tradeoff |
| UD-IQ3_XXS | 81,961,823,936 | Rejected after real agent prompts remained in reasoning without producing tool calls |

This GGUF table now describes the fallback path. The mmap-patched NVFP4 route
is primary because it preserves native QSA and MTP while fitting one GB10.

The qualification target is UD-IQ4_XS. It consists of three split GGUF files:

| File | Bytes | SHA-256 |
|---|---:|---|
| `UD-IQ4_XS/Qwen3.8-Flash-Next-UD-IQ4_XS-00001-of-00003.gguf` | 10,946,624 | `5ce89370720f8bf90890f439361282104c1aa1482d4013bb9a50923e758e71a4` |
| `UD-IQ4_XS/Qwen3.8-Flash-Next-UD-IQ4_XS-00002-of-00003.gguf` | 49,835,229,856 | `577a38a2392b40ca2193cea502e1d92f60b8cd370675d308e0ec21885d9daaa7` |
| `UD-IQ4_XS/Qwen3.8-Flash-Next-UD-IQ4_XS-00003-of-00003.gguf` | 43,836,407,744 | `d4634e6d84f0ebb0940be15c90d3790bf6464e3dea3a1cddc567dc0e83ad8833` |

The first qualification is text-only, so the 907,542,944-byte BF16 projector
is not downloaded or advertised. Vision can be added only after the text agent
journey is stable and a separate memory measurement proves sufficient reserve.

### llama.cpp readiness

The existing signed llama.cpp image is build b10524 and predates Flash-Next.
Official ARM64 image build b10644 also predates the merged `qwen4exp` commit.
The implementation therefore needs a source-pinned image at or after merge
commit `6c84c7d5d8833c6e0df69628f75a0f599797934e`, with the resulting OCI digest
recorded in engine configuration.

Day-zero correctness risks are material. A Jetson Thor ARM64/Blackwell report
observed incoherent output after offloading more than eight layers, and a
separate CUDA report identified a quantized MoE MMQ tail-read defect:
<https://github.com/ggml-org/llama.cpp/issues/27763>,
<https://github.com/ggml-org/llama.cpp/issues/27792>. The real GB10 test must
therefore compare a known deterministic prompt with GPU offload enabled and
fail qualification on incoherent output; configuration may select a corrected
upstream revision or a generic CUDA backend flag after evidence, never by model
name in Rust.

### Patched vLLM readiness

The build starts from
`vllm/vllm-openai:qwen38-flash-next@sha256:fc120ece0a388cc0aa1caad4a9f1cd92113484ab7ec2fd0efadd62585be05bf8`
and imports the PLE mmap patch by exact revision and SHA-256. The resulting ARM64
image is
`sha256:ae03e2a6feecd27520d2598f28dde37c0f7c85c59631d8c488b5803331a6753d`.
On the real Spark its FP8 mmap test passed single-row, batched, 131,072-row,
range-failure, tensor-view, and prewarm checks. The image reports vLLM
`0.1.dev20073+g8e685d198` and PyTorch `2.13.0+cu130`.

## 4. Engine research

### Compared engines

| Engine | Flash-Next status | DGX Spark status | API status | Role in this work |
|---|---|---|---|---|
| llama.cpp | Support merged | Qualified on ARM64/GB10 | OpenAI Chat upstream; sy supplies complete OpenAI/Anthropic adapters | IQ4_XS fallback |
| vLLM + PLE mmap | Day-zero Qwen image plus pinned patch | Patch test reproduced on the real Spark; source recipe independently reproduced on DGX Spark | OpenAI server; sy supplies complete OpenAI/Anthropic adapters | Primary Flash-Next engine |
| SGLang | Listed by Qwen | Day-zero recipes target multi-GPU deployments; no single-Spark qualification found | OpenAI server | Reference implementation, not initial runtime |
| Transformers Serve | Official local path | Portable but not performance-qualified for GB10 | OpenAI-compatible | Correctness reference only |
| FreeToken | Flash-Next PRs open | Source builds on GB10 are reported working with zero patches | Native Chat, Responses, Anthropic, tools, reasoning, and SSE | New independent engine, first with an already supported model |

### FreeToken composition and maturity

FreeToken is Apache-2.0 Python/CUDA/C++, not Rust. The explicit request to add
this engine supersedes the earlier preference for Rust components. At upstream
commit `9ef3651309fe4058672f2cc92069238dea06be1b`, the repository is approximately
4.05 MB Python, 249 KB CUDA, 128 KB C++, and 111 KB C. Release v0.1.2 publishes
x86_64 wheels only, and the installation document still declares x86_64 as a
requirement: <https://github.com/FlashML-org/FreeToken/blob/main/docs/install.md>.

The source is more portable than the release packaging. FreeToken issue 22
contains two independent zero-patch DGX Spark builds at commit `2757bb5` using
`TVM_FFI_CUDA_ARCH_LIST=12.1` in CUDA 13 containers. One report measured a
gpt-oss-20b fused run at 47.1 end-to-end decode tokens/s, 1,911 prompt tokens/s,
working SSE, reasoning levels, batching, and retrieval. The same report found
that default offload behavior was pathological on unified memory, while fused
mode removed repeated multi-minute stalls:
<https://github.com/FlashML-org/FreeToken/issues/22>.

This evidence leads to a Spark profile with:

- `TVM_FFI_CUDA_ARCH_LIST=12.1` at image build time;
- `--moe-backend fused`, never the current `auto` default;
- `--attention-backend triton` and `--nvfp4-backend triton` for sm_121;
- an explicit KV token budget rather than `cudaMemGetInfo`-based auto-sizing;
- one running request during initial qualification;
- no network access after the immutable snapshot and image are present.

FreeToken provides `GET /health`, `GET /v1/models`, OpenAI chat/completions,
`POST /v1/responses`, `POST /v1/messages`, token counting, reasoning parsing,
tool parsing, and streaming. Its native APIs are exercised directly through
the sy gateway; protocol translation is retained only where the client surface
requires it.

### FreeToken model compatibility and physical fit

FreeToken main lists DeepSeek-V4-Flash, GLM-5.2, Qwen3.6/3.5 MoE, Qwen3-MoE,
gpt-oss, Gemma-4, MiniMax-M2.5, and Muse Glimmer as known-good families:
<https://github.com/FlashML-org/FreeToken/blob/main/docs/models.md>.

Current single-Spark candidates are constrained by 128 GB unified memory:

| Candidate | Pinned checkpoint size | FreeToken status | Result |
|---|---:|---|---|
| `nvidia/Qwen3.6-35B-A3B-NVFP4@491c2f1ea524c639598bf8fa787a93fed5a6fbce` | 23,462,477,790 bytes | Known-good family | Safe first engine qualification |
| `nvidia/MiniMax-M2.5-NVFP4@b6220d658389629b9d507d4b2bb314f41fea7898` | 139,923,512,451 bytes | Known-good | Exceeds physical memory before runtime state |
| `utarn/DeepSeek-V4-Flash-0731-NVFP4@ca20bac907e9711b759fcebd214a2e58ba7bd857` | 173,430,795,167 bytes | Architecture known; this quant is community supplied | Does not fit |
| `LibertAIDAI/GLM-5.3-Flash-NVFP4@aa28e1f54130286c95fee10d0705c74ce8743734` | 194,692,696,910 bytes | GLM-5.3 support is still on FreeToken's roadmap | Unsupported and does not fit |

Qwen3.8-Flash-Next support is also still under review in FreeToken PRs 226 and
232. The FP8 checkpoint is 185,563,783,486 bytes and cannot fit one Spark.
FreeToken must first qualify with Qwen3.6-35B-A3B-NVFP4. Flash-Next may select
FreeToken only after an upstream model implementation is merged or a separately
reviewed source revision passes the same real-device tests.

#### Flash-Next compatibility gate (2026-08-28)

Do not attempt to start the existing NVFP4 snapshot with current FreeToken.
The exact upstream `main` and deployed source revision are both
`9ef3651309fe4058672f2cc92069238dea06be1b`. Its model registry has no
`Qwen4ExpForConditionalGeneration` entry and deterministically returns
`Model architecture Qwen4ExpForConditionalGeneration not supported` before
weight loading. Upstream issue 214 remains an open feature request, the
maintainer model matrix omits Flash-Next, and roadmap issue 79 lists Qwen3.8
family and MTP work as incomplete:
<https://github.com/FlashML-org/FreeToken/issues/214>,
<https://github.com/FlashML-org/FreeToken/blob/main/docs/models.md>,
<https://github.com/FlashML-org/FreeToken/issues/79>.

A config-only probe on the real `dgx-spark`, using the exact deployed ARM64
image `sha256:d2b52ac045b612c9b47b3fbd5d5970a980063314fdf97b770184688a4a3991fd`
and installed immutable snapshot, reproduced that rejection without GPU access
or weight loading. It reported `architectures=['Qwen4ExpForConditionalGeneration']`
and `model_type=qwen4_exp`, then exited at the registry lookup. This is the
qualification result for current FreeToken, not a performance result.

Architecture registration alone is insufficient. The checkpoint declares
`Qwen4ExpForConditionalGeneration` / `qwen4_exp` and requires QSA sparse
attention, four gated residual streams, the 51B-parameter PLE n-gram table,
and a separate MTP layer. Current FreeToken implements none of those
Flash-Next-specific model components. Its 135,253,622,894-byte snapshot also
exceeds the Spark's 128,427,978,752 bytes of physical memory before KV cache,
CUDA graphs, engine code, or the OS are counted. The patched-vLLM path fits
only because it keeps roughly 44 GiB of PLE on NVMe via a specialized mmap
implementation. A FreeToken qualification becomes meaningful only when a
pinned source revision provides native QSA, PLE mmap/offload, gated residual,
and MTP support and passes a pre-start memory-envelope check.

## 5. Proposal

### Architecture

Operational data remains external:

- `models.toml` owns repository, revision, all split artifacts, hashes,
  capabilities, quantization, and selected engine profile.
- Each `engines/*.toml` owns image identity, entrypoint, environment, resource
  envelope, routes, health policy, artifact bindings, and profiles.
- The signed release inventory enumerates every engine declaration rather than
  naming llama.cpp and vLLM in Rust or the package script.
- The executor selects any compatible engine by artifact traits and explicit
  profile. It contains no Qwen, FreeToken, or checkpoint branches.
- Profile-specific resource envelopes allow an 82 GB model without inflating the
  admission estimate of unrelated models.

The initial route is:

```text
Claude / Codex / OpenCode
          |
          v
  sy HTTPS gateway and scoped bearer
          |
          +---- patched vLLM + Flash-Next NVFP4 (primary)
          |
          +---- llama.cpp + Flash-Next IQ4_XS (fallback)
          |
          `---- FreeToken + a supported Spark-sized NVFP4 MoE
```

### Build and supply-chain design

Both custom images are reproducible inputs. Registry artifacts are pinned by
manifest digest; source-built images are pinned by the locally verified Docker
content ID, so no mutable tag or private registry is required:

- source repository and exact commit;
- digest-pinned CUDA base image;
- locked Python dependencies for FreeToken;
- ARM64 and sm_121 build arguments;
- OCI image digest recorded in signed engine configuration;
- local build log and SBOM retained with run evidence.

The Spark upgrade remains a control-plane/config transaction. Engine image
construction or import is a separate explicit software installation step and
must not modify the DGX OS, driver, CUDA installation, firmware, Docker daemon,
Python, power, clocks, swap, kernel, or bootloader.

## 6. User journeys and acceptance

### Flash-Next journey

1. Download the immutable 207-file NVFP4 serving set plus required metadata
   under one friendly alias.
2. Stop any competing model, serve from the pinned patched-vLLM container, and record startup
   memory, exact argv, engine fingerprint, and restart count.
3. Verify OpenAI Chat, OpenAI Responses, and Anthropic Messages streams with
   reasoning, text, tools, usage, cancellation, and a follow-up request.
4. Create three clean directories under `~/sources/testbed`.
5. Launch Claude, Codex, and OpenCode independently against the same model.
6. Each client creates Tetris, runs its own checks, and exits successfully.
7. Build/run each result, inspect controls and gameplay behavior, and confirm
   the model container remains healthy with zero unexpected restarts.

### FreeToken journey

1. Build and install the pinned ARM64/sm_121 image without host-stack updates.
2. Add one generic declarative engine file and deploy the signed catalog.
3. Download Qwen3.6-35B-A3B-NVFP4 and serve with fused/triton settings.
4. Exercise native Chat, Responses, and Messages streaming through the gateway.
5. Repeat the three clean Tetris client journeys.
6. Record TTFT, prompt/decode throughput, memory, tools, output validity, and
   post-run health.
7. Treat the 35B-A3B checkpoint as the large single-Spark qualification; do
   not transfer a second model that the operator has already rejected.

No stress or soak gate is part of either journey.

## 7. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Flash-Next plus KV cache approaches the unified-memory envelope | host pressure or OOM | PLE mmap, explicit 114 GB container envelope, native 262K context, and measured reserve before agent runs |
| Day-zero llama.cpp CUDA correctness | coherent-looking lifecycle with invalid text | deterministic semantic prompts, actual Tetris artifacts, zero-restart check, corrected pinned revision or generic backend flag |
| FreeToken release docs exclude ARM64 | non-reproducible local setup | pinned source image, exact dependency lock, real Spark build, SBOM and image digest |
| FreeToken `auto` chooses offload | repeated multi-minute request stalls | declarative Spark profile forces fused mode |
| CUDA memory API undercounts reclaimable UMA | undersized KV cache | explicit token budget and measured system reserve |
| Native API differs subtly from client expectations | Claude/Codex/OpenCode failures | black-box streams, tool calls, cancellation, usage, and fresh client journeys |
| Engine packaging names specific engines | every new engine requires Rust edits | signed generic directory enumeration with boundary tests |
| Large advertised MoEs exceed 128 GB | download succeeds but serve cannot | reject by declared resource envelope before transfer/serve |

## 8. Decisions and hand-off

Already determined by the request and evidence:

- Flash-Next initial runtime: pinned patched vLLM with native NVFP4, QSA, MTP,
  and PLE mmap.
- Flash-Next fallback: pinned post-merge llama.cpp with text-only UD-IQ4_XS;
  IQ3_XXS failed long-agent control quality.
- FreeToken deployment: pinned source-built ARM64 CUDA image.
- FreeToken Spark mode: fused MoE, Triton attention/NVFP4, explicit KV budget.
- Initial and large-model FreeToken qualification model:
  Qwen3.6-35B-A3B-NVFP4.
- All operational values remain configuration; Rust changes are schema- and
  inventory-generic only.

Next: capture the feature journey, implement in micro-TDD increments, and attach
real Spark evidence to this SPEC.
