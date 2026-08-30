# SPEC: Recommended DGX Spark model checkpoints

## 1. Summary

Select one immutable Spark-oriented checkpoint for Qwen3.8-27B, Muse
Glimmer-30B, and Ornith-1.5-35B-A3B on the 128 GB DGX Spark. The recommendation
uses llama.cpp with a high-quality Q4 GGUF as the throughput-oriented primary
path and retains FP8 with vLLM as the reference and compatibility fallback.

Request: recommend the exact Spark versions of the three named models.
Type: decision.
Actor: a user or coding agent invoking `sy spark ... download|serve|launch`.
Surface: declarative Spark engine profiles and immutable Hugging Face model
identities.
Success looks like: each selected checkpoint fits one Spark, is supported by a
pinned serving engine, and completes real Claude and OpenCode streamed turns.

## 2. Background and research

### Checkpoint approaches

Four distribution approaches were compared:

- BF16 Hugging Face safetensors preserve the reference weights but consume
  roughly twice the weight memory and bandwidth of FP8.
- Block FP8 safetensors retain the vLLM execution path while materially reducing
  weight memory. Qwen states that its official 128-element block-FP8 checkpoint
  is nearly identical to the original model in measured performance:
  <https://huggingface.co/Qwen/Qwen3.8-27B-FP8>.
- K-quant GGUF reduces weight bandwidth while retaining selected tensors at
  higher precision than a uniform four-bit conversion. For Qwen3.8-27B on DGX
  Spark, the llama.cpp maintainer's exact recommendation is a Q4_K_M target with
  the dedicated Q4_0 MTP draft:
  <https://github.com/ggml-org/llama.cpp/discussions/27080>.
- MLX, NVFP4, AWQ, GPTQ, and experimental INT4 builds target other runtimes or
  add less-proven compatibility and quality trade-offs.

The three selected checkpoints are:

| Role | Immutable checkpoint | Hub size | Why |
|---|---|---:|---|
| Recommended coding/agent default | `ggml-org/Qwen3.8-27B-GGUF@0669b98607d47046c7c2b3f801011d54a08cfccf`, target `Qwen3.8-27B-Q4_K_M.gguf`, draft `mtp-Qwen3.8-27B-Q4_0.gguf` | 19.24 GiB target + draft | Exact llama.cpp maintainer recommendation for MTP on DGX Spark; native long context, vision, and preserved reasoning |
| Best throughput/quality balance | `ornith-ai/Ornith-1.5-35B-A3B-GGUF@12393612fd4f730ff5aadc23e9b8f9648aa49ceb`, file `Ornith-1.5-35B-Q4_K_M.gguf` | 20.22 GiB | Publisher-owned Q4 GGUF; no publisher-owned Q4_K_XL artifact exists yet |
| Visual/UI specialist | `lactroiii/Muse-Glimmer-30B-GGUF@c8e212a87fbc137e44463663fb7550ae92079849`, file `Muse-Glimmer-30B-KQuant-Dynamic-Q4_K_XL.gguf` | 18.30 GiB | Byte-identical mirror of Meta's dynamic Q4_K_XL build; add the 1.30 GiB projector for vision |

Hub sizes and revisions are read from the Hugging Face model API and must be
re-resolved only through an explicit model update operation.

### Technical context

Qwen3.8-27B is a dense hybrid model with 48 Gated DeltaNet layers, 16 full
attention layers, a vision encoder, and a 262,144-token native context. The
selected ggml-org repository publishes the Q4_K_M target, dedicated Q4_0 MTP
draft, and model projector under one immutable revision:
<https://huggingface.co/ggml-org/Qwen3.8-27B-GGUF>.

Ornith-1.5-35B-A3B is a Qwen3.5-family MoE with roughly 35B total and 3B active
parameters. Its publisher-owned GGUF currently provides Q4_K_M rather than
Q4_K_XL, so the publisher artifact is preferred over an unrelated conversion:
<https://huggingface.co/ornith-ai/Ornith-1.5-35B-A3B-GGUF>.

Muse Glimmer is a dense 30B multimodal model with a 2B vision encoder and 28B
text decoder. Meta publishes a dynamic Q4_K_XL GGUF and a separate projector;
the catalog pins a public mirror with identical declared sizes and SHA-256 values
because the publisher resolver returns a regional 403 from `dgx-spark`;
llama.cpp build 10353 or newer provides its channel-aware reasoning and tool
parsing: <https://huggingface.co/meta-models/Muse-Glimmer-30B-GGUF>.

### Relevant runtime failures

- Tagged vLLM 0.27.0/0.27.1 does not contain native Muse Glimmer model and
  reasoning/tool-parser support. Muse therefore needs a separately pinned
  vLLM 0.28 engine profile rather than silently falling back to a generic text
  profile: <https://github.com/vllm-project/vllm/issues/52594>.
- Muse DFlash speculative decoding has had multiple correctness/startup defects;
  it must remain disabled until a pinned release passes sy's streaming and tool
  E2E suite: <https://github.com/vllm-project/recipes/issues/782>.
