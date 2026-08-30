# SPEC: DGX Spark remote model serving

| Field | Value |
|---|---|
| Status | Research complete; key decision register closed; ready for `/journey` |
| Date | 2026-08-23 |
| Target | `dgx-spark` (`DGX Spark`, ARM64, GB10) |
| Product surface | `sy spark <host> ...`, HTTPS control API, OpenAI Responses- and Anthropic Messages-compatible inference gateway |

### Decision-review contract

The evidence and real-host observations in this document are research results.
Interactive approval is required only for a **key decision** that materially
changes externally visible behavior, a trust boundary, supported scope, or an
irreversible/destructive host action. Those decisions have stable IDs and are
reviewed one at a time. Choices already accepted by the user remain normative.

The researcher owns implementation mechanisms, dependency/crate selection,
timeouts, sampling, threshold mechanics, validation fixtures, and measured
tuning defaults. These are recorded as evidence-backed engineering
recommendations and must pass the stated real-Spark gates; they are not turned
into a questionnaire. Empirical values that can only be learned from the Spark
remain evidence gates rather than invented product policy. Functionality belongs
in cohesive Scope or in Anti-Goals for a substantive reason. `/journey`, `/roadmap`, and
implementation remain blocked only until the pending key-decision rows are
closed.

## 1. Summary

Add a `spark` plane to `sy` with an Ollama-shaped lifecycle and a deliberately
different execution architecture:

- `sy spark <host> download|ls|serve|ps|stop` is the simple human and agent
  surface requested by the user.
- An unprivileged `sy spark agent` owns HTTPS, authentication, model downloads,
  desired state, operation progress, and the inference gateway.
- A root-only `sy spark executor` has Docker authority but no network listener.
  It accepts a small typed protocol over a Unix socket and constructs Docker
  operations exclusively from root-owned, versioned recipes.
- Inference engines run in digest-pinned containers attached only to managed
  Docker-internal bridge networks, publish no host ports, and mount immutable
  model snapshots read-only. The host agent reaches their container addresses;
  remote clients use stable, authenticated OpenAI- and Anthropic-compatible
  gateway paths.
- Engine choice and tuning are driven by exact recipes and locally verified
  functional evidence. A compatible winner is preferred; absent one, the model
  visibly falls back to a verified vLLM recipe. There is no universal engine for
  Spark: TensorRT-LLM, vLLM, SGLang, llama.cpp, NIM, and Rust-native candidates
  support different model formats and capabilities.
- The control plane is Rust-first but not crate-maximalist: it reuses the
  workspace's IPC, systemd, tracing, metrics, Linux, crypto, and signature
  mechanisms; adopts narrow maintained crates for HTTP, OpenAPI, SQLite,
  migrations, Docker, Hub access, rate limiting, and certificates;
  and keeps only Spark-specific policy and reconciliation in `sy`.