- Qwen3.8 DFlash defects reported against vLLM do not validate llama.cpp's
  separate `draft-mtp` path. The pinned llama.cpp image exposes native MTP, and
  the exact Spark recipe reports higher accepted-token throughput:
  <https://github.com/ggml-org/llama.cpp/discussions/27080>.
- A DGX Spark report shows that priority scheduling can eliminate hybrid-model
  prefix-cache hits. Keep the default FCFS scheduler:
  <https://github.com/vllm-project/vllm/issues/52897>.
- NVIDIA's Q4_K_XL recommendation is explicitly for the CUDA llama.cpp serving
  path. It is not a claim that GGUF is the best checkpoint for vLLM.

## 3. Proposal

### Approach

Add the three immutable identities as ordinary downloadable models. Model
metadata selects declarative engine profiles; no model name, checkpoint,
sampling value, or engine argument is embedded in Rust.

### Key decisions

| Decision | Choice | Reasoning | Alternatives |
|---|---|---|---|
| Qwen checkpoint | ggml-org Q4_K_M target plus Q4_0 MTP draft | Exact Spark recipe improves decode while keeping target quality and reasoning | Official FP8 remains the vLLM reference |
| Ornith checkpoint | Publisher Q4_K_M GGUF | Closest trusted Q4 artifact; no publisher Q4_K_XL exists | Official FP8 remains the vLLM reference |
| Muse checkpoint | Publisher dynamic Q4_K_XL plus projector | Meta supplies an architecture-specific higher-quality Q4 build | Red Hat FP8 remains the vLLM reference |
| Primary runtime | Pinned CUDA llama.cpp | NVIDIA's validated Spark path and native GGUF engine | vLLM is the fallback/reference engine |
| Speculation | Qwen llama.cpp `draft-mtp` with `--spec-default` | Exact maintainer recipe, dedicated verified draft, and no runtime Hub access | Disable by removing the model profile or draft role in configuration |

### Scope

- Immutable repository revisions and friendly aliases.
- Declarative profiles for Qwen3.8/Ornith and Muse Glimmer.
- OpenAI chat/responses and Anthropic messages streaming.
- Reasoning and tool-call parsers appropriate to each family.
- Text and image capability publication matching actual E2E evidence.
- Real Spark download, cold serve, restart recovery, Claude launch, OpenCode
  launch, tool round-trip, and post-run health verification.

### Anti-goals

- Do not select INT4/NVFP4 as defaults because none offers enough validated
  quality and protocol benefit to justify the additional risk here.
- Do not enable DFlash or speculation for other models without the same pinned
  artifact, streaming, cancellation, and tool-call evidence.
- Do not extend context to one million tokens; it materially reduces concurrency
  and has no demonstrated coding-agent benefit on this single-GPU appliance.

## 4. Technical design

### Architecture

The model catalog owns repository, revision, exact filename, projector, alias,
size, and verified capabilities. Declarative engine configuration owns runtime
selection and all arguments. Rust continues to select profiles from model
metadata and validates configuration without recognizing model names.

Recommended initial profiles:

- Qwen3.8: pinned CUDA llama.cpp with its Q4_K_M target, Q4_0 MTP draft,
  projector, preserved reasoning, and `draft-mtp`; no reasoning budget.
- Ornith: pinned CUDA llama.cpp with its checkpoint chat template, projector
  when vision is advertised, and one initial slot.
- Muse: llama.cpp build 10353 or newer, dynamic Q4_K_XL, projector, Jinja chat
  template, and no DFlash.
- Every model retains an FP8/vLLM challenger for protocol and quality comparison.

### Shipped configuration contract

The executable contains parsers, never operational catalog bytes. A separately
signed `SHA256SUMS` inventory covers `sy-aarch64`, `models.toml`, and every
sorted `engines/*.toml` file discovered in the release; activation verifies the
exact set, retains the coherent bundle with the release, then installs
`sy.spark.models/v2` at `/etc/sy/spark/models.toml` and one strict
`sy.spark.engine/v3` declaration per file under `/etc/sy/spark/engines/`. Model declarations own immutable Hub
revisions, exact files, typed auxiliary roles, quantization, and capabilities.
Engine declarations match only those artifact traits and own image identity,
arguments, mounts, resource envelopes, probes, routes, and profiles.

The GGUF primary is llama.cpp build `b10524` (upstream commit
`9ee9fc04c136ef2ae729bfc60d18961b23c13ddf`) in the signed arm64 CUDA 13 image
`ghcr.io/ggml-org/llama.cpp@sha256:1a9e22a3ab130c186f632fef78c8b0bf8aea5585a6795bf9021ca447c9bf335d`.
The explicit safetensors fallback remains `vllm/vllm-openai` 0.27.1 at digest
`sha256:ae35bb2db70814d1239ea588e1abb5288adcbd287cac1d4d00ea0f28cd2033df`;
there is no silent fallback from one engine to the other.

### Real-device evidence status

Step 9 verified every pinned Q4 artifact on `dgx-spark` with the signed
llama.cpp image. The engine-side TTFT estimate is logged prompt evaluation plus
one logged decode-token duration; it excludes the gateway envelope.

| Model | Cold/restart | TTFT | Decode | Startup/steady evidence | Published protocol evidence |
|---|---|---:|---:|---|---|
| Qwen3.8-27B | 16.671 s cold | 492.45 ms | 11.75 tok/s | cgroup peak 9,509,982,208 B; steady 9,507,266,560 B; GPU 21,560 MiB | OpenAI and Anthropic reasoning, text, two parallel tools, usage, finish, and image=`black` |
| Ornith-1.5-35B-A3B | 18.002 s cold; 5.590 s restart | 90.33 ms | 80.42 tok/s | cgroup peak 24,345,657,344 B; steady 24,171,687,936 B; GPU 22,823 MiB | OpenAI and Anthropic reasoning, text, two parallel tools, usage, finish, and image=`black` |
| Muse Glimmer 30B | 20.225 s cold; 19.808 s restart | 427.78 ms | 10.65 tok/s | restart cgroup peak 1,537,896,448 B; steady 1,533,771,776 B; GPU 21,073 MiB | OpenAI and Anthropic reasoning, text, two parallel tools, usage, finish, and image=`black` |

Real evidence changed only generic declarative tuning: context is 65,536 for a
real 40,750-token Claude request, the signed vision fixture is a deterministic
224x224 opaque-black PNG, and its bounded response budget is 256 tokens. The
generic readiness adapter uses low reasoning and portable required-tool
semantics. No model or repository name was added to Rust.

### Non-functional requirements

- Performance: expose time-to-first-token and decode tokens/second separately;
  retain at least 8 GiB system reserve and the existing emergency floor.
- Reliability: exact-image and exact-revision startup; no route publication
  before semantic and streamed protocol probes pass.
- Security: existing non-root container, read-only model mount, scoped bearer,
  route allowlist, and signed configuration boundaries remain unchanged.
- Observability: record selected profile, engine fingerprint, checkpoint
  revision, quantization, startup peak, steady peak, TTFT, and decode rate.

### CLI surface

No new command is needed. The models use existing `download`, `serve`, `ps`,
`stop`, and `launch` commands with aliases. JSON schemas and exit codes remain
unchanged.

### Testing strategy

- Unit: metadata-to-profile selection and parser/argument projection.
- Integration: OpenAI and Anthropic streamed reasoning, text, tools, and image
  requests through the real gateway.
- E2E: each checkpoint on `dgx-spark`, followed by real Claude and OpenCode
  headless turns and confirmation that the same generation remains healthy.
- No stress or soak gate is required.

### Upgrade policy

The signed external release payload owns the complete model and engine catalogs
and replaces their installed bytes after signature, hash, and schema validation.
Changing catalogs and rebuilding/signing the bundle requires no Rust change or
recompile. Deprecated schemas, legacy
recipe directories, and unavailable active engine identities are rejected
explicitly rather than merged or decoded through compatibility paths. Updating
either engine container does not update the DGX Spark platform.

### Dependencies

No Rust dependency is required. A signed CUDA llama.cpp aarch64 engine image is
required. The signed vLLM 0.27.1 image is an explicitly selected FP8 reference;
Muse FP8 is not qualified until a pinned compatible profile passes the same
real-device protocol suite.

## 5. User journey sketch

1. User downloads one recommended alias; sy resolves and records the immutable
   checkpoint revision.
2. User serves it; metadata selects the correct declarative engine profile.
3. Semantic and protocol probes pass before the route is published.
4. User launches Claude or OpenCode and receives a streamed tool-capable turn.
5. `ps` shows the same healthy generation and its engine/checkpoint identity.

### Friction map

| Friction | Phase | Opportunity |
|---|---|---|
| Large checkpoint transfer | Download | resumable content-addressed blobs and exact progress |
| GGUF needs a llama.cpp engine | Serve | automatic signed profile selection |
| First-run kernel warmup | Serve | truthful startup operation progress |
| Quantization quality uncertainty | Verification | compare semantic/tool probes to BF16 reference |

## 6. Risks and mitigation

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| Q4 quality loss | weaker agent decisions | medium | compare against official FP8 reference on real tasks |
| Muse parser/runtime immaturity | malformed reasoning or tools | medium | llama.cpp 10353+, no DFlash, protocol fixtures |
| Hybrid cache/scheduler defect | lost throughput | medium | FCFS, no MTP, expose cache metrics |
| Model revision drift | unreproducible behavior | high if unpinned | immutable commit in catalog |

## 7. Open questions

None required for checkpoint selection. Performance tuning values remain
evidence produced by each real Spark serve, then recorded in configuration.

## 8. Hand-off

- Expand the journey with `/journey` if these checkpoints are to become shipped
  aliases.
- Use `/roadmap` before implementation.
- Use `/implement` for one configuration/runtime/E2E step at a time.