The critical design result is that the network-facing process must not possess
Docker authority. Membership in the Docker group or access to the Docker socket
is effectively root access; exposing either through an HTTP agent turns a parser
or authentication defect into full-host compromise. Docker documents this
explicitly in its [daemon attack-surface guidance](https://docs.docker.com/engine/security/)
and [Linux post-install guidance](https://docs.docker.com/engine/install/linux-postinstall/).

The second critical result is that admission cannot trust a single engine memory
percentage on GB10. CPU, GPU, filesystem cache, weights, and KV cache contend for
one unified memory pool. NVIDIA's [Spark optimization guide](https://docs.nvidia.com/dgx/dgx-spark-porting-guide/optimization.html)
warns that CUDA-reported free memory does not capture reclaimable host cache and
swap behavior, while a current [vLLM Spark failure report](https://github.com/vllm-project/vllm/issues/46307)
shows `gpu-memory-utilization` failing as a hard host-memory ceiling. The design
therefore combines conservative admission, live host pressure monitoring, and a
root-side emergency guard.

The third result is a sharp build/buy boundary. Mature Rust crates can safely buy
mechanisms—HTTP/TLS/OpenAPI, SQLite/migrations, Docker protocol, Hub/cache,
cryptography and rate limiting—but none can buy the product's core
semantics: desired versus observed Docker state, generation-safe reconciliation,
GB10 admission/emergency policy, exact recipe selection, route security, and
verified download promotion. Those remain small explicit `sy` modules. Even a
Rust inference server remains an untrusted OCI recipe, never an in-process part
of the root or network control plane.

### Real-host evidence

Read-only inspection of `dgx-spark` on 2026-08-23 established the deployment
boundary below. No package, service, image, configuration, or OS component was
changed during research.

| Area | Observed value | Design consequence |
|---|---|---|
| Platform | DGX Spark software build 7.5.0; Ubuntu 24.04; ARM64 | Build a dedicated ARM64 `sy` artifact; do not alter the validated base stack |
| Kernel/driver | `6.17.0-1022-nvidia`; NVIDIA 580.159.03 | Pin user-space engine images compatible with the installed driver |
| Accelerator | NVIDIA GB10 | Use Spark/SM121-specific recipes; generic CUDA recipes are insufficient |
| Memory | 119 GiB visible RAM; 16 GiB swap; 115 GiB available when idle | Treat swap as emergency margin, never model capacity |
| Storage | ext4 NVMe; about 793 GiB free | Keep the Hugging Face cache on the local NVMe; enforce free-space reserves |
| Container stack | Docker 29.2.1; NVIDIA Container Toolkit 1.19.0; Docker active | Reuse the installed runtime without updating it |
| Docker privilege | Login user cannot use the socket; root can | Preserve that boundary; do not add the user or agent to the Docker group |
| Resource controls | cgroup v2 with memory, I/O, PIDs, and `dmem`; `systemd-oomd` inactive | Apply cgroup limits as defense in depth, with an independent memory guard |
| Network | Wi-Fi address `10.1.30.143`; Ethernet and ConnectX down | Bind one configured address, never auto-bind all interfaces |
| Firewall | UFW reports inactive; firewalld inactive | HTTPS and strong authentication are mandatory even on the current LAN |
| CPU/VM | CPU governors already `performance`; THP `madvise`; pressure idle | Do not apply generic sysctl/governor “optimizations” |
| Python | Python 3.12 present; no Hugging Face client or `uv` | Use Rust `hf-hub` normally; reserve a hash-locked isolated Python client as a removable HTTP-transfer fallback; never mutate system Python |

The inventory aligns with NVIDIA's current
[DGX Spark software model](https://docs.nvidia.com/dgx/dgx-spark/software.html),
[container-runtime contract](https://docs.nvidia.com/dgx/dgx-spark/nvidia-container-runtime-for-docker.html),
and [release notes](https://docs.nvidia.com/dgx/dgx-spark/release-notes.html).
Those sources reinforce containerized user-space frameworks on top of the
validated host driver rather than replacement of the appliance stack.

## 2. Background & Research

### Market Context

| Approach | Strength | Fatal or material weakness | Decision |
|---|---|---|---|
| SSH for every command | No resident control API; reuses existing trust | Weak asynchronous semantics, hard progress/cancellation, brittle quoting, and poor agent integration | SSH is bootstrap and break-glass transport only |
| Install Ollama directly | Familiar commands and compact lifecycle | Does not consistently expose the best Spark-native kernels, formats, and exact NVIDIA recipes | Copy the interaction model, not the serving engine |
| One HTTP daemon with Docker socket | Small implementation | A remote-code path to root; a compromised model-serving route can own the host | Rejected |
| Root HTTP daemon with an operation allowlist | Easier than two processes | Network parser, TLS, and proxy remain inside the root trust boundary | Rejected |
| Unprivileged gateway plus local root executor | Separates remote input from Docker authority; supports durable orchestration | Adds a narrow internal protocol and reconciliation logic | Selected |
| Kubernetes or Nomad | Mature scheduling and reconciliation | Disproportionate control-plane cost for one appliance; mutates more of the host | Out of scope |

[Ollama's API](https://github.com/ollama/ollama/blob/main/docs/api.md) usefully
separates downloaded models from running models. That distinction maps directly
to `ls` versus `ps`. Its storage and engine choices are not adopted.

Docker recommends restart policies instead of combining container restart with
a host-level process manager. The agent and executor are systemd services, while
desired-running engine containers use Docker's
[`unless-stopped` policy](https://docs.docker.com/engine/containers/start-containers-automatically/).
Systemd does not create one unit per model container.

### Technical Context

#### Serving-engine comparison

| Engine | Prefer when | Spark-specific concern | Role in `sy` |
|---|---|---|---|
| TensorRT-LLM | A tested NVIDIA recipe and optimized/quantized artifact exist | Narrower compatibility and build/conversion coupling | High-priority verified recipe for matching artifacts |
| vLLM | Broad Hugging Face compatibility and OpenAI serving matter | GB10/SM121 kernel gaps and unified-memory tuning vary by model | Broad-compatibility candidate only when an exact Spark recipe exists |
| SGLang | Prefix reuse, structured generation, and agentic request patterns require its scheduler features | Kernel and multi-node behavior must match the exact image | Candidate only with locally verified functional evidence |
| llama.cpp | GGUF artifacts or CPU/GPU offload are required | Some Spark-native artifact formats are unsupported | Explicit GGUF recipe family |
| mistral.rs | A supported architecture/format benefits from its Rust server, batching, paged attention, quantization, and CUDA path | Its broad default server surface includes UI, file, code/shell, MCP, and remote-loading features that this appliance must disable; exact GB10 behavior still needs local verification | Experimental recipe family eligible only through the same functional, safety, isolation, and durability gates |
| candle-vLLM | A Rust/Candle implementation has an exact locally verified capability advantage | Published evidence is for different hardware and the server surface is broader than needed | Watchlist recipe family; not enabled without an exact Spark gate |
| NVIDIA NIM | A supported, licensed profile exists and operational support matters | Registry credentials, profile availability, and licensing | Explicit opt-in recipe family, never a hidden fallback |

The [llama.cpp server reference](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)
defines its GGUF/OpenAI-compatible serving surface. NVIDIA's
[Spark NGC guidance](https://docs.nvidia.com/dgx/dgx-spark/ngc.html) defines NGC
authentication and NIM availability; neither source supports treating those
engines as invisible substitutes for a requested artifact.

NVIDIA publishes separate Spark playbooks for
[vLLM](https://github.com/NVIDIA/dgx-spark-playbooks/blob/main/nvidia/vllm/README.md),
[TensorRT-LLM](https://github.com/NVIDIA/dgx-spark-playbooks/blob/main/nvidia/trt-llm/README.md),
and [SGLang](https://github.com/NVIDIA/dgx-spark-playbooks/blob/main/nvidia/sglang/README.md).
Their commands are model-specific, not one generic launch line. Current vLLM
recipes combine such variables as FP8 KV cache, FlashInfer, Marlin, chunked
prefill, prefix caching, maximum sequence count, batched-token limits, and MTP.
These are compatibility-bearing recipe fields, not global defaults.

Rust-native serving is credible enough to test, but not credible enough to
replace evidence. [`mistral.rs`](https://github.com/EricLBuehler/mistral.rs)
publishes an OpenAI-compatible Rust server and CUDA batching/paged-attention
paths; its current release also exposes many agentic
and file/code-execution routes that are intentionally outside this product.
[`candle-vLLM`](https://github.com/EricLBuehler/candle-vllm) exposes a Rust/CUDA
OpenAI server with continuous batching and CUDA graphs, but its published
single-request table is for Hopper 80G rather than GB10. The former is therefore
a high-priority candidate and the latter a watchlist candidate, both isolated in
OCI and both disabled until an exact recipe passes the same local gate as the
established engines. [`atoma-infer`](https://github.com/AtomaAI/atoma-infer) is
rejected because its own status says it has no verified build, test, or serving
baseline and is not production-ready. Candle and Burn themselves are framework
libraries, not complete appliance servers; embedding either into the privileged
control plane would combine two failure domains without buying lifecycle
semantics.

NVIDIA's Spark inference guidance shows why compatibility evidence must name the
model, precision, artifact format, and supported context. Upstream recipe claims
are inputs to local verification, never acceptance results.

NVFP4 can reduce memory, but it is not a free format conversion. It requires
quality evaluation under NVIDIA's
[NVFP4 quantization playbook](https://github.com/NVIDIA/dgx-spark-playbooks/blob/main/nvidia/nvfp4-quantization/README.md).
Quantization is selected only by a verified artifact recipe whose quality gate
passes; `sy` does not quantize an arbitrary requested model during `serve`.

#### Primary acceptance model

The user-selected primary fixture is
[`ornith-ai/Ornith-1.5-9B`](https://huggingface.co/ornith-ai/Ornith-1.5-9B),
exposed locally as the Ollama-shaped alias `ornith-1.5:9b`. It is a recent MIT-
licensed 9B dense Qwen3.5-derived multimodal reasoning model whose model card
documents vLLM and SGLang serving, image inputs, separate reasoning output, and
OpenAI-style tool calls. It therefore exercises the accepted coding-agent and
vision contracts with a vendor-neutral artifact suitable for bounded
single-Spark functional tests.

Its recency is also a compatibility risk, not permission to relax gates. The
upstream card currently requires recent vLLM/SGLang versions plus exact
reasoning and tool parsers, and its vLLM example enables remote code. The Spark
recipe must freeze the model commit, runtime image digest, processor/chat
template, parser names, and any repository code; `--trust-remote-code` is
allowed only after the exact code is reviewed, hashed, copied into the recipe
provenance, and isolated under the existing engine policy. Failure to reproduce
the recipe on GB10 blocks acceptance rather than selecting a similarly named
artifact.

### Deep Dives

#### Compatibility is an exact fingerprint

Recent upstream failures demonstrate that an engine name and semver are not a
sufficient compatibility contract:

- vLLM has reported missing [NVFP4 kernels for SM121](https://github.com/vllm-project/vllm/issues/50925),
  a [sparse-attention gap on Spark](https://github.com/vllm-project/vllm/issues/45317),
  [FlashInfer plus MTP failures](https://github.com/vllm-project/vllm/issues/37754),
  and [fatal errors after long service](https://github.com/vllm-project/vllm/issues/52877).
- SGLang has reported a [GB10 shared-memory kernel failure](https://github.com/sgl-project/sglang/issues/28019)
  and [masked multi-node failures](https://github.com/sgl-project/sglang/issues/32021).

A verified recipe is consequently keyed by all of:

```text
host software build + architecture + driver + GPU/SM
+ model repository + immutable commit + artifact hashes
+ engine image digest + managed-network policy version
+ recipe schema/version + upstream recipe commit
+ parser/tokenizer assets + relevant launch flags
```

A tag such as `latest`, an unpinned branch such as `main`, or merely the string
`vllm` cannot become a verified recipe. A changed fingerprint has no inherited
selection status.

#### Model storage should not be reinvented

Hugging Face already provides a content-addressed cache with `blobs`, `refs`,
and immutable `snapshots` assembled with symlinks. Reusing the documented
[language-neutral cache layout](https://github.com/huggingface/hub-docs/blob/main/docs/hub/local-cache.md)
deduplicates revisions and remains compatible with engine tooling; that reference
explicitly lists both Python `huggingface_hub` and Rust `hf-hub` as users. The
official Rust [`hf-hub`](https://github.com/huggingface/hf-hub) client becomes the
primary async snapshot/Xet mechanism rather than creating a second blob database
or making Python part of the agent's normal control path. Endpoint, cache root,
and fine-grained token are configured explicitly; implicit environment discovery
is not accepted for secrets or identity.

Resume support is not verification. A current
[DGX Spark/aarch64 report](https://github.com/huggingface/huggingface_hub/issues/4223)
shows Xet transfer errors leaving `.incomplete` blobs while the official Python
CLI exits zero. Every transport therefore ends with a Rust-owned check of the
resolved commit, expected repository tree, required files, declared sizes and
recipe hashes, snapshot symlink containment, and absence of incomplete entries.
Only that check can atomically make a model visible in `ls`.

Xet high-performance mode may use much larger buffers and saturate network, disk,
and CPU, while Hugging Face notes that the Xet chunk cache is normally best left
disabled for downloads in its
[environment-variable reference](https://huggingface.co/docs/huggingface_hub/en/package_reference/environment_variables).
The current research recommendation does not set `HF_XET_HIGH_PERFORMANCE` in
the resident agent and leaves the
chunk cache disabled. Rust `hf-hub` uses its default Xet path; downloads receive
lower CPU/I/O weight while inference is active. A bounded no-progress watchdog
classifies Xet transport/integrity failures and may switch once to the contained
official Python HTTP path with `HF_HUB_DISABLE_XET=1`; authentication, missing
revision, policy, and disk errors never trigger it. A future high-performance
mode requires a separately resource-bounded helper design and real-Spark memory
evidence, not a mutable process-global environment toggle.

#### Durability needs intent and observation

SQLite records desired state and operation history; Docker labels and events
record observable runtime identity; the Hugging Face cache records model bytes.
None is treated as a complete source of truth alone.

SQLite runs in WAL mode with `synchronous=FULL`. SQLite documents that FULL
synchronizes the WAL at each commit, whereas NORMAL can lose the newest commit
after power loss in its [WAL documentation](https://www.sqlite.org/wal.html).
The agent checkpoints on controlled shutdown and bounded thresholds, retains the
WAL/SHM files with the database, and reconciles labels after an unclean restart.

Long operations use durable resources rather than holding one HTTP request open.
Mutations accept `Idempotency-Key`, return `202 Accepted` plus a `Location`, and
expose polling and server-sent events. Semantics follow the current
[HTTPAPI idempotency-key draft](https://datatracker.ietf.org/doc/html/draft-ietf-httpapi-idempotency-key-header);
errors use [RFC 9457 Problem Details](https://www.rfc-editor.org/rfc/rfc9457.html).

### Build-versus-buy investigation

Here “buy” means adopt and pin an open-source component, not purchase a hosted
service. Components were evaluated against six gates: exact responsibility fit;
active maintenance and a compatible permissive license; Linux ARM64 and the
workspace MSRV; bounded features/dependency graph; observable failure and
recovery semantics; and security cost, including build scripts, native code,
FFI, `unsafe`, secrets, and network reach. A crate being written in Rust is a
preference, not permission to weaken the process boundary or skip a real GB10
measurement.

The recommendation vocabulary is provisional pending interactive approval:

- **Reuse** an existing `sy` capability and improve it generically when needed.
- **Adopt** a narrow Rust crate for a solved mechanism behind an in-tree trait or
  adapter owned by `sy`.
- **Build** only product policy, wire contracts, state machines, and the small
  glue whose invariants are unique to this appliance.
- **Contain** unavoidable native or non-Rust code behind one audited FFI or
  subprocess/container boundary with no authority expansion.
- **Reject/hold** a component when it duplicates a boundary, hides retry state,
  broadens attack surface, or lacks exact ARM64/GB10 evidence.

No new external control service is justified. Redis, PostgreSQL, NATS, an OAuth
server, a Prometheus daemon, and a generic reverse proxy would add independent
upgrade, credential, backup, and recovery domains to a single-node appliance.
The existing Docker daemon, systemd, journald, OpenSSH client, and local SQLite
file are the deliberate platform dependencies.

#### HTTP, API, authentication, and transport

| Responsibility | Options examined | Decision and owned boundary |
|---|---|---|
| HTTP routing/middleware | Raw Hyper; Actix Web; [`axum`](https://github.com/tokio-rs/axum); Cloudflare [`Pingora`](https://github.com/cloudflare/pingora) | **Adopt axum 0.8 + tower/tower-http.** It is a thin Hyper/Tokio layer, uses the workspace runtime, exposes standard Tower middleware, and forbids unsafe code. Raw Hyper would make ordinary extraction/routing/error work ours; Actix is credible but introduces a second framework idiom; Pingora is a broad proxy framework rather than a small appliance API. `sy` builds only request policy, problem documents, and the exact inference route map. |
| TLS accept/reload/drain | Hand-built `tokio-rustls`; nginx/Caddy; [`axum-server`](https://github.com/programatik29/axum-server) | **Adopt axum-server 0.8** with `tls-rustls-no-provider`, TLS 1.3, certificate hot reload, and graceful shutdown. Reuse the workspace's rustls `ring` provider instead of linking a second AWS-LC provider. A separate proxy service would duplicate identity, logs, limits, and lifecycle. |
| OpenAPI contract | Hand-maintained YAML; `schemars`; `aide`; [`utoipa`](https://github.com/juhaku/utoipa) + `utoipa-axum` | **Adopt utoipa.** Derive OpenAPI 3.1 from the same serde wire types and normalize it into a checked artifact. Do not ship Swagger/Redoc UI assets or a runtime `/openapi.json` route. Schema generation does not replace `deny_unknown_fields`, semantic validation, or compatibility tests. |
| Coding-client protocol adapters | LiteLLM or another gateway service; generic protocol crates; narrow in-tree adapters | **Build narrow OpenAI Responses and Anthropic Messages adapters over one typed internal inference model covering generation events and embedding results.** No maintained Rust component buys exact Codex/Claude Code compatibility without also adding a second routing, authentication, policy, and lifecycle service. The adapters cover streaming, image inputs, client-side tools, stop reasons, usage, errors, embeddings, and model identity; actual pinned Codex and Claude Code clients are the acceptance oracle. |
| Engine upstream | A generic proxy crate/service; caller-selected URL; direct Hyper connection | **Build a fixed bridge-network upstream adapter** with `hyper::client::conn::http1`, `hyper-util::rt::TokioIo`, and a bounded connection pool. It accepts only executor-observed endpoints tied to a managed container, generation, shared-network ID, and recipe-fixed port; callers cannot choose a destination. A generic proxy or extra relay service would add surface without strengthening the user-selected shared bridge. |
| Token verification/secrets | JWT/OAuth; Argon2; RustCrypto HMAC | **Adopt** RustCrypto [`hmac`](https://github.com/RustCrypto/MACs) over the existing `sha2`, [`secrecy`](https://github.com/iqlusioninc/crates/tree/main/secrecy), and OS RNG. Use `verify_slice` for constant-time HMAC-SHA-256 verification and zeroize transient secret values. Random bearer secrets are not passwords; JWT, OAuth, and memory-hard password hashing add no useful property here. |
| Rate and concurrency control | `tower-governor`; ad-hoc maps; [`governor`](https://github.com/boinkor-net/governor) core | **Adopt governor core** and build the small Tower middleware that returns `sy.spark.problem/v1`. Key verified requests by token ID plus scope, cap unknown-source traffic separately, periodically retain/shrink keyed state, and enforce a hard active-token cardinality limit. Tokio semaphores separately bound inference and mutation concurrency. |
| Local certificates | OpenSSL subprocess; external ACME; [`rcgen`](https://github.com/rustls/rcgen) | **Adopt rcgen in installer code only** for ECDSA P-256 CA/leaf material with explicit DNS/IP SANs. There is no public DNS/ACME dependency. The root-only CA signs; the unprivileged agent receives only its leaf key and chain. |

#### Persistence and privileged orchestration

| Responsibility | Options examined | Decision and owned boundary |
|---|---|---|
| Embedded state | SQLx SQLite; system SQLite; [`rusqlite`](https://github.com/rusqlite/rusqlite) | **Adopt rusqlite 0.40 with `bundled,backup`.** Bundling contains the one new control-plane C FFI and prevents host SQLite ABI/version drift without installing a package. A single dedicated database thread owns one connection and accepts bounded Tokio `mpsc` requests with oneshot replies; do not issue arbitrary `spawn_blocking` calls or add a pool. `sy` owns WAL/FULL, busy timeout, foreign keys, backup, and domain transactions. |
| Migrations | Hand-rolled version switch; Refinery; [`rusqlite_migration`](https://github.com/cljoly/rusqlite_migration) | **Adopt rusqlite_migration** for atomic, embedded, `user_version`-based mechanics. Current Refinery rusqlite bounds lag the selected rusqlite line, while its cross-database machinery is unnecessary. Product migrations are forward-only and immutable; migration validation plus a checked snapshot detects edits, and a verified online backup precedes application. `sy`, not the crate, owns N/N-1 release compatibility and rollback policy. |
| Durable jobs | [`effectum`](https://docs.rs/effectum/latest/effectum/); [`Apalis`](https://github.com/apalis-dev/apalis); in-tree state machine | **Build the operation journal and reconciler.** A generic queue can retry a closure, but cannot decide whether a Docker side effect already occurred, whether labels/generation match desired state, or whether cancellation is still safe. Automatic queue recovery would obscure the intent-versus-observation invariant. |
| Docker Engine API | Parse Docker CLI output; hand-code HTTP; [`Bollard`](https://github.com/fussybeaver/bollard) | **Adopt Bollard** with `default-features=false, features=["pipe"]`, an explicit `/var/run/docker.sock`, and a narrow in-tree `ContainerRuntime` trait. Its Tokio streams, events, stats, generated API 1.52 types, and version negotiation buy substantial protocol coverage. Disable HTTP/SSH/TLS discovery, BuildKit, WebSocket, attach, and unrelated features. Call `/version`, require each recipe's minimum API, and fail closed; the Docker CLI is break-glass diagnostics only, never a runtime fallback. |
| Root RPC | New gRPC/tonic; tarpc; existing `crates/sy-ipc` | **Reuse and generically extend `sy-ipc`.** It already supplies typed length-delimited JSON, IDs, deadlines, cancellation, and streaming over Unix sockets. Replace its hard-coded same-eUID check with an injected `PeerAuthorizer` while retaining same-eUID as the default. The Spark executor policy accepts exactly the installed numeric `sy-spark` UID, records peer PID/UID, and still revalidates every closed method. Tonic/protobuf/HTTP2 and tarpc would create a second wire stack without improving the local trust decision. |

The database actor and operation state machine are deliberately separate. The
actor serializes durable SQLite work; the state machine serializes *meaning* and
records intent before asking the root executor for an external side effect.
Neither Bollard nor a job queue becomes the source of truth.

#### Model acquisition and SSH bootstrap

| Responsibility | Options examined | Decision and owned boundary |
|---|---|---|
| Hugging Face Hub/cache | Custom HTTP/Xet/cache client; Python `huggingface_hub`; official Rust [`hf-hub`](https://github.com/huggingface/hf-hub) 1.x | **Adopt Rust `hf-hub` as the primary client.** It provides async snapshot/cache and Xet support plus structured errors; configure endpoint, token, and cache explicitly because the library intentionally does not consume environment variables. `sy` owns allowlisted repository identity, progress/no-progress deadlines, disk admission, revision/tree/hash verification, and atomic visibility. Do not build Hub or Xet protocols. |
| Transfer fallback | Retry Rust Xet forever; trust helper exit status; pinned official Python client | **Contain one hash-locked Python `huggingface_hub` venv as a removable HTTP-only fallback.** A reported DGX Spark/aarch64 failure can leave `.incomplete` blobs after Xet errors while `hf download` exits zero; [`HF_HUB_DISABLE_XET=1` is the stable workaround](https://github.com/huggingface/huggingface_hub/issues/4223). Invoke fixed argv without a shell, expose only a credential *path*, and apply the same Rust-side manifest verification. Fallback occurs only for classified Xet transport, integrity, or no-progress failures—not auth, 404, policy, or disk errors—and is recorded in operation events. |
| SSH client | Native [`russh`](https://github.com/Eugeny/russh); libssh2 FFI via [`ssh2`](https://github.com/rust-lang/ssh2-rs); [`openssh`](https://docs.rs/openssh/latest/openssh/) wrapper; system OpenSSH | **Reuse the installed `ssh`/`sftp` executables through a typed Rust subprocess adapter for bootstrap only.** This preserves `dgx-spark` alias resolution, `known_hosts`, agent/hardware-token, keyboard-interactive, and password/sudo prompts. The Rust `openssh` wrapper is passwordless-only, while russh/ssh2 would duplicate mature host-key/auth configuration or add FFI for a handful of calls. Never use `sshpass`, pass a password in argv, or interpolate an arbitrary remote command; upload verified files and invoke fixed literal bootstrap entrypoints. |

The Python fallback is removed only by a separately approved change after the
Rust client exposes a supported way to select non-Xet HTTP transfer and passes
bounded interrupted/resume/integrity fixtures on the real DGX Spark. Until then,
filesystem state, the resolved commit, expected tree, file sizes/hashes, and the
absence of `.incomplete` entries—not process exit status—define success.

#### Linux integration, observability, compatibility, and supply chain

| Responsibility | Decision |
|---|---|
| systemd lifecycle | **Reuse** existing `sy_core::notify` support for `READY`, `STOPPING`, and watchdog notifications. Do not add a supervisor crate or process manager. |
| Logs and metrics | **Reuse** `tracing` + `tracing-journald` and the workspace `metrics`/Prometheus-over-UDS stack. Build only Spark metric definitions and bounded labels. Do not introduce OpenTelemetry or another metrics service. |
| Linux memory/pressure/files | **Reuse** `procfs` for memory/PSI where supported and `rustix` for descriptor-relative filesystem and cgroup operations; build strict parsers only for uncovered cgroup-v2 files. A broad system-information or cgroup framework would not remove the GB10-specific accounting policy. |
| Artifact verification | **Reuse** the workspace `minisign-verify` path for signed release/recipe manifests and exact OCI/model digests. Do not add a resident Sigstore/cosign service. |
| Dependency policy | **Adopt** [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) as a release-CI gate for licenses, advisories, bans, and sources, and [`cargo-auditable`](https://github.com/rust-secure-code/cargo-auditable) for dependency metadata in the ARM64 executable. Add the missing repository policy file; the Makefile's optional invocation is insufficient for this feature. |

Every new direct crate must be lockfile-pinned, permissively licensed, sourced
from crates.io or a separately approved immutable source, and build in ARM64 CI.
Default features are disabled where practical. Review records must inventory
features, duplicate major versions, build scripts, native libraries, `unsafe`,
binary-size/RSS delta, and unresolved RustSec/yanked/unmaintained advisories.
Exceptions are time-bounded and named. No application `unsafe` is authorized by
this design; bundled SQLite is the only new direct control-plane C FFI. Native
CUDA kernels and framework runtimes remain out of process in engine images.

#### Inference-engine buy decision

The inference server remains a recipe-selected external component, including
when that component is written in Rust. `mistral.rs` becomes a high-priority
experimental recipe family because it has relevant scheduler/quantization
capabilities; it does **not** become the universal default. Its exact OCI image,
model commit, CUDA/SM121 build, disabled-feature surface, output quality, memory
envelope, API behavior, and restart durability must satisfy the same functional
gates as a passing TensorRT-LLM/vLLM/SGLang/llama.cpp recipe for that model and
objective.

Rust-engine recipes receive only a local immutable snapshot, attach only to the
managed internal bridge without a published port, and must disable UI, remote downloads/media, file upload,
shell/Python/code execution, server-side agent loops, MCP, and multi-model hot
loading. Candle-vLLM remains a watchlist until equivalent GB10 evidence is
reproduced. Atoma-infer is rejected at its present readiness level. This is an
explicit **No** to embedding an inference framework inside the agent or executor:
language purity cannot outrank fault isolation, model
coverage, or least privilege.

## 3. Proposal

### Approach

Build a new remote `spark` plane in the same Rust package: an Ollama-shaped local
CLI talks HTTPS to an unprivileged agent, which persists intent and delegates a
closed set of Docker actions to a local root executor. Exact, root-owned recipes
map immutable model snapshots to digest-pinned Spark engine images. Engines use
managed Docker-internal networking without published ports, and the agent
provides the only authenticated remote inference endpoint.

The result makes acquisition, inventory, serving, discovery, tuning, recovery,
and stopping simple for both humans and coding agents without treating GB10 as a
generic CUDA host or mutating NVIDIA's validated appliance stack.

### Decision Proposals

No key-decision row remains pending. Accepted choices are normative;
engineering recommendations are owned by the research flow and validated
through the acceptance gates in this specification.

| ID | Decision | Recommended choice | Reasoning | Alternatives | Status |
|---|---|---|---|---|---|
| D01 | Privilege boundary | Unprivileged HTTPS agent plus root Unix-socket executor | Remote parsers never hold root-equivalent Docker authority | Docker group, passwordless sudo, or root HTTP daemon | **Accepted: A (2026-08-23)** |
| D02 | Engine-selection strategy | Exact model/host/image recipes—including gated Rust-native candidates—compete under a deterministic functional objective; `serve` prefers the winning verified recipe, with D07 defining the fallback | Spark compatibility and capabilities vary by artifact, kernel, and workload; implementation language is not compatibility evidence | One universal engine, or a required caller-selected engine | **Accepted: A, refined by D07 (2026-08-23)** |
| D03 | Model-storage layout | Native Hugging Face content-addressed blob/snapshot cache plus SQLite control metadata | Preserves cross-engine deduplication, immutable snapshots, and ecosystem compatibility while keeping desired state independent from model bytes | A `sy`-specific copied model store, or separate engine-owned caches | **Accepted: A (2026-08-23)** |
| D04 | Engine network boundary | Docker-internal bridge networking with no published engine ports; the host agent connects to engine container addresses | Keeps engines unreachable from the LAN while avoiding the network-namespace relay selected against by the user | `--network=none` plus relay, or host-loopback published ports | **Accepted: B (2026-08-23)** |
| D04a | Internal bridge topology | One user-defined internal bridge shared by every managed engine | Minimizes network objects and lifecycle work; the accepted residual trade-off is that managed engines can reach one another's container ports | One isolated internal bridge per served instance | **Accepted: B (2026-08-23)** |
| D05 | Serving durability semantics | A served instance remains desired-running until explicit `stop` and automatically returns after agent, executor, Docker, or host restart | Matches a durable appliance service and makes transient control-plane or host failure transparent once health is restored | Require `serve` after host reboot, or make persistence selectable per instance | **Accepted: A (2026-08-23)** |
| D05a | Durable-state authority | SQLite WAL/FULL stores desired intent and operation history; Docker labels, events, and full scans provide observed state for generation-safe reconciliation | Separates requested state from fallible external side effects and can recover across partial failures | Docker labels/restart policy as the sole state, or generated systemd units per model | **Accepted: A (2026-08-23)** |
| D06 | GB10 admission accounting | Serialize starts and admit against the aggregate measured cold-start envelopes of all desired-running models plus live `MemAvailable`, PSI, and swap state; fail closed when required safety telemetry is unavailable | GB10 CPU, GPU, cache, weights, and KV allocations contend for unified memory, so one engine percentage is not a host limit | Static recipe budgets only, or engine/CUDA-reported free memory only | **Accepted: A (2026-08-23)** |
| D06a | Emergency memory action | Protect host availability by stopping the newest starting/tuning managed engine first and, if critical pressure persists, the most recently started growing managed engine; suppress automatic restart and record the cause | Proactive shedding is deterministic and confined to managed workloads, unlike an indiscriminate kernel OOM kill | Stop transitional engines only, or alert without stopping any running engine | **Accepted: A (2026-08-23)** |
| D06b | Safety-threshold ownership | Store explicit admission reserve, emergency floor, PSI window, and persistence count in the declarative host policy; tuning never rewrites them | Safety behavior remains deterministic, reviewable, dry-run visible, and independent from engine selection | Let tuning rewrite thresholds automatically, or allow each `serve` request to override them | **Accepted: A (2026-08-23)** |
| D06c | Initial admission reserve | Preserve at least 8 GiB `MemAvailable` after admitting aggregate cold-start envelopes | User-selected capacity-first policy; this leaves only one sixteenth of the 128 GiB unified pool for the OS, control plane, filesystem activity, and allocation variance | Research recommendation: 16 GiB; safer option: 24 GiB; intermediate option: 12 GiB | **Accepted: custom 8 GiB (2026-08-23)** |
| D06d | Initial emergency `MemAvailable` floor | 8 GiB, equal to the selected admission reserve | Enforces the chosen reserve under observed runtime growth; three consecutive low samples limit reaction to sustained breach | 6 GiB for 2 GiB grace, or 4 GiB for 4 GiB grace | **Accepted: A (2026-08-23)** |
| D07 | Selection UX and fallback | `serve` uses an exact compatible winner when available; otherwise it starts a compatible verified vLLM recipe as the visible fallback. `tune` remains explicit and optional | Starting a broadly supported model stays simple and immediate, while tuning can select another engine only from locally verified functional evidence | Refuse until tuned, or auto-tune during `serve` | **Accepted: vLLM fallback (2026-08-23)** |
| D08 | Installation boundary | Versioned application-owned release over SSH with protected-version assertions; never update DGX OS, driver, runtime, Docker, toolkit, firmware, or system Python | Adds the feature without mutating the validated appliance stack | Host package/runtime upgrades, which violate the user's explicit restriction | **Accepted user constraint; mechanism is an engineering recommendation** |
| D09 | Component boundary | Reuse workspace primitives, adopt narrow Rust mechanisms, build Spark domain semantics, and contain unavoidable SQLite/Python/CUDA boundaries | Fulfils the delegated build/adopt research while minimizing original security-critical code | A hand-built stack or external control services | **Engineering recommendation** |
| D10 | Remote-access trust boundary | Direct HTTPS on one explicitly configured Spark LAN address, with an SSH-delivered local-CA pin and scoped tokens | Preserves a persistent HTTP API without a third-party network dependency while authenticating both endpoint and caller | Tailscale-only HTTPS, or a loopback agent reached only through an SSH tunnel | **Accepted: direct LAN HTTPS (2026-08-23)** |
| D11 | Inference API scope | OpenAI Responses-compatible and Anthropic Messages-compatible APIs, including SSE streaming and client-side tool calls, plus the native `sy spark` lifecycle API | Lets Codex use its supported custom-provider Responses wire protocol and Claude Code use its supported custom Messages base URL | OpenAI only, Anthropic only, or Ollama HTTP compatibility | **Accepted: OpenAI + Anthropic for Codex and Claude Code (2026-08-23)** |
| D12 | Model capability scope | Text generation and tool calling, vision-language image inputs, and text embeddings, each gated by exact recipe capabilities and end-to-end tests | Covers coding agents, image understanding, and semantic retrieval without claiming unrelated media-generation support | Text/tool calling only, or only one of vision and embeddings | **Accepted: include vision and embeddings (2026-08-23)** |

### Scope

- Workstation host profiles, SSH bootstrap, CA pinning, scoped credentials, and
  the complete CLIG/JSON client surface.
- ARM64 `sy spark agent` and `executor` subcommands, feature-gated from
  desktop-only dependencies and installed as declarative system services.
- TLS control API, asynchronous/idempotent operations, resumable events,
  OpenAI Responses and Anthropic Messages adapters, inference gateway, route
  policy, rate/concurrency limits, and stable schemas.
- Root-owned recipe catalog, exact compatibility fingerprints, selection,
  digest-pinned engine images, health semantics, and provenance.
- Rust `hf-hub` snapshot download/resume/verification/deduplication, contained
  HTTP-only fallback, immutable model identity, aliases, credentials, removal,
  and disk reserve.
- TensorRT-LLM, vLLM, SGLang, llama.cpp, NIM, and gated Rust-native recipe
  families where exact compatible evidence exists; explicit unsupported,
  watchlist, and experimental behavior.
- Text/tool generation, vision-language image inputs, and text embeddings as
  first-class recipe capabilities with protocol-native limits, identity, and
  health probes.
- Transactional serve/stop, engine containers, Docker labels/events/restart
  policy, crash/reboot reconciliation, cancellation, and bounded crash loops.
- Unified-memory admission, independent emergency guard, cgroup defense in
  depth, pressure/swap/thermal observation, and safe multi-instance accounting.
- Fixed functional objectives, bounded compatibility evaluation, quality,
  safety, isolation and durability gates, compile caches, and fingerprint
  invalidation.
- Installer dry-run, atomic upgrade/rollback, schema migration/backups, systemd
  hardening/watchdogs, diagnostics, metrics, audit, and redacted logs.
- Hermetic unit/integration/container tests and a real-Spark functional/recovery
  acceptance matrix covering exact text/tool, VLM, and embedding
  snapshots.

### Anti-Goals

- No general remote shell, Docker API, arbitrary container launcher, or arbitrary
  engine arguments: each would bypass the typed root boundary.
- No multi-host scheduler or distributed training: this is one named inference
  appliance, and cluster orchestration would introduce the wrong control plane.
- No model fine-tuning or implicit quantization/conversion: those require a
  distinct artifact-quality and provenance workflow.
- No public-Internet serving: a single-owner LAN appliance is the security and
  operational boundary; Internet ingress belongs behind a separately managed
  production gateway.
- No automatic sysctl, swap, governor, THP, clock, power, firmware, driver, OS,
  Docker, or CUDA changes: they violate the validated DGX base and are not
  justified by model-specific evidence.
- No hidden or ad hoc engine/precision/revision fallback or unreviewed remote
  code: the sole automatic engine fallback is D07's compatible verified vLLM
  recipe, and its exact fingerprint is reported and persisted.
- No claim of universal Hugging Face compatibility: a model without an exact
  safe recipe fails with actionable compatibility evidence.
- No image/video generation, speech, or audio models: D12 selects image
  understanding and text embeddings, while generative media requires different
  APIs, resource accounting, safety controls, and engine families.
- No Ollama HTTP or MCP surface: the accepted OpenAI Responses and Anthropic
  Messages APIs are the coding-agent integration primitives; another protocol
  would duplicate the gateway without adding a required client.

### Product Contract

#### Required command surface

```text
sy spark <host> download <repo> [--revision <ref>] [--alias <name>]
                                [--update-alias] [--detach] [--dry-run]
sy spark <host> ls [--json]
sy spark <host> show <model> [--json]
sy spark <host> rm <model> [--yes] [--dry-run]

sy spark <host> serve <model> [--name <instance>] [--recipe <id>]
                              [--objective agent|interactive|long-context]
                              [--allow-unverified] [--detach] [--dry-run]
sy spark <host> ps [--json]
sy spark <host> stop <instance-or-model> [--timeout <duration>] [--dry-run]
sy spark <host> logs <instance> [--follow] [--json]

sy spark <host> recipes [<model>] [--json]
sy spark <host> bench <model-or-instance> [--recipe <id>] [--json] [--dry-run]
sy spark <host> tune <model> [--objective <name>] [--json] [--detach] [--dry-run]
sy spark <host> operations [<operation-id>] [--follow] [--json]
sy spark <host> operations cancel <operation-id> [--dry-run]
sy spark <host> status [--json]
sy spark <host> doctor [--json]
sy spark <host> client-config <instance> --client codex|claude-code [--json]
sy spark <host> token create ... [--dry-run]
sy spark <host> token revoke <token-id> [--dry-run] [--yes]
sy spark <host> token list [--json]
sy spark <host> cert status [--json]
sy spark <host> cert rotate [--dry-run] [--yes]
sy spark <host> install|upgrade|rollback [--dry-run] [--yes]
```

Mutating commands wait and render progress by default. `--detach` prints the
operation ID and exits after acceptance. Every output-producing command supports
`--json`; logs and progress stay on stderr in human mode. `serve` never downloads,
converts, evaluates, or tunes implicitly: missing prerequisites yield a command
the caller can run deliberately.

Every state-changing subcommand supports `--dry-run`, and every flag has a
documented `SY_SPARK_*` environment equivalent. Flags override environment,
which overrides declarative host/agent defaults.

`<model>` resolves to an immutable downloaded snapshot or an unambiguous local
alias. `<instance>` is the serving identity. By default the instance name is a
stable normalized model alias, so an identical `serve` is idempotent; `--name` permits
multiple configurations of one snapshot.

If that instance is already creating under the same canonical request, the CLI
attaches to its operation; if it is healthy with the same model/recipe/objective,
the request succeeds with the existing endpoint and no restart. A conflicting
model, recipe, objective, or resource policy for the same name returns `409`
with `stop`/`--name` remediation rather than replacing live service implicitly.

#### Core behavior

| Command | Meaning |
|---|---|
| `download` | Resolve a repository reference once, persist its commit, estimate space, and materialize/verify its snapshot without allocating GPU memory |
| `ls` | List complete local immutable snapshots, size, aliases, compatibility, and active references |
| `serve` | Select an exact tuned winner or the compatible verified vLLM fallback, pass admission, create one managed engine container on the shared internal bridge, wait for health, then publish its gateway endpoint |
| `ps` | Show desired and observed service state, engine/recipe, endpoint, health, uptime, resource envelope, and last failure |
| `stop` | Persist stopped intent, disable restart, drain, stop, and remove the matching managed engine without deleting the model |
| `rm` | Remove only unreferenced cache material after a dry-run report; never remove active model data |
| `bench` | Evaluate one exact installed recipe against bounded functional compatibility gates |
| `tune` | Evaluate bounded locally verified recipe candidates and persist a winning profile for one exact fingerprint |

There is no bare `sy spark <host> <arbitrary-command>` escape hatch. The
`{command}` placeholder means one of the documented subcommands.

#### Stable exit codes

| Code | Meaning |
|---:|---|
| 0 | Requested observation or mutation completed successfully |
| 1 | Unexpected local/client or remote internal failure not covered below |
| 2 | CLI usage or local configuration error |
| 3 | Remote request rejected by policy, compatibility, admission, or state precondition |
| 4 | Agent unreachable, TLS identity mismatch, or authentication failure |
| 5 | Accepted operation failed; operation record contains the durable cause |
| 6 | Operation was cancelled or exceeded its declared command timeout |

An HTTP `202` is not CLI success for the default blocking mode; the CLI follows
the operation to a terminal state. With `--detach`, successful acceptance exits
zero and prints the operation resource.

## 4. Technical Design

### Architecture

The feature lives in a new `src/spark/` module of the existing crate, with
separate client/wire, agent/API/gateway, state, recipe/selection, executor/Docker,
fixed engine-upstream, resource-guard, and SSH installer boundaries. Declarative assets live in
the repository:

```text
src/spark/{cli,client,wire,agent,state,recipe,executor,gateway,upstream,resources,install}.rs
configs/sy/spark/agent.toml and configs/sy/spark/recipes/*.toml
configs/systemd/system/sy-spark-{agent,executor}.service and sy-spark.target
configs/apparmor.d/ and configs/selinux/ policy selected for the detected LSM
tests/spark_*.rs and feature-gated real-Spark recipes
```

This is a new remote HTTPS/Unix-socket plane because neither `aiplane` nor
`knowledge` owns a remote NVIDIA appliance or Docker. It reuses the generic
`sy-ipc` framing for agent-to-executor RPC, but it does not join an existing
plane's daemon protocol, acquire the laptop NPU, add an `aiplane::Workload`, or
create an MCP server. Wire request/response types remain shared between client
and agent inside the module so CLI, OpenAPI, and API cannot drift.

#### Process and trust topology

```text
local workstation
  sy spark client
      │ HTTPS 1.3, pinned server identity, scoped bearer token
      ▼
DGX Spark
  sy spark agent                 unprivileged `sy-spark` user
    ├── control API + SSE
    ├── allowlisted inference gateway ───────► managed container IP:recipe port
    ├── SQLite desired state / operations              │
    ├── HF snapshot downloader                         │ shared Docker
    └── typed Unix-socket RPC                          │ internal bridge
              │                                        │ no published ports
              ▼                                        ▼
  sy spark executor              root, no network listener
    ├── root-owned recipe registry
    ├── Docker Engine access
    ├── network/container event and label reconciliation
    └── independent host-memory emergency guard
              │                                        ▲
              └────────────────────────────────────────┘

         Docker + NVIDIA runtime     existing, never upgraded by `sy`
```

Inference data is proxied through the unprivileged agent so every remote route
has one TLS and authorization boundary. Engines have addresses only on one
user-defined Docker `--internal` bridge and publish no host port. The gateway
streams directly to the executor-observed container endpoint and does not buffer
complete generations or perform another model invocation. Its correctness and
bounded-buffer behavior have explicit acceptance gates. Because D04a selected
one shared bridge, managed
engines can reach one another's exposed container ports; that lateral reach is
an accepted residual risk and is reported by `doctor` rather than misrepresented
as per-instance network isolation.

#### Local client

The workstation `sy` binary owns host resolution, TLS pin validation, token
loading, retries that preserve idempotency keys, progress rendering, JSON output,
and inference endpoint discovery. It never logs or places a token in process
arguments. The local CA certificate fingerprint is obtained through the already
trusted SSH bootstrap; it is not accepted on first use over the LAN.

Before an agent profile exists, `install` passes the supplied alias (including
the existing `dgx-spark`) to OpenSSH as a discrete argv value so normal
`~/.ssh/config`, `/etc/hosts`, known-host, and agent behavior apply. After
bootstrap, normal commands resolve only the explicit `spark.toml` profile. There
is no mDNS/subnet discovery or shell interpolation of hostnames.

Bootstrap uses installed `ssh` and `sftp` through a typed subprocess adapter,
not an SSH library or constructed shell command. Only fixed remote installer
entrypoints are legal. Authentication stays interactive when OpenSSH or sudo
requires it; `sshpass`, password-valued argv/environment variables, and stored
bootstrap passwords are prohibited.

Client configuration is declarative:

```toml
[hosts.dgx-spark]
url = "https://10.1.30.143:9843"
ca_cert_sha256 = "sha256:<fingerprint>"
credential = "spark/dgx-spark"
request_timeout_seconds = 30
```

The token is stored separately in a mode-0600 credential file under the `sy`
configuration directory. `SY_SPARK_TOKEN` is supported for ephemeral automation;
the normal configuration never embeds it in TOML.

#### Unprivileged agent

`sy spark agent` is the only network listener. It owns:

- TLS termination and hot-reload of an installer-rotated leaf certificate.
- Axum/Tower routing and an utoipa-derived, drift-tested OpenAPI contract.
- Hashed, scoped API-token verification and revocation.
- Rate limits, request/body limits, and per-token concurrency.
- The versioned control API and inference route allowlist.
- SQLite desired state, idempotency records, operations, compatibility
  evaluations, and audit
  metadata.
- Unprivileged Rust `hf-hub` downloads into the application-owned Hugging Face
  cache, with the contained verified HTTP helper used only after classified Xet
  failures.
- Recipe visibility and selection, while treating the executor's root-owned
  recipe digest as authoritative.
- Reconciliation requests to the executor and streaming proxying to healthy,
  executor-observed container endpoints on the managed internal bridge.

It has no Docker socket, no `sudo`, no Linux capabilities, no writable recipe
directory, and no access to SSH or model-registry credentials outside its
systemd-provided credential handles.

#### Root executor

`sy spark executor` has no TCP socket and does not parse remote HTTP or model
metadata. Its Unix socket is in a root-owned mode-0750 directory; the socket is
mode 0660 for one non-login service group. Every connection must have the exact
configured agent UID according to `SO_PEERCRED`; filesystem membership alone is
not accepted.

Its protocol is length-bounded, versioned, and closed over these typed actions:

```text
InspectHost
ListManagedContainers
EnsureManagedNetwork
EnsureRecipeImage(recipe_id)
StartInstance(instance_id, generation, model_commit, recipe_id)
PromoteRestartPolicy(instance_id, generation)
StopInstance(instance_id, generation, grace_seconds)
ReadManagedLogs(instance_id, generation, cursor)
InspectInstance(instance_id, generation)
```

This protocol extends the shared `sy-ipc` server with an injectable
`PeerAuthorizer`; the existing same-eUID behavior remains the default for every
other consumer. Spark installs one policy that accepts only the static numeric
`sy-spark` UID and records the kernel-provided peer PID and UID. Socket group
membership permits connection setup but never substitutes for this identity
check.

The executor derives image digest, argv, environment, mounts, devices, managed
network attachment, fixed engine port, labels, limits, and health probe from its
root-owned recipe and network policy.
The caller cannot submit any of those Docker fields. Instance IDs, generations,
model commit paths, cursors, and grace periods are independently validated. Paths
are resolved beneath fixed roots with descriptor-relative, no-symlink operations;
string-prefix path checks are insufficient.

All managed containers carry Docker labels, using the documented
[label mechanism](https://docs.docker.com/engine/manage-resources/labels/):

```text
io.sy.spark.managed=true
io.sy.spark.instance=<stable id>
io.sy.spark.generation=<monotonic integer>
io.sy.spark.role=engine
io.sy.spark.model_commit=<full commit>
io.sy.spark.recipe=<recipe id>
io.sy.spark.operation=<creating operation id>
```

The executor refuses to inspect, stop, remove, or read logs from a container
without a matching managed label set. It never acts on an unmanaged container,
even during memory emergencies.

#### Engine containers

Every engine container must satisfy these baseline controls unless a reviewed
recipe documents a narrower compatible variation:

- Image referenced by immutable digest and verified architecture.
- NVIDIA GPU access through the existing container toolkit; never `--privileged`.
- Attach only to the one `sy`-managed user-defined Docker `--internal` bridge;
  the engine binds its recipe-fixed port on the container interface and
  publishes no host port.
- The selected Hugging Face repository-cache root is mounted read-only and the
  engine receives its exact `snapshots/<commit>` path. Mounting the snapshot
  directory alone is invalid because its blob symlinks resolve through the
  repository cache. No unrelated repository, host home, SSH state, Docker
  socket, agent token, Hugging Face token, or NGC token is mounted.
- Cache files use a 0027 umask; a non-root engine receives only the numeric
  supplemental read group needed to traverse that read-only bind. That group
  grants nothing useful inside the container because no other host path is
  mounted.
- Read-only root filesystem, dropped capabilities, `no-new-privileges`, bounded
  PIDs, and explicit writable tmpfs/compile-cache mounts.
- Non-root container UID by default; a root-in-container exception must be exact,
  justified by the upstream recipe, and pass the same dropped-capability tests.
- Default Docker seccomp profile retained unless a reviewed Spark recipe records
  a necessary exception and its security test.
- Docker's internal bridge has no external default route, as documented by
  [`docker network create --internal`](https://docs.docker.com/reference/cli/docker/network/create/).
  Image pulls and model downloads are separate host operations with network
  access; engine startup never requires Internet access.
- Offline engine variables prevent an accidental model fetch during startup.
- Engine-native API keys are defense in depth, not the remote security boundary.

vLLM's own documentation states that its API key protects `/v1`, `/v2`, and
`/inference` but not every inference-capable route, notably `/invocations`; see
the [OpenAI-compatible server security note](https://docs.vllm.ai/en/latest/serving/online_serving/openai_compatible_server/).
The gateway therefore exposes an explicit method/path allowlist and never passes
arbitrary engine paths.

#### Shared internal bridge

The executor creates or validates one user-defined internal bridge dedicated to
all managed inference engines. The network is an explicitly labeled Docker
resource with a root-owned expected configuration; `sy` never changes Docker's
daemon-wide networking or host firewall configuration. No engine port is
published. Docker documents that the host can reach container ports on a bridge
while outside hosts cannot reach unpublished ports under the default routing
model; the agent therefore connects directly from the host to the
executor-observed container address and recipe-fixed port.

The executor returns an endpoint only after verifying the container ID,
instance, generation, engine role, network ID, attachment, and address. The
agent cannot submit or persist an arbitrary upstream URL, and reconciliation
invalidates a route when any of those observed fields changes. A recreated
container receives a new observed endpoint and must pass health and semantic
checks before the gateway republishes it.

The selected shared topology deliberately permits managed engine containers to
reach one another's ports; Docker documents that all ports are mutually
reachable on one user-defined bridge. Engines receive no agent, Hub, registry,
Docker, or host credentials, and the gateway remains the only LAN-facing route,
but this is not claimed to be lateral network isolation. `doctor` reports the
shared-network policy and attached managed/unmanaged containers; an unmanaged
attachment is drift that blocks new starts until resolved through an explicit
managed action.

#### Build and dependency boundary

The remote processes remain subcommands of the same `sy` package, not a
snowflake repository. A `spark-agent` Cargo feature gates server-only HTTP/TLS,
SQLite, executor IPC, and system-service code. The ordinary workstation build
retains the lightweight client. The Spark artifact is built as ARM64 with:

```text
cargo build --release --no-default-features --features spark-agent
```

The build/buy ledger in this SPEC is the roadmap's default, not an invitation to
repeat framework selection during implementation. New evidence may replace a
component only through an updated research decision with equal ARM64, license,
security, failure-semantics, and dependency analysis.

The control-plane boundary is:

```text
reuse: sy-ipc, notify/watchdog, tracing/journald, metrics UDS, procfs,
       rustix/nix, reqwest/rustls, sha2, minisign verification
adopt: axum/tower, axum-server, utoipa, governor, hmac/secrecy, rcgen,
       rusqlite + rusqlite_migration, Bollard, hf-hub
build: Spark wire/domain types, policy, state machines, reconciliation,
       fixed bridge upstream, resource admission/guard, installer adapter
contain: bundled SQLite FFI; hash-locked Python HTTP fallback; OCI CUDA engines
reject: generic reverse proxy, job queue, gRPC stack, custom Hub/Xet client,
        Docker CLI fallback, native SSH stack, embedded inference framework
```

The SQLite connection lives on one bounded database-actor thread. Bollard is
compiled pipe-only and wrapped behind `ContainerRuntime`; it never discovers a
remote socket. Rustls uses the already locked `ring` provider rather than adding
a second crypto provider. Shell construction is prohibited. Docker CLI output
may be shown in documented break-glass diagnostics, but never parsed by normal
execution or used as a reconciliation fallback.

All server-only crates are optional under `spark-agent`. The ARM64 release build
must pass `cargo deny`, embed `cargo auditable` metadata, and publish its resolved
feature/native-code inventory alongside the signed release manifest.

#### Filesystem layout

| Path | Owner/mode | Purpose |
|---|---|---|
| `/opt/sy-spark/releases/<version>/sy` | root, read-only | Versioned ARM64 executable |
| `/opt/sy-spark/current` | root-owned atomic symlink | Active application release |
| `/opt/sy-spark/hf-http-fallback/<lock-digest>/` | root, read-only | Hash-locked official Python HTTP-transfer fallback; no system Python mutation |
| `/etc/sy/spark-agent.toml` | root, 0640 for agent group | Non-secret service policy |
| `/etc/sy/spark-recipes.d/*.toml` | root, 0644 | Reviewed versioned recipes |
| `/var/lib/sy-spark/state.sqlite3*` | `sy-spark`, 0600 | Desired state and WAL files |
| `/var/lib/sy-spark/huggingface/` | `sy-spark`, 0750 | Native Hugging Face cache |
| `/var/lib/sy-spark/compile-cache/` | executor-created per instance | Fingerprinted engine compile artifacts |
| `/var/lib/sy-spark/tls/` | `sy-spark`, 0700 | Server key and certificate chain |
| `/var/lib/sy-spark/ca/` | root, 0700 | Local CA private key used only through SSH maintenance |
| `/var/lib/sy-spark/executor/emergency.jsonl` | root, 0600 | Fsynced guard actions/restart suppressions imported by agent |
| `/run/sy-spark/executor.sock` | root/service group, 0660 | Privileged local protocol |

Service logs go to journald. Secrets are delivered using systemd credentials or
root-created descriptors and are never stored in SQLite, recipes, environment
diagnostics, Docker labels, or container arguments.

### Configuration and Recipe Model

#### Agent policy

`/etc/sy/spark-agent.toml` is declarative and validated before either service
reloads. Initial policy for the inspected host is:

```toml
schema = "sy.spark.agent/v1"
listen = "10.1.30.143:9843"
allowed_client_cidrs = ["10.1.30.0/24"]
plain_http_loopback_only = true

[operations]
max_parallel_downloads = 1
max_parallel_starts = 1
max_parallel_tunes = 1

[resources]
system_reserve_gib = 8
emergency_available_floor_gib = 8
disk_reserve_gib = 100
startup_guard_interval_ms = 500
steady_guard_interval_ms = 2000
emergency_consecutive_samples = 3
memory_full_psi_avg10_percent = 2.0

[retention]
operation_days = 90
database_backups = 7
```

The installer must populate the exact address through which SSH reached the
host. It never substitutes `0.0.0.0` or `::` automatically. CIDRs are an
additional filter, not authentication. Configuration lowering a safety reserve
requires an explicit `--allow-lower-safety-reserve` acknowledgement and is
recorded in the audit log.

The user-selected 8 GiB admission reserve is an aggressive capacity-first value
for 119 GiB visible RAM. A verified recipe can require more reserve; it cannot
lower host policy. The 100 GiB disk reserve is approximately ten percent
of the inspected filesystem and prevents a model or image pull from consuming
the appliance's recovery space.

#### Recipe schema

A recipe is immutable data with these required groups:

| Group | Required fields |
|---|---|
| Identity | Schema version, recipe ID/version, status, maintainer, source URL, exact source commit |
| Terms/provenance | Model/image/engine licence identifiers and URLs, gated acceptance requirement, redistribution policy, artifact/signature provenance |
| Host match | Architecture, GPU model/SM, DGX software build, driver constraints, container-toolkit constraints |
| Model match | Repository, allowed full commits, artifact format/precision, required files and hashes, tokenizer/parser identity, remote-code policy |
| Engine | Engine/version, image repository plus digest, entry point, fixed arguments, permitted bounded substitutions |
| Isolation | Read-only mounts, writable cache/tmpfs paths, network policy, capabilities, seccomp variation, PID ceiling, required disabled engine features/routes |
| Resource envelope | Download size, image size, startup peak, steady peak, KV-cache policy, context ceiling, concurrency ceiling, compile-cache allowance |
| Health | Startup deadline, native health route, semantic one-token probe, expected served model identity |
| Gateway | Exact allowed method/path pairs and supported OpenAI capabilities |
| Evidence | Verification host fingerprint, objective, functional gate results, resource envelope, and timestamp |

Recipe arguments are token arrays, not shell strings. The only runtime
substitutions are executor-generated paths, the recipe-fixed loopback port,
instance identity,
and values selected from recipe-declared enums/ranges. The executor rejects an
unknown field, unknown substitution, duplicate ID, invalid digest, non-absolute
container mount, writable model mount, or host path outside fixed roots.

Recipe status is one of:

- `local-verified`: the exact fingerprint passed real-host correctness,
  durability, safety, isolation, capability, and quality gates.
- `upstream-verified`: vendored directly from an exact NVIDIA or engine recipe
  but not yet measured by `sy` on this host.
- `experimental`: structurally reviewed, exact, and runnable only when the
  caller names it with `--recipe` and acknowledges `--allow-unverified`.
- `disabled`: retained for audit/history and never launchable.

An experimental recipe still obeys isolation and admission controls. Successful
startup does not promote it; only the complete verification suite can change
recipe status. A Rust-native engine is not exempt. For mistral.rs and
candle-vLLM, the recipe must prove local-snapshot-only operation and disabled UI,
remote acquisition/media, file APIs, shell/Python/code execution, server-side
agent loops, MCP, and multi-model loading. If the exact engine version cannot
disable and test one of those capabilities, its recipe is `disabled`.

#### Deterministic selection

For a requested snapshot and objective, recipe selection is ordered as follows:

1. A caller-named exact recipe, if compatible and permitted.
2. A non-expired winning tune result for the exact fingerprint and objective.
3. A compatible verified exact vLLM recipe, preferring local verification over
   upstream verification and then recipe ID for deterministic ties.
4. Rejection with compatible non-vLLM and experimental candidates plus the
   exact missing vLLM fingerprint evidence.

There is no automatic change of engine, precision, model revision, context
length, or remote-code policy after a launch failure. The operation reports the
failed recipe and preserves its logs; a new explicit request chooses a different
recipe.

### Model Acquisition and Identity

#### Canonical identity

The persistent model key is:

```text
huggingface:<repository>@<full immutable commit>
```

`download <repo> --revision <branch-or-tag>` resolves the supplied reference
once through the Hub, records the returned commit, and downloads that immutable
revision. A display alias never replaces the commit key. `ls --json` reports
repository, commit, local snapshot path identifier, logical and unique disk
bytes, completion/verification state, compatible recipes, aliases, and active
instance references.

Aliases accept the Ollama-shaped `<name>:<tag>` form. The acceptance recipe
creates `ornith-1.5:9b` only as an alias for a pinned
`ornith-ai/Ornith-1.5-9B` commit; it never downloads or trusts an Ollama
manifest as the canonical artifact.

The default repository alias is created only when absent or already points to
that commit. Downloading a newer commit never moves it silently:
`--update-alias` is required. Existing instances remain pinned to their original
commit. An unqualified name that maps to several commits/aliases fails as
ambiguous and prints canonical choices.

`show` also reports model/engine/image licence provenance and whether the Hub or
registry requires an operator-accepted gate. `sy` verifies access but never
clicks through terms, redistributes gated bytes, or interprets a successful
download as permission for a different use.

The executor mounts the cache directory for only that encoded repository, not
the entire Hub cache, so `snapshots/<commit>` can resolve its relative blob
symlinks without exposing other downloaded repositories. It descriptor-resolves
the commit directory and every recipe-required auxiliary repository before
Docker sees a bind source.

The agent never imports executable Python from a model repository. A recipe with
`trust_remote_code=true` must name the exact commit, code files and hashes, and
run only inside the isolated engine container. Default policy rejects it.

#### Download flow

1. Authenticate, authorize the `models:write` scope, and reserve the
   idempotency key in one SQLite transaction.
2. Resolve repository access and the full commit with a fine-grained read token.
   Hugging Face recommends per-application fine-grained tokens in its
   [token guidance](https://huggingface.co/docs/hub/security-tokens).
3. Use the Rust Hub metadata APIs to obtain the immutable tree, required files,
   expected sizes, Xet metadata where applicable, and dry-run byte plan. Reject
   when the result plus image/temporary allowance would cross the disk reserve.
4. Persist the operation and resolved commit before transferring bytes.
5. Download into the native cache with Rust `hf-hub`. Existing blobs are reused;
   incomplete blobs remain resumable and are not visible as a complete model.
   A progress heartbeat records bytes and file, while a bounded watchdog detects
   a stalled transfer.
6. On a classified Xet transport, integrity, or no-progress failure, stop the
   Rust attempt and invoke the release-pinned official Python client once with
   fixed argv and `HF_HUB_DISABLE_XET=1`. It receives only the common cache path,
   canonical repo/commit, allowlisted files, and a read-only credential path.
   Its exit code is evidence, never proof. Auth, 403/404, policy, and disk
   failures bypass fallback and become the durable terminal cause.
7. Independently verify cache metadata/files against the immutable repository
   tree, reject every `.incomplete` entry for the snapshot's required blobs,
   descriptor-resolve symlinks, and verify any stronger recipe hashes.
8. Atomically mark the snapshot complete and create/update its explicit alias.

Cancellation stops transfer at a safe file boundary and leaves resumable
incomplete cache entries. It never promotes a partial snapshot. `rm --dry-run`
uses Hugging Face cache references plus `sy` model/instance references to report
exactly which unique blobs can be reclaimed. `rm` refuses active snapshots and
requires `--yes` when it would remove the last local reference to any blob.

Downloads run with lower CPU and I/O weights while inference is active. The
resident service never enables Xet high-performance mode, and the Xet chunk cache
remains disabled; either change requires a resource-bounded helper and
measurement on this NVMe. `status`, operation events, and audit record
`rust-xet` versus `python-http-fallback`, the fallback classification, attempts,
and verification result without secrets.

#### Registry credentials

Hugging Face and NGC credentials are separate, fine-grained, read-only systemd
credentials. A model download can access the Hugging Face credential; an image
pull can access only the registry credential required by its recipe. Engine
containers receive neither. The Rust client reads the credential into a
zeroizing wrapper and configures `hf-hub` explicitly. The Python fallback sees
only `HF_TOKEN_PATH` pointing at that credential file, never a token-valued argv
or environment variable. The executor constructs an in-memory Docker
registry-auth header from its credential descriptor and persists no Docker
client config. Authentication errors identify the registry and required scope
without echoing a credential fragment.

### Durable Orchestration

#### Persistent state

SQLite contains logical records, not model bytes or engine logs:

| Record | Durable content |
|---|---|
| `models` | Canonical repository/commit, verification, sizes, timestamps |
| `aliases` | User-facing alias to canonical model key |
| `instances` | Stable ID/name, model key, recipe fingerprint, generation, desired state, observed summary, restart suppression |
| `operations` | Type, actor/token ID, target, progress, state, timestamps, structured terminal result/problem |
| `idempotency` | Token ID, operation kind, key, canonical request hash, operation ID, expiry |
| `benchmarks` | Full fingerprint, objective, functional gate evidence, compatibility, selected flag, and invalidation reason |
| `token_metadata` | Token ID, HMAC-SHA-256 verifier, scopes, creation/use/revocation metadata; never plaintext or pepper |
| `audit` | Security and policy changes, recipe selection, emergency actions, installer transitions |

One dedicated blocking database actor owns the sole rusqlite connection and
serves a bounded Tokio request channel; asynchronous handlers never share a
connection, open a pool, or launch arbitrary per-query blocking tasks. It sets
WAL, `synchronous=FULL`, foreign keys, and a bounded busy timeout before serving.

Embedded `rusqlite_migration` migrations are atomic, forward-only, and compatible
with the immediately preceding application release. Released migration entries
are immutable and covered by validation plus a checked snapshot because
`user_version` alone does not checksum old SQL. Before each migration and once
per changed day, the agent creates an online SQLite backup, verifies it, fsyncs
it and its directory, and retains the newest seven valid backups. Restore is an
operator command, never an automatic guess after corruption.

#### State machines

```text
download: queued → resolving → preflight → transferring → verifying → complete
                                                      ↘ failed | cancelled

operation: accepted → running → succeeded | failed | cancelled

instance desired: stopped | running
instance observed: absent | creating | warming | healthy | degraded | stopping | failed
```

Every transition and its operation progress are committed before the side
effect that relies on them. Terminal states are immutable. A retry creates a new
operation and, for serving, a monotonically greater instance generation.

#### Serve transaction

1. Validate authorization, request shape, and idempotency. Resolve the model to
   one complete commit and select one exact recipe.
2. Inspect the live host and all desired-running managed instances. Admission
   uses aggregate cold-start peaks, not just steady-state values, so simultaneous
   Docker restarts after a reboot remain inside the safety envelope.
3. Commit the operation, instance generation, and transitional `creating` state.
4. Ask the executor to ensure the managed internal network and exact
   digest-pinned engine image, create compile-cache space, and start the engine
   with restart disabled. Image pulling is an observable sub-step; model
   downloading is never one.
5. The executor labels the engine before start, verifies its attachment and
   observed container endpoint, and activates the high-frequency memory guard.
6. Wait for the engine health probe over the internal bridge, then run a bounded
   semantic probes through the enabled OpenAI and Anthropic routes that clients
   will use. Confirm
   the served model identity.
7. In one logical completion sequence, enable `unless-stopped` for the engine,
   mark desired state running/observed healthy, and publish the gateway route.
   Reconciliation makes either ordering safe if the process dies between these
   steps.
8. Return instance, commit, recipe/image fingerprint, objective, endpoint,
   resource envelope, startup measurements, and operation ID.

If startup, network, memory, health, or semantic checks fail, the executor disables
restart, captures bounded diagnostic logs, removes the engine for that
generation, and the agent marks it failed with desired state stopped. A new
explicit `serve` creates a new generation; there is no unattended startup crash
loop.

#### Stop transaction

1. Resolve an unambiguous instance and commit desired state stopped before the
   external side effect.
2. The executor immediately changes restart policy to `no` for that exact
   generation.
3. The gateway stops admitting requests for the instance and allows existing
   streams to drain until the bounded grace period.
4. The executor stops/removes the matching engine generation.
5. Observed state becomes absent; model and compile cache remain.

If the agent dies after step one, startup reconciliation completes the stop. If
it dies before step one commits, the previous running intent remains. An
identical idempotent request returns the original operation.

#### Restart, reboot, and reconciliation

Healthy desired-running engine containers use `unless-stopped`; their aggregate
cold-start envelopes must fit admission so Docker may safely restart them
together. The root executor is enabled independently of the network agent and
starts its memory guard as early in boot as the existing Docker service permits.
The agent reconnects to the executor and gateway routes only after each engine
passes health again.

Reconciliation runs at agent startup, on Docker events, after every mutating
operation, and periodically as a safety net:

1. Read desired instances and in-flight operations from SQLite.
2. Ask the executor for containers with the complete `io.sy.spark.*` label set.
3. Match stable instance ID, generation, engine role, and attachment to the
   exact managed network; never adopt a name-only or incorrectly attached
   container.
4. Resume waiting for a matching creating/warming engine, publish a healthy
   desired-running engine, or remove one whose desired state is stopped.
5. Mark a missing/broken desired-running engine degraded and perform a bounded
   recipe-defined restart. Five failures within ten minutes suppress restart,
   disable/stop the engine, and leave desired
   running versus observed failed visible until an explicit new `serve` creates
   a generation.
6. Quarantine duplicate or future-generation managed containers by disabling
   restart and report them to `doctor`; do not delete ambiguous evidence.

Docker's event stream provides prompt observation and a full label scan
closes event gaps. Labels are untrusted until every field validates against
SQLite and the root-owned recipe registry.

#### Idempotency and cancellation

An idempotency key is scoped by authenticated token ID, operation kind, and
canonical request hash. Reuse with an identical request returns the same
operation; reuse with a different request returns `409 Conflict`. Keys persist
for at least the operation retention period.

Cancellation is itself idempotent. Downloads stop safely and remain resumable;
pre-health starts disable restart and remove their generation; tuning finishes
or aborts the current sample before cleanup. A healthy serving instance is not
cancelled through an old completed `serve` operation—`stop` is required.

### Unified-Memory Safety and Engine Selection

#### Admission gates

Admission is serialized and requires all gates to pass from one fresh host
snapshot:

1. **Reboot envelope:** the sum of recipe-declared cold-start peaks for all
   desired-running instances plus the candidate is no greater than visible
   `MemTotal - system_reserve`.
2. **Live envelope:** current `MemAvailable - candidate_incremental_start_peak`
   remains at or above `system_reserve`.
3. **Pressure:** memory full PSI is below policy, no meaningful swap-in is in
   progress, and the previous managed operation has released its transient
   allocation.
4. **Storage:** image, model, compile-cache, and temporary worst-case bytes leave
   at least `disk_reserve` free.
5. **Compatibility:** host fingerprint, image architecture/digest, model commit,
   parser assets, and recipe all match exactly.
6. **Concurrency:** only one start, tune candidate, or other high-memory
   transition is active.

`cudaMemGetInfo`, engine KV-cache estimates, Docker stats, and cgroup counters are
diagnostic inputs, not independent proof of safety on unified memory. A cgroup
memory ceiling is still applied as defense in depth, but the design does not
assume the current kernel accounts every CUDA allocation to that controller.
The exposed `dmem` controller must be measured before it can become an admission
authority.

Swap is never added to capacity. Any sustained swap-in during startup rejects
readiness because the engine may appear healthy while the rest of the host is
thrashing.

#### Independent emergency guard

The executor samples `MemAvailable`, memory PSI, swap activity, and managed
container events every 500 ms during start/tune and every two seconds in steady
state. It enters emergency mode after three consecutive samples below the
configured available-memory floor or when full-memory PSI `avg10` reaches two
percent.

Emergency action is deterministic:

1. Disable restart for the newest managed engine still starting or
   tuning, terminate its engine, and record a root-side emergency event.
2. If pressure remains and no transitional container exists, disable and stop
   the most recently started managed engine generation whose memory delta is
   growing.
3. Continue only across `io.sy.spark.managed=true` containers. Never kill,
   pause, or reconfigure an unmanaged process or container.
4. Suppress automatic restart for the victim. On reconnection the agent imports
   the emergency record, marks the operation/instance failed, and exposes the
   measurements through `doctor`.

The executor fsyncs restart suppression before termination. If Docker's Unix API
does not respond within the emergency deadline, it validates the labeled
container's recorded init PID, start time, and cgroup-v2 path, uses `cgroup.kill`
on that exact managed engine cgroup, and keeps suppression active until Docker accepts
`restart=no`. PID reuse or any label/cgroup mismatch aborts that target rather
than risking an unmanaged process. The Docker-independent fallback is covered by
synthetic cgroup tests, not exercised against arbitrary host workloads.

Agent and executor units use a protective negative `OOMScoreAdjust`; downloader
workers are easier to kill and engine containers are deliberately more killable.
This influences the kernel's final fallback but never replaces the proactive
guard. A memory guard fault makes new admission unavailable; it does not permit
an unguarded start.

#### Functional selection objective

“Optimal” means the most capable exact candidate supported by locally verified
functional evidence for one declared objective and fingerprint. The default
objective is `agent`, reflecting the requested coding-agent workload.

| Objective | Required capability and ordering |
|---|---|
| `agent` | Text generation, both public adapters, reasoning separation, client-side tool calls, structured output, and the recipe context ceiling |
| `interactive` | Text generation through both public adapters with streaming, cancellation, and bounded buffers |
| `long-context` | The largest recipe-declared context tier whose exact parser, safety envelope, and semantic fixtures are locally verified |

`bench` evaluates one named installed recipe; `tune` evaluates the finite set of
installed locally verified recipes compatible with the exact snapshot and host.
Neither command downloads an engine, pulls an image, converts a model, or makes
an unsupported family launchable. Each result records:

- exact host, model, recipe, image, tokenizer, parser, and network identity;
- required and forbidden API capabilities, semantic and model-identity checks;
- admission, memory-floor, PSI, swap, thermal, and emergency-guard compatibility;
- isolation, health, restart durability, and compile-cache ownership;
- explicit unsupported or uninstalled engine families and remediation.

A failed functional gate makes the candidate ineligible rather than assigning
it a weaker score.

#### Bounded autotuning

`tune` never searches arbitrary engine arguments. It evaluates a finite matrix
declared by compatible recipes, one candidate at a time. Candidate axes may
include:

- exact engine/image recipe;
- verified artifact precision/quantization;
- context ceiling and maximum concurrent sequences;
- KV-cache dtype and size;
- batched-token limit and scheduler policy;
- chunked prefill and prefix caching;
- FlashInfer, Marlin, MTP/speculative decoding, fast safetensor loading, and
  CUDA-graph modes when the exact recipe declares compatibility;
- bounded CPU thread count, shared-memory tmpfs size, and compile-cache mode.

A winning candidate must pass every applicable API, identity, quality, resource,
isolation, health, and durability gate. Among passing candidates, selection
prefers the highest recipe-declared capability tier, then fewer specialized
launch toggles, then the lexicographically smaller recipe ID. This ordering is
finite and deterministic; verified vLLM is the visible fallback, not a winner
declared by policy.

A tune result stores every functional gate result and the complete fingerprint.
Changing the model commit, engine image digest, host software/driver, recipe,
tokenizer/parser, capability contract, or objective invalidates selection.
Invalid data remains visible for audit but cannot be selected by `serve`.

#### Compile and runtime caches

Engine compile artifacts are isolated by the full fingerprint and created in a
temporary directory. Only a successful health/semantic check atomically promotes
the directory. Containers cannot write to another fingerprint's cache. Cache
garbage collection is reference-aware, dry-run capable, preserves active
instances, and leaves the disk reserve intact.

Prefix/KV caches are process-local runtime state and disappear on stop. They are
never presented as durable model data. `ps` distinguishes loaded model weights,
runtime KV utilization, and persistent compile cache.

#### Host controls that are explicitly rejected

- No `drop_caches`, swap disabling, swappiness changes, huge-page changes,
  overclocking, power-limit changes, or forced clock controls.
- No change to the already observed `performance` CPU governor or THP `madvise`
  without separate host-specific evidence and operator authorization.
- No universal `gpu-memory-utilization`, context length, batch size, KV-cache
  dtype, attention backend, or speculative-decoding flag.
- No sharing mutable compile caches across image/model fingerprints.
- No explicit Xet high-performance mode/helper; ordinary downloads
  competing with active inference receive lower CPU and I/O weight.

These constraints preserve the DGX base and prevent tuning folklore from
becoming persistent appliance configuration.

### Network, Authentication, and API

#### Threat model

The design assumes a client or model may be malicious, the LAN may be observed
or spoofed, an engine HTTP implementation may expose undocumented routes, and a
container escape or executor defect has high impact. It does not claim to make
public-Internet exposure safe. SSH compromise, kernel/driver compromise, and
physical access remain host-level threats.

Primary trust-boundary controls are:

- TLS 1.3 for every non-loopback connection.
- Server identity bootstrapped and pinned through SSH.
- Scoped, revocable, high-entropy bearer tokens stored hashed on the agent.
- One configured listen address, CIDR filtering, no CORS, no cookie auth, and
  conservative request/concurrency limits.
- No Docker authority or root capability in the network-facing process.
- No caller-controlled URL, image, argv, environment, mount, device, or host
  path reaches the executor.
- Loopback-only engines behind exact inference method/path allowlists.
- Read-only immutable model data and no registry/control credentials in engines.

#### TLS identity lifecycle

Install creates an application-local CA under a root-only path and a leaf
certificate whose SANs cover the configured IP and declared hostnames. The agent
can read only its leaf key and certificate chain. The CA public-certificate
fingerprint is returned over the existing SSH channel and written to the local
host profile. The client validates the chain, SAN, validity, and pinned CA
fingerprint; there is no trust-on-first-use over HTTP.

`cert rotate` uses SSH privilege to sign an overlapping leaf under the pinned CA;
the agent only hot-reloads the result. CA rotation also requires SSH and an
explicit client re-pin, with an overlap manifest for every configured client.
It cannot be completed through the HTTP API alone. Private keys are mode 0600
and never cross the host.

The `ca_cert_sha256` client field is therefore the CA pin obtained over SSH.

Plain HTTP is permitted only on loopback for tests and local diagnostics. HTTPS
is still HTTP interaction and satisfies the requested agent/client transport
without accepting bearer-token exposure on the LAN.

#### Token model

Tokens contain a public lookup ID plus at least 256 uniformly random secret bits.
The agent stores `HMAC-SHA-256(pepper, token_id || secret)` and verifies it with
RustCrypto's constant-time `verify_slice`; the random secret does not need a
password KDF. The pepper is a systemd credential outside SQLite. This keeps
per-request authentication cheap and avoids turning a memory-hard password hash
into an inference-request denial-of-service multiplier. Scopes are:

```text
models:read       models:write
instances:read    instances:write
inference         logs:read
operations:read   operations:cancel
benchmarks:read   benchmarks:write
admin
```

The bootstrap admin token is transferred through SSH directly into a local
mode-0600 credential file, not printed in logs or passed as a command argument.
`token create` can narrow scopes, CIDRs, expiry, and maximum concurrent inference
requests. Revocation is effective for requests begun after its successful admin
response and terminates no existing stream unless the administrator requests
stream revocation explicitly.

Loss or deliberate rotation of the pepper invalidates all bearer tokens and uses
the SSH recovery path to issue replacements; it never weakens verification.
Authentication failures are generic. Audit records contain token ID, scope,
source address, operation, and result, never the bearer secret or inference
prompt. Rate limiting happens after cheap token-ID parsing but before expensive
request parsing or proxy work; a coarse source limit also covers unknown token
IDs, followed by the narrower per-token policy after constant-time verification.
Active token metadata/verifiers are loaded from SQLite into an existing
`ArcSwap`-backed immutable snapshot, so inference authentication never queries
SQLite. Token creation/revocation commits first, swaps the new snapshot, and only
then returns success; the guarantee is “effective for the first request begun
after the successful admin response,” while already authenticated streams follow
the explicit stream-revocation policy.

Governor keys verified traffic by `(token_id, scope)`, not source IP, and the
adapter emits the same problem schema plus `Retry-After`. Revocation removes its
key; periodic retention and shrinking plus a hard active-token/cardinality cap
prevent the limiter itself becoming an unbounded memory sink. Tokio semaphores
enforce independent operation and inference concurrency limits.

#### Control API

The versioned base is `/api/sy.spark/v1`. Opaque API resource IDs avoid embedding
repository slashes or commits in paths.

| Method/path | Purpose | Scope |
|---|---|---|
| `GET /models` | Complete downloaded snapshots | `models:read` |
| `GET /models/{id}` | Snapshot details/compatibility | `models:read` |
| `POST /downloads` | Resolve and download one immutable revision | `models:write` |
| `DELETE /models/{id}` | Reference-aware removal plan/execution | `models:write` |
| `GET /instances` | Desired/observed serving inventory | `instances:read` |
| `POST /instances` | Start one instance generation | `instances:write` |
| `DELETE /instances/{id}` | Drain and stop one instance | `instances:write` |
| `GET /instances/{id}/logs` | Bounded/cursored managed logs | `logs:read` |
| `GET /recipes` | Recipe compatibility and evidence | `models:read` |
| `POST /benchmarks` | Evaluate one exact installed candidate | `benchmarks:write` |
| `POST /tunings` | Evaluate a bounded compatible candidate set | `benchmarks:write` |
| `GET /operations` | Operation inventory | `operations:read` |
| `GET /operations/{id}` | Durable state/progress/result | `operations:read` |
| `GET /operations/{id}/events` | Resumable server-sent events | `operations:read` |
| `DELETE /operations/{id}` | Request cancellation | `operations:cancel` |
| `GET /status` | Agent/executor/host summary | `instances:read` |
| `GET /doctor` | Compatibility, pressure, disk, security checks | `admin` |
| `/tokens...` | Token creation/list/revocation | `admin` |

Mutations require `Idempotency-Key`. An accepted response contains no optimistic
claim of completion:

```http
HTTP/1.1 202 Accepted
Location: /api/sy.spark/v1/operations/01J...
Retry-After: 1
Content-Type: application/json

{"schema":"sy.spark.operation/v1","id":"01J...","state":"accepted"}
```

Operation events have monotonic sequence IDs and support `Last-Event-ID`; the
client falls back to polling after a disconnect. Progress is structured by
stage, current/total bytes or candidates, rate, and a human-safe message. The
server never promises a percentage when total work is unknown.

All errors use `application/problem+json` with stable `type`, `code`, `status`,
`detail`, `operation_id` when applicable, and structured remediation. Examples
include `spark.recipe.unsupported`, `spark.host.fingerprint-mismatch`,
`spark.memory.admission-rejected`, `spark.disk.reserve`,
`spark.instance.ambiguous`, and `spark.executor.unavailable`. Stack traces,
absolute host paths, engine tokens, and Docker socket errors are redacted from
remote problems.

#### Inference gateway

Each healthy instance has two stable protocol base URLs:

```text
https://<spark>:9843/openai/<instance>/v1
https://<spark>:9843/anthropic/<instance>
```

The OpenAI surface exposes only implemented capabilities from this route set:

```text
GET  /v1/models
POST /v1/chat/completions
POST /v1/completions
POST /v1/responses
POST /v1/embeddings
```

`POST /v1/responses` is normative rather than a name-only shim: Codex custom
providers currently support the Responses wire API, a configurable `base_url`,
and bearer-token environment keys according to the official
[Codex configuration reference](https://developers.openai.com/codex/config-reference).
The gateway implements text and image request items, SSE events,
function/custom-tool calls and outputs, embeddings, usage, incomplete/error
states, and stateless continuation used by the pinned Codex compatibility test.
Unsupported OpenAI-hosted tools fail with an OpenAI-shaped error rather than
being ignored.

The Anthropic base URL is intentionally above `/v1`, matching Claude Code's
documented `ANTHROPIC_BASE_URL` gateway configuration. It exposes:

```text
POST /v1/messages
POST /v1/messages/count_tokens
```

The Messages adapter implements system content, text/image user and assistant
content blocks, SSE message/content-block events, client-side
`tool_use`/`tool_result`, stop reasons, token usage, and Anthropic-shaped errors.
These shapes follow the
official [Messages API](https://platform.claude.com/docs/en/api/messages/create)
and [streaming event contract](https://platform.claude.com/docs/en/build-with-claude/streaming).
Provider-hosted Anthropic tools and beta features are rejected unless an exact
capability is deliberately implemented and compatibility-tested.

Both adapters translate through one bounded typed internal model—generation
events, image parts, and embedding vectors—and then to the exact engine
capability declared by the recipe. They do not convert one public JSON protocol
into the other. `Authorization: Bearer` and
`x-api-key` are accepted only as presentations of a scoped `sy` token and are
stripped before upstream dispatch. `client-config` prints the exact base URL,
model/instance name, CA path, and secret environment-variable names for Codex or
Claude Code; it never prints or persists the token itself.

Unsupported methods and routes are rejected by the gateway, not forwarded to
the engine. `/invocations`, health, metrics, tokenization, debug, profiling,
adapter loading, file access, and engine administration are never remote pass-
through routes. `GET /v1/models` is rewritten to the public instance/model
identity rather than leaking the internal container address or engine
configuration.

The gateway enforces the `inference` scope, instance-level concurrency and body
limits, recipe context/output ceilings, a bounded header allowlist, stream idle
timeouts, and disconnect propagation. Hop-by-hop headers and caller-supplied
forwarding headers are stripped. Request and response bodies stream with bounded
buffers so long generations do not scale agent memory with output size.

The gateway returns `503` while a desired-running container is rebooting or
warming and includes a bounded `Retry-After`; it never routes to a generation
until semantic readiness passes. An instance generation change drains existing
streams against the old generation before publishing the new route.

#### CLI/API JSON contracts

Every JSON document has a schema discriminator such as:

```text
sy.spark.model/v1
sy.spark.instance/v1
sy.spark.operation/v1
sy.spark.status/v1
sy.spark.problem/v1
```

Fields are additive within `/v1`; meaning and type do not change. Unknown fields
must be ignored by clients. Enumerations gain values only when clients already
have an `unknown` presentation path. Timestamps are UTC RFC 3339, byte sizes are
integer bytes, durations are integer milliseconds, and identifiers are opaque
strings. Human CLI tables are projections of these same documents.

`utoipa` derives OpenAPI 3.1 components and operations from these same serde
types and axum routes. CI normalizes the generated document and compares it with
a committed artifact so route, scope, response, and schema drift is deliberate.
The artifact ships with developer/release documentation, not through the agent;
there is no runtime documentation route or UI. Request structs reject unknown
fields; response clients remain forward-compatible as described above.

Retries are automatic only for safe reads and idempotency-keyed mutations.
Authentication, compatibility, admission, and semantic operation failures are
not retried blindly. Retry backoff honors `Retry-After`, is bounded, and uses
jitter while preserving the original key and canonical request bytes.

### Migration & Compatibility

#### SSH bootstrap and installation

SSH is used only for install, upgrade, rollback, certificate-pin recovery, and
break-glass diagnosis. Normal lifecycle and inference use HTTPS. Authentication
material is supplied through an SSH agent, interactive prompt, or protected file
descriptor; a password is never embedded in argv, a unit, shell history, the
repository, or remote state.

`install --dry-run` performs read-only discovery and emits a machine-readable
change manifest covering:

- DGX software build, OS, architecture, kernel, NVIDIA driver, GPU, Docker,
  container-toolkit, systemd, active LSM/AppArmor/SELinux mode, Python/venv
  support for the contained HTTP fallback, memory, swap, disk, filesystem,
  listen address/port, and existing installation;
- every directory, file, user/group, credential, certificate, and systemd unit
  to create or replace;
- executable, fallback-wheel lock, recipe, and unit hashes, ownership, modes,
  service transitions, database migration, and rollback target;
- protected DGX versions before the operation and the assertion that none will
  change.

The mutating form requires `--yes`, stages files under an application-specific
temporary directory, verifies hashes, fsyncs, and atomically installs a release.
It creates a static non-login `sy-spark` service identity, persistent paths, the
root-owned recipe registry, TLS identity, initial scoped credential, and systemd
units. It uses the existing Python only to create a release-pinned virtual
environment with hash-locked official Hugging Face client wheels for the
HTTP-only fallback. Rust `hf-hub` in the signed binary is the normal downloader;
the venv is not imported into the agent and never changes system Python.

The local installer drives `ssh`/`sftp` with discrete argv and fixed remote
entrypoint names. Files are uploaded with a signed manifest and verified before
activation. OpenSSH owns host-key checking and interactive key/password/
keyboard-interactive behavior; `sy` does not emulate it, store the supplied
password, use `sshpass`, or construct arbitrary remote shell text.

When an LSM is active, the corresponding repository-owned AppArmor or SELinux
policy is part of the same dry-run/install/rollback manifest. An unknown or
unenforceable active policy blocks install; operators are never told to paste a
manual profile or disable host security.

It must not run any of:

```text
apt upgrade / dist-upgrade
do-release-upgrade
driver, CUDA, firmware, kernel, Docker, or container-toolkit installation/update
system Python pip installation
automatic sysctl, firewall-enable, reboot, or bootloader changes
```

No engine image or model is pulled during installation. Firewall state is
reported by `doctor`; inactive UFW is not silently enabled because doing so may
alter unrelated host access. The exact bind address, CIDR policy, TLS, and token
controls are active before the agent accepts a remote request.

#### Systemd services

`sy-spark-executor.service` runs as root with only `AF_UNIX`, a strict protected
filesystem view, no home access, no network address family, no writable recipe
path, and explicit access to the Docker socket plus application run/cache paths.
It starts after Docker and before the agent. Its watchdog covers the memory guard
and Docker event loop independently.

`sy-spark-agent.service` runs as `sy-spark` with an empty capability/ambient set,
`NoNewPrivileges`, protected home/system paths, private devices/tmp, namespace
restrictions, bounded file descriptors/processes/memory, and write access only to
its state/cache/TLS paths. Its watchdog covers reconciliation and HTTP liveness.
It has `Wants`/`After` on the executor but remains available in a read-only
degraded mode if the executor is restarting; all mutations return a typed `503`.

Both use `Restart=on-failure`, bounded restart backoff, `sd_notify`, and journald.
Engine containers are not systemd units. Unit hardening is integration-tested
with `systemd-analyze security`, but that score does not replace explicit
boundary tests.

#### Atomic upgrade

Application releases live side by side. Upgrade proceeds as follows:

1. Inspect and render the full change/migration/compatibility plan.
2. Record protected DGX versions and create a verified online database backup.
3. Upload a new binary, hash-locked HTTP-fallback venv, recipes, and units into a
   new versioned release directory; validate them without changing `current`.
4. Reject the upgrade if its recipe/schema set cannot represent every active
   desired-running instance or the preceding release cannot read the expanded
   database schema.
5. Atomically switch `current`, daemon-reload, and restart executor and agent.
   Existing engine containers remain running; only the gateway has a bounded
   restart interruption.
6. Verify executor identity, guard, database, reconciliation, HTTPS pin, and a
   gateway semantic request against each existing instance.
7. Compare protected DGX versions byte-for-byte with the preflight snapshot and
   commit the release record only when they are unchanged.

If health fails, the installer restores the preceding symlink/units and database
backup where schema rollback requires it. It does not stop or remove a healthy
engine container as an automatic rollback tactic. A rollback that cannot safely
represent active state is rejected with the exact instance/recipe blocker.

#### Recovery contract

- Loss of the agent process: systemd restarts it; healthy containers continue;
  SQLite and label reconciliation restore routes.
- Loss of the executor process: existing containers continue; the agent refuses
  mutations, reports degraded safety, and systemd restarts the executor.
- Loss of Docker: desired state remains; operations fail durably; reconciliation
  resumes after Docker returns.
- Host reboot: Docker applies desired-running restart policy within the admitted
  aggregate cold-start envelope; the gateway republishes only after readiness.
- Incomplete download: Rust Hub transfer resumes by commit; classified Xet
  failure may switch once to verified HTTP fallback, and no helper exit status
  can promote an unverified snapshot.
- SQLite WAL crash: WAL recovery precedes Docker reconciliation.
- Corrupt SQLite: agent enters read-only recovery mode and identifies verified
  backups; it never fabricates desired state from container names.
- Missing/corrupt recipe: executor refuses mutation and preserves managed
  evidence; it does not run a container from SQLite-supplied arguments.
- Expired certificate or token: SSH recovery rotates identity without touching
  models or containers.

### Observability and Operator Experience

`status` is the compact health view: agent/executor versions, host fingerprint,
TLS expiry, database/WAL/backup health, Docker connectivity, memory/disk/PSI/swap,
running and warming counts, operation counts, recipe-catalog digest, and degraded
reasons. `doctor` adds actionable compatibility/security checks and never mutates.

`ps` reports both intent and reality. Required fields include instance ID/name,
model repository/commit, desired/observed state, generation, engine, recipe and
engine image digest, objective, gateway endpoint, readiness, uptime,
restart count,
declared/observed memory, context/concurrency ceilings, active requests, and last
problem code.

Structured journald records carry timestamp, severity, component, event code,
request/operation/instance/generation IDs, actor token ID, and safe fields.
Prompts, generated text, bearer/registry credentials, Authorization headers,
Docker auth, absolute credential paths, and raw model configuration are excluded
by default. Engine-log retrieval is byte/time bounded, cursored, redacted for
known credential patterns, and requires `logs:read`.

An authenticated Prometheus-format agent endpoint exposes operation and failure
counts, download bytes, inference request/token metrics, engine
health/restarts, memory/disk/PSI/swap, emergency actions, and reconciliation
drift. Cardinality is bounded: full commits, operation IDs, prompts, and client
IDs are not metric labels.

### Non-Functional Requirements

- **Functional selection:** exact capability, identity, correctness, isolation,
  health, durability, no-swap, thermal, and aggregate cold-start safety gates
  determine candidate eligibility.
- **Reliability:** SQLite WAL/FULL committed intent, idempotent mutation keys,
  resumable downloads/events, exact generation reconciliation, bounded restart
  loops, verified backups, and explicit degraded/recovery states.
- **Security:** TLS 1.3 and SSH-pinned identity, scoped hashed tokens, exact
  listen/CIDR policy, unprivileged network process, peer-credential executor IPC,
  root-owned recipes, internal-only unpublished engine ports, no arbitrary
  Docker fields, no secret injection, and declarative AppArmor/SELinux policy
  when the detected LSM
  requires one.
- **Observability:** structured tracing/journald events, operation/audit records,
  bounded redacted logs, status/doctor, and bounded-cardinality metrics covering
  requests, tokens, pressure, disk, restarts, drift, and emergency actions.
- **Resource safety:** bounded control-plane memory and buffers, 8 GiB system
  admission reserve, configured emergency floor,
  and 100 GiB filesystem reserve on the inspected host.

Detailed gates and recovery semantics are normative in the sections above and
in the acceptance suite below; this summary does not weaken them.

### CLI / MCP Surface

- The normative subcommands, JSON documents, and exit codes are in the Proposal's
  Product Contract; all are backed by `/api/sy.spark/v1` resources rather than
  shell execution.
- Every output command supports `--json`; every mutation supports `--dry-run`;
  destructive model removal, token revocation, certificate rotation, and
  install/upgrade/rollback actions require `--yes`; every flag has a
  `SY_SPARK_*` equivalent with flags > env > config precedence.
- When stdin is not a TTY, the client never prompts. Missing acknowledgement or
  credentials produce a typed error and stable exit code. `--log-format json`
  and `SY_LOG_FORMAT=json` make client stderr logs machine-readable without
  changing stdout schemas.
- Dry-run performs resolution, policy, compatibility, sizing, and admission
  checks and returns the exact planned side effects, but creates no operation,
  database mutation, download, image pull, container, token, certificate, file,
  unit transition, or remote configuration change.
- No MCP tool is added. HTTPS is the machine control surface and the authenticated
  OpenAI/Anthropic endpoints are the inference surfaces; wrapping those in a
  second protocol would duplicate rather than extend the laptop's existing
  stdio MCP planes.

### Dependencies

#### Reused workspace dependencies and mechanisms

| Component | Normative use |
|---|---|
| `tokio`, `tokio-util`, `tokio-stream`, `bytes` | Runtime, bounded channels, cancellation, streaming HTTP upstream, and subprocess supervision |
| `serde`, `serde_json`, `clap`, `ulid` | One set of strict wire/domain types, CLIG parsing, stable JSON, and opaque identifiers |
| `arc-swap` | Wait-free immutable active-token/route snapshots; administrative writes persist before publishing a replacement |
| `reqwest` 0.12 + rustls 0.23/`ring` | Workstation HTTPS client and one selected crypto provider; no OpenSSL or duplicate AWS-LC provider |
| `crates/sy-ipc` | Shared length-delimited Unix RPC, deadlines, cancellation, and streaming; add `PeerAuthorizer` while preserving the same-eUID default |
| `tracing`, `tracing-journald`, `metrics`, `metrics-exporter-prometheus` | Existing journald and Prometheus-over-UDS observability path; no second telemetry pipeline |
| `sy_core::notify` | `READY`, `STOPPING`, and watchdog systemd integration |
| `procfs`, `rustix`, `nix` | Proc/PSI observation, peer credentials, descriptor-relative safe paths, and strict cgroup-v2 file access |
| `sha2`, `minisign-verify` | Fingerprints/hashes and signed application/recipe manifest verification |

#### Approved new Rust dependencies

Exact compatible patch versions are frozen in `Cargo.lock`; the version family
below records the researched API boundary, not permission for an unreviewed
upgrade.

| Dependency | Feature/version boundary | Use and rejection criteria |
|---|---|---|
| [`axum`](https://github.com/tokio-rs/axum) 0.8 + `tower`/`tower-http` | Only routing, body limit, timeout, trace, and required HTTP utilities | Agent routing/middleware. Remove any unused middleware feature; no framework-specific state machine. |
| [`axum-server`](https://github.com/programatik29/axum-server) 0.8 | `tls-rustls-no-provider`; HTTP/1 and HTTP/2 only | TLS 1.3 accept, hot certificate reload, and graceful drain using the workspace `ring` provider. |
| [`utoipa`](https://github.com/juhaku/utoipa) + `utoipa-axum` | OpenAPI generation only; no UI packages/assets | Derive and fixture-test the API contract from serde/axum types. |
| [`governor`](https://github.com/boinkor-net/governor) | Core limiter, custom clock in tests; no framework wrapper | Per-token/scope and unknown-source rate state behind a bounded custom middleware. |
| RustCrypto [`hmac`](https://github.com/RustCrypto/MACs) + [`secrecy`](https://github.com/iqlusioninc/crates/tree/main/secrecy) | HMAC-SHA-256 and zeroizing secret wrappers only | Constant-time verifier for random tokens and protected transient token/pepper values. |
| [`rcgen`](https://github.com/rustls/rcgen) | Installer-only ECDSA P-256 CA/leaf generation | Explicit SAN certificates; never a runtime CA API or engine dependency. |
| [`rusqlite`](https://github.com/rusqlite/rusqlite) 0.40 | `default-features=false`, `bundled,backup` plus only proven required flags | One connection on one DB actor thread. Bundled SQLite is the audited native boundary and online backup mechanism. |
| [`rusqlite_migration`](https://github.com/cljoly/rusqlite_migration) | Embedded migrations; no directory loading at runtime | Atomic `user_version` migrations, validated and snapshot-tested. |
| [`bollard`](https://github.com/fussybeaver/bollard) | `default-features=false, features=["pipe"]` | Exact local Docker socket events/stats/images/containers/logs and API negotiation behind `ContainerRuntime`; no discovery, HTTP, SSH, TLS, BuildKit, WebSocket, or generic attach. |
| [`hf-hub`](https://github.com/huggingface/hf-hub) 1.x | Async snapshot/Xet client only; explicit endpoint/cache/token | Primary Hub mechanism. Its currently separate reqwest/sha2 major versions and binary-size cost are audited and revisited when the workspace upgrades; no hidden env configuration. |

`hyper`/`hyper-util` are already transitive to the selected HTTP stack and may be
used directly only in the exact fixed bridge-network upstream adapter. No generic reverse-
proxy behavior is exposed. The executor calls Docker `/version`, negotiates the
intersection with Bollard, and refuses any recipe below its declared minimum API.

#### Contained non-Rust and platform dependencies

| Component | Containment and exit condition |
|---|---|
| Bundled SQLite C core | One rusqlite/libsqlite3-sys FFI, no application `unsafe`, no host package mutation. Replace only if a Rust storage engine later matches SQLite's atomicity, recovery, backup, tooling, and long-term compatibility at lower risk. |
| Official Python `huggingface_hub` HTTP fallback | Hash-locked venv in `/opt`, fixed no-shell argv, Xet disabled, no imports into the agent, same Rust verification. Remove after the Rust client can explicitly select HTTP and passes the stated real-Spark three-repository recovery gate. |
| System OpenSSH `ssh`/`sftp` | Typed bootstrap-only subprocesses with native config/known-host/auth handling; fixed remote entrypoints. It is not present in normal lifecycle or inference calls. |
| Docker, systemd, journald | Reuse the inspected host services through their stable local interfaces; installation cannot update or reconfigure their base packages. |
| Inference OCI images/CUDA kernels | Untrusted out-of-process engines selected by immutable recipes, attached only to the managed internal bridge with no published ports, and digest-pinned. Rust engines receive no privilege exception. |

#### Explicitly rejected or deferred components

- No SQLx connection pool, Refinery, effectum, Apalis, Redis, PostgreSQL, or
  message broker. They do not own Docker observation/generation semantics.
- No tonic/gRPC, tarpc, generic reverse proxy, Caddy/nginx, `socat`, native SSH
  crate, Docker CLI parser, custom Hugging Face/Xet implementation, JWT/OAuth
  service, OpenTelemetry collector, or resident load-generator service.
- No embedded Candle/Burn/mistral.rs inference library in the agent or executor.
  `mistral.rs` and Candle-vLLM may enter only as isolated experimental recipes;
  Atoma-infer is rejected until its upstream production baseline changes and an
  exact Spark gate passes.

All server-only additions are optional under `spark-agent`. Release CI must run
`cargo fmt --check`, Clippy with warnings denied, tests, ARM64 build, and
`cargo deny check` with committed license/advisory/ban/source policy. The release
binary embeds `cargo auditable` metadata. CI fails for a new duplicate crypto
provider, unapproved git dependency, unresolved/yanked/security advisory without
a named time-bounded exception, or an undocumented build-script/native/unsafe
addition. The signed release manifest includes the resolved Cargo features,
licenses, dependency audit result, and ARM64 artifact hash.

### Testing Strategy

Tests follow the repository's micro-TDD and black-box E2E rules. No control-plane
behavior is accepted solely because a mocked unit test passes.

#### Unit and property tests

- Recipe schema rejection, exact host/model/image fingerprinting, deterministic
  selection, bounded substitutions, digest parsing, and unknown-field handling.
- Canonical repository/commit/alias/instance validation, descriptor-relative
  path containment, symlink and traversal resistance.
- Admission arithmetic at boundaries, overflow-safe byte math, aggregate reboot
  envelope, disk reserve, PSI/swap gates, and emergency victim ordering.
- Every desired/observed/operation transition, generation race, idempotency
  request hash, cancellation, restart suppression, and reconciliation decision.
- Token scope/CIDR/expiry/revocation, TLS pin matching/rotation, header stripping,
  inference route allowlists, request/output ceilings, and redaction.
- Governor token/scope keys, unknown-source limit, `Retry-After`, cardinality
  cleanup/cap, custom-clock refill, and independent concurrency semaphores.
- `sy-ipc` default same-eUID compatibility plus Spark `PeerAuthorizer` acceptance
  of exactly one numeric UID and rejection of root/other group members.
- Database-actor queue saturation/shutdown, migration validation/snapshot,
  backup-before-migrate, WAL/FULL/foreign-key setup, and N/N-1 schema reads.
- JSON/OpenAPI normalization and compatibility, unknown request rejection,
  forward-compatible response decoding, and stable CLI exit-code mapping.
- Hub error classification, no-progress deadlines, Xet-to-HTTP fallback rules,
  incomplete/blob/tree/hash verification, and helper-exit-zero distrust.

Property tests follow at least one example test and cover arbitrary path/name
bytes, state/event interleavings, and resource sums near integer limits.

#### Hermetic integration tests

A temporary test topology runs the real client, rustls HTTP agent, SQLite WAL,
Unix-socket executor protocol, fake Docker Engine endpoint, and fake streaming
canonical engine. It verifies:

- `download → ls → serve → ps → inference → stop` over the real HTTPS/JSON wire;
- pinned Codex completes a streamed Responses tool-call round trip through its
  generated custom-provider config, and pinned Claude Code completes the same
  task through its generated Anthropic base-URL config;
- equivalent OpenAI and Anthropic prompts preserve text, client-side tools,
  stop conditions, token usage, cancellation, and protocol-native errors across
  the internal event model;
- client disconnect/reconnect through SSE `Last-Event-ID` and polling fallback;
- duplicate concurrent idempotency requests create one operation/container;
- kill points before and after every SQLite/executor side effect reconcile to the
  intended state without an unlabeled or duplicate container;
- truncated WAL, missed Docker event, duplicate/future labels, stale container
  endpoint, wrong network attachment, failed readiness, crash loops, log truncation, and
  executor loss;
- simulated low `MemAvailable`, PSI, and swap-in trigger the exact guard action
  without touching an unmanaged fake container;
- malformed/oversized HTTP, SSE, TOML, recipe, executor frames, Docker events,
  logs, and engine responses fail closed without secret leakage;
- wrong UID on the Unix socket, label spoofing, path swapping, unauthorized
  route/scopes, plaintext LAN HTTP, wrong CA pin, replayed key with changed body,
  and SSRF-shaped model/image inputs are rejected.
- the fixed SSH/SFTP adapter never shell-interpolates aliases, paths, versions,
  or passwords, and its non-TTY behavior fails with a typed prompt-required
  result rather than attempting `sshpass` or persisting credentials;
- the DB actor remains bounded under concurrent reads/writes, shutdown drains or
  rejects deterministically, and no async handler opens a second connection.

The fake engine streams slowly and disconnects mid-token to prove buffers,
timeouts, cancellation, and drain behavior. A fake downloader exercises partial
files, corrupt-success helpers, Xet classifications, HTTP fallback, and cache
deduplication; a gated test uses Rust `hf-hub` against a small immutable fixture
repository. The fallback helper is replaced by a fixture executable in hermetic
tests, but its exact production argv/environment allowlist is snapshot-tested.

#### Container integration tests

Where Docker is available, tests use an isolated project label/temporary root
and a harmless ARM64-compatible engine fixture. They verify actual Docker label
filters, engine restart-policy transitions, one shared `--internal` bridge,
host-to-container access, accepted peer-engine reachability, absence of
published ports/external egress, read-only
mounts, capability/security options, event gaps, logs, and cleanup. Test teardown
resolves exact IDs from its unique labels and never operates on other containers.

#### Real Spark E2E

The real-model acceptance matrix covers every D12 capability:

- pinned `ornith-ai/Ornith-1.5-9B`, exposed as `ornith-1.5:9b`, covers text
  generation, reasoning separation, tool calling, and image understanding;
- `Qwen/Qwen3-Embedding-0.6B` covers text embeddings because its public
  [model card](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B) provides an
  Apache-2.0 safetensors fixture suitable for bounded correctness and durability
  tests.

Every exact model commit, image digest, tokenizer/processor hash, and upstream
recipe commit is frozen; a model name alone is never the fixture identity.

With explicit operator authorization for service faults, the real-host run must
prove:

1. Install dry-run and install report identical protected DGX release, kernel,
   driver, CUDA/runtime, Docker, and toolkit versions before and after. The login
   user and agent still cannot access the Docker socket.
2. A deliberately interrupted download resumes, a partial snapshot never
   appears in `ls`, verification completes, and unique disk-byte accounting is
   correct. At least one controlled Xet transport/integrity failure demonstrates
   the HTTP fallback; auth, missing revision, and disk errors demonstrate that it
   does not fall back.
3. Every compatible recipe passes health, a deterministic semantic request,
   served-model identity, stop/restart, forbidden-route, isolation, and
   resource-reserve checks before it is eligible.
4. Each installed, locally verified tune candidate runs the bounded functional
   matrix. Gate evidence is persisted; the selected `agent` profile passes every
   applicable gate and wins deterministic capability/simplicity/recipe-ID
   ordering.
   If a mistral.rs recipe supports the frozen model, it runs as an experimental
   competitor with UI/download/file/code/shell/MCP/multi-model features disabled;
   candidate status alone can never make it the selected default.
5. The selected profile serves through the gateway, and exact pinned Codex and
   Claude Code binaries each complete a deterministic streamed tool-call task
   through their generated configs. The bounded client matrix remains above the
   memory floor without swap-in or thermal throttle, and exposes correct
   `ps/status/metrics` data.
6. The VLM target answers a deterministic local image fixture through both
   public protocol adapters with no remote-media fetch, while the embedding
   target returns deterministic dimension/normalization/similarity results
   through `/v1/embeddings`.
7. Restarting agent and executor processes restores the same operation/instance
   state and gateway route without restarting a healthy engine container.
8. A controlled Docker restart and host reboot are optional maintenance gates.
   When separately authorized, they prove desired-running service returns only
   after health; otherwise each is recorded as not run without blocking
   acceptance.
9. `stop` drains, disables restart, and removes only the selected engine
   container; model bytes remain. An already-absent instance is an idempotent
   success.

The memory emergency path is validated with injected readings and a bounded test
container, never by intentionally exhausting real unified memory. Functional
reports always include the complete fingerprint and objective; upstream claims
are context, not acceptance thresholds.

#### Cross-cutting acceptance

- Agent and executor memory remain bounded by their declared systemd policy.
- No request, response, log stream, event stream, or Docker event queue has an
  unbounded in-memory buffer.
- A committed desired-state transition has zero logical RPO across process crash
  under SQLite WAL/FULL tests.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, repository
  tests, ARM64 build, unit hardening checks, feature-minimal build, committed
  `cargo deny` policy, auditable metadata inspection, and resolved-feature/
  native-code inventory pass.
- The primary Rust Hub path and Python fallback share one cache without
  corruption across a bounded interruption/restart fixture. Removing the
  fallback is a separately approved change.

## 5. User Journey Sketch

**Actor and context:** the Spark operator bootstraps and curates models from a
TTY; a local coding agent then discovers and uses the same named host and JSON/
OpenAI Responses and Anthropic Messages surfaces. **Trigger:** they need a model
served optimally on GB10 without logging into the appliance or maintaining
model-specific Docker commands.

| Phase | Actor action | What `sy` does | What the actor sees |
|---|---|---|---|
| 1. Bootstrap | Review and approve `sy spark dgx-spark install --dry-run` | Inspects protected versions, stages the same ARM64 binary/configs/units, creates split services and SSH-pinned TLS identity | Exact change manifest, unchanged DGX stack, healthy authenticated host |
| 2. Acquire | Run `download ornith-ai/Ornith-1.5-9B --alias ornith-1.5:9b` | Resolves a commit, sizes disk, transfers/resumes/verifies the native HF snapshot | Per-file progress, durable operation ID, complete model in `ls` only after verification |
| 3. Select | Run `recipes` and `tune --objective agent` | Explains compatibility, evaluates bounded exact candidates, and applies functional/safety/isolation/durability gates | Gate evidence and one deterministic winning fingerprint, or a precise rejection |
| 4. Serve/use | Run `serve`, then generate a Codex or Claude Code client config | Admits aggregate memory, starts the engine on the shared internal bridge, probes semantics, publishes authenticated OpenAI and Anthropic routes | Readiness progress, client-ready config, stable endpoints, and truthful `ps --json` resource/recipe state |
| 5. Recover | Continue through client disconnect or controlled service restart | Durable operations resume; labels/events/SQLite reconcile; unhealthy generations stay unpublished | Same IDs/intent after reconnect and an explicit degraded cause if recovery cannot complete |
| 6. Stop/retain | Run `stop` and inspect `ls` | Persists stopped intent, drains, disables restart, removes the exact group, retains snapshot/compile cache | Endpoint disappears, no unrelated container changes, model remains ready for explicit restart |

### Friction Map

| Friction | Phase | Opportunity |
|---|---|---|
| SSH password/key and root bootstrap feel risky | Bootstrap | Read-only dry-run, exact protected-version assertion, atomic release, and no credential argv/storage |
| LAN endpoint identity is unfamiliar | Bootstrap/use | SSH-delivered CA pin, scoped token file, exact bind address, and `doctor` certificate/security explanation |
| Large gated downloads can stall or fail | Acquire | Fine-grained credential error, dry-run size, per-file bytes/rate, no-progress watchdog, classified HTTP fallback, and Rust-owned final verification |
| “Best engine” is model/workload dependent | Select | Comparable functional gate evidence, declared objective, deterministic winner, and a visible verified vLLM fallback when untuned |
| Unified memory makes a plausible model dangerous | Select/serve | Aggregate cold-start report, fixed reserves, serialized start, pressure guard, actionable rejection |
| Healthy process is not yet a usable model | Serve | Native health plus semantic OpenAI probe and served-identity verification before route publication |
| Reboot/crash can leave Docker and database disagreeing | Recover | Desired state, role/generation labels, event/full-scan reconciliation, restart budget, visible quarantine |
| Stop might accidentally delete expensive bytes | Stop | Drain/remove the managed engine only; model deletion is a separate dry-run/`--yes` operation |

North star: one canonical model name becomes a verified, healthy, durable,
authenticated Codex/Claude-compatible endpoint without shell choreography or
mutation of the DGX base software stack.

## 6. Risks & Mitigation

| Risk | Impact | Likelihood | Mitigation and residual risk |
|---|---|---|---|
| Executor protocol defect reaches Docker root authority | Critical | Low–medium | Closed typed protocol, exact peer UID, root-owned recipes, no arbitrary Docker fields, fuzz/property tests; executor compromise still has host impact |
| Engine/container escape through driver/runtime | Critical | Low | Digest pins, no secrets/socket/home, least container privilege, reviewed images, no public engine port; kernel/driver escape remains outside app isolation |
| Unified-memory exhaustion wedges the host | Critical | Medium | Aggregate cold-start admission, conservative reserves, start serialization, PSI/swap checks, early root guard, restart suppression; novel driver accounting can still surprise estimates |
| Docker restarts several models concurrently after reboot | High | Medium | Admit persistent set against aggregate cold-start peaks, not steady peaks; reject denser persistent state |
| Recipe becomes stale after any host/image/model change | High | High | Exact fingerprint invalidation and refusal; requires new evidence rather than silently reusing compatibility data |
| Upstream tag/model revision or dependency is replaced | High | Medium | Full commit/image digest/wheel hashes/source commit, artifact verification, release manifest |
| Hub/Xet reports success with incomplete bytes on Spark | High | Medium | Rust-owned tree/file/incomplete verification, bounded no-progress classification, one visible HTTP fallback, real-Spark interruption tests; Hub availability remains external |
| Contained Python fallback accumulates supply-chain/runtime debt | Medium | Medium | Hash-locked isolated wheels, fixed no-shell invocation, no control authority, Rust primary path, and removal only through a separately approved change |
| Malicious model remote code | High | Medium | Reject by default; exact hashed reviewed code only in isolated container with no credentials/network |
| Rust inference server exposes convenient but dangerous agentic/file routes | High | Medium | Experimental internal-only OCI recipe, local snapshot only, gateway allowlist, UI/download/file/code/shell/MCP disabled and tested; unsupported disablement blocks the recipe |
| Compromised managed engine calls a peer engine API on the selected shared bridge | High | Medium | No credentials in engines, offline mode, gateway remains the only LAN route, peer reachability is reported explicitly, and engine-native keys are defense in depth where complete; D04a accepts residual lateral API reachability |
| Engine exposes an unauthenticated alternate route | High | Medium | No published ports and gateway allowlist protect LAN clients; peer engines on the accepted shared bridge remain a residual caller class |
| Token or registry secret leaks | High | Low | TLS, SSH pin bootstrap, fine-grained credentials, peppered HMAC token verifiers, redaction, no engine injection, scoped rotation |
| Wi-Fi loss interrupts clients | Medium | Medium | Durable operations, reconnectable SSE/polling, idempotent retries, engine containers continue |
| Partial downloads or compile caches fill disk | High | Medium | Dry-run sizing, 100 GiB reserve, resumable cache, atomic promotion, reference-aware explicit GC |
| SQLite corruption or migration error loses intent | High | Low | WAL/FULL, checks, online backups, N-1 schema compatibility, read-only recovery; operator chooses restore evidence |
| Long-running engine fatal error or crash loop | High | Medium | Semantic health, event reconciliation, restart budget, visible failed state, no hidden fallback |
| Selection accepts incomplete evidence | High | Low | Exact functional gate set, durable raw results, full invalidation, and explicit retuning |
| Gateway buffers an unbounded stream | High | Low | Bounded streaming path and buffer tests; direct engine exposure remains prohibited |
| Rust dependency growth adds duplicate crypto/HTTP stacks or native code | Medium | Medium | Feature-minimal crates, Cargo.lock, cargo-deny/auditable, duplicate/build-script/native/unsafe inventory, ARM64 CI, signed resolved-feature manifest |
| Bootstrap subprocess accidentally becomes a remote shell surface | Critical | Low | System OpenSSH with discrete argv, fixed remote entrypoints and SFTP manifest, no arbitrary command interpolation/sshpass, adversarial argv tests |
| Installer changes protected Spark software | Critical | Low | Explicit denylist, read-only dry-run, before/after protected-version assertion, application-owned release roots, rollback |

## 7. Open Questions

The key-decision register is closed, so `/journey` is unblocked. The following
are empirical verification inputs owned by the research and implementation
flows, not questions for the user; they retain fail-closed behavior:

- Which exact registry image digests, model commit, tokenizer/parser hashes, and
  NVIDIA playbook commits pass each recipe's full gate when frozen?
- Does kernel 6.17's exposed `dmem` controller reliably account all GB10 CUDA
  allocations? Until demonstrated, it remains diagnostic only.
- What cold-start/steady unified-memory envelope does each exact recipe measure?
  Upstream GPU-memory percentages are never copied as local reservations.
- Does pinned Bollard negotiate correctly with Docker 29.2.1 for every required
  event/image/container/log operation on ARM64? A failure blocks executor work;
  it does not authorize a shell or remote Docker fallback.
- What byte-progress window and attempt limit distinguish a slow Xet shard from
  a stalled transfer on this network/NVMe? The initial implementation uses a
  conservative bounded default and reports it without weakening final integrity
  verification.
- Which exact mistral.rs image/model combination, if any, passes this product's
  narrower route, quality, durability, isolation, and memory gates? Until then
  every such recipe remains experimental.

### Classification of questions from the earlier draft

This classification prevents implementation detail from leaking back into the
interactive review. Only items linked to a pending key-decision ID are presented
to the user.

| Question from the earlier draft | Research recommendation | Status |
|---|---|---|
| HTTP or HTTPS? | Native TLS 1.3 HTTPS on the configured LAN address, SSH-pinned local CA; plaintext only on loopback/tests | Accepted D10; TLS mechanics are engineering-owned |
| Give the agent Docker group or sudo? | Neither. Separate root executor with exact peer identity and a closed typed protocol | Accepted D01 |
| One best engine? | No. Exact recipes compete under a declared objective; selection is evidence-backed and deterministic | Accepted D02 |
| Custom model store? | No. Native immutable Hugging Face snapshot/blob cache plus SQLite metadata | Accepted D03 |
| How are engines networked? | One shared Docker-internal bridge, no published ports, direct host-agent upstream | Accepted D04/D04a |
| Should running models survive? | Remain desired-running until explicit `stop`; recover after control-plane, Docker, and host restarts | Accepted D05 |
| What stores durable intent and observation? | SQLite WAL/FULL desired intent + Docker labels/events/full scans + guarded `unless-stopped` reconciliation | Accepted D05a |
| How is GB10 start admission bounded? | Aggregate measured cold-start envelopes plus live `MemAvailable`/PSI/swap observation, serialized starts, and fail-closed telemetry | Accepted D06 |
| May critical pressure stop a managed engine? | Stop the newest transitional engine first, then the most recently started growing engine if pressure persists; suppress restart and record the cause | Accepted D06a |
| Who controls memory-pressure thresholds? | Explicit declarative host policy; tuning may recommend but cannot silently lower safety floors | Accepted D06b |
| What are the initial memory floors? | 8 GiB admission reserve and 8 GiB emergency `MemAvailable` floor | Accepted D06c/D06d |
| Which real-model fixtures? | Pinned `ornith-ai/Ornith-1.5-9B` (alias `ornith-1.5:9b`) and `Qwen/Qwen3-Embedding-0.6B` snapshots cover text/tools/vision and embeddings | User-selected primary fixture plus engineering validation matrix under D12 |
| Which interface should bind? | One explicitly configured Spark LAN address, never all interfaces automatically | Accepted D10 |
| Can `serve` download or auto-tune? | Download and tuning remain explicit; `serve` uses the verified vLLM fallback when no tuned winner exists | Accepted D07 |
| Build or adopt the control plane? | Adopt narrow Rust mechanisms and reuse workspace primitives; build only Spark policy, exact transports, state machines, reconciliation, and resource safety | Engineering recommendation D09 |
| Make a Rust inference server the universal engine? | No. mistral.rs is a high-priority experimental OCI recipe and Candle-vLLM a watchlist; both must win the same exact GB10 gates as established engines | Engineering recommendation under D02 |
| Which Hub downloader? | Rust `hf-hub` is primary; one verified Python HTTP-only fallback covers the observed DGX Xet failure and has an objective removal gate | Engineering recommendation |
| Add a durable job-queue framework? | No. Docker intent/observation/generation/cancellation semantics remain an explicit `sy` state machine over SQLite | Engineering recommendation |
| Add an SSH crate? | No. Typed system OpenSSH subprocesses preserve the existing alias, known-host, agent, hardware-token, keyboard-interactive, and password behavior for bootstrap only | Engineering recommendation |
| Add a new executor RPC framework? | No. Extend `sy-ipc` with an injected peer authorizer while retaining its existing default behavior | Engineering recommendation |

Credentials are supplied explicitly when a gated model or registry requires
them; the system never searches or copies existing secrets implicitly.

## 8. Hand-off

1. Run `/journey` against this specification and capture install, download,
   serve, inference, recovery, and stop from both human and coding-agent views.
2. Run `/roadmap` against that journey, keeping security boundary, durable state,
   fake-engine E2E, and real-Spark recipe verification as independently testable
   steps.
3. Run `/implement` one roadmap item at a time with micro-TDD.

This feature is a remote GPU serving plane, not an `aiplane::Workload`: it must
not acquire the laptop NPU or inherit Ryzen AI preparation/fallback behavior.
