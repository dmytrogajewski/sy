# SPEC: sy-aiplane admission control + priority scheduler

## 1. Summary

A scheduler, admission controller and warm-pool manager for `sy-aiplane`
— the Rust daemon owning `/dev/accel/accel0` on a single Ryzen AI
desktop. Three priority classes (`interactive | background | batch`),
per-class bounded queues with timeouts, cooperative cancellation via
ONNX Runtime `RunOptions::SetTerminate`, and an LRU warm-pool with
sticky "always-warm" interactive workloads.

## 2. Background & Research (with citations)

### 2.1 Inference-server admission control — primary sources

- **vLLM.** Continuous batching scheduler; priority field with lower-
  value-first, ties broken by arrival; preemption mode `RECOMPUTE` is
  default in V1; HoL blocking is a known issue on the Q2-2026 roadmap.
  - `https://docs.vllm.ai/en/stable/api/vllm/v1/core/sched/scheduler/`
  - `https://docs.vllm.ai/en/latest/api/vllm/config/scheduler/`
  - `https://github.com/vllm-project/vllm/issues/39749`
  - Cancellation: `engine.abort(request_id)` + `request.is_disconnected()`
    in the OpenAI server loop.
    `https://github.com/vllm-project/vllm/issues/4240`
    `https://github.com/vllm-project/vllm/issues/24584`

- **llama.cpp.** `--parallel` defines N slots; each slot owns its KV
  region (`total_ctx / n_parallel`); `--cont-batching` interleaves
  decode steps across slots; if slots fill, requests queue FIFO.
  - `https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md`
  - `https://github.com/ggml-org/llama.cpp/discussions/4130`

- **TGI.** `--max-concurrent-requests` (default 128) is hard
  backpressure; `MAX_WAITING_TOKENS` (default 20) caps how long queued
  tokens wait before forcing a prefill; router uses queues + scheduler
  + block allocators.
  - `https://huggingface.co/docs/text-generation-inference/en/architecture`
  - `https://huggingface.co/docs/text-generation-inference/basic_tutorials/launcher`

- **Triton.** Numeric priority levels (1 = highest); per-level
  `ModelQueuePolicy` with `max_queue_size`, `default_timeout_microseconds`,
  `timeout_action ∈ {REJECT, DELAY}`, `allow_timeout_override`. Each
  request carries optional `priority` and `timeout` fields. `ModelWarmup`
  blocks `Ready` until synthetic warmup completes.
  - `https://docs.nvidia.com/deeplearning/triton-inference-server/user-guide/docs/user_guide/model_configuration.html`
  - `https://docs.nvidia.com/deeplearning/triton-inference-server/user-guide/docs/protocol/extension_schedule_policy.html`

- **Ollama.** Default is serial per-model FIFO. `OLLAMA_NUM_PARALLEL`
  allocates N KV-cache slots per model; `OLLAMA_MAX_LOADED_MODELS`
  defaults to `3 × GPUs` (3 on CPU). Cache eviction is best-fit on
  memory pressure.
  - `https://docs.ollama.com/faq`

- **VitisAI EP.** Compile-then-cache: first session compiles for
  "a few minutes"; subsequent sessions load the cache instantly.
  AMD's docs claim up to 8 simultaneous inference sessions per NPU
  via temporal sharing managed by runtime, no user action required.
  - `https://ryzenai.docs.amd.com/en/latest/modelrun.html`
  - `https://onnxruntime.ai/docs/execution-providers/Vitis-AI-ExecutionProvider.html`
  - **Known bug:** multiple ORT sessions in one process crash silently
    after the first. `https://github.com/amd/RyzenAI-SW/issues/223`
    → strong argument for process-per-workload.

### 2.2 Accelerator admission on Linux

- **amdxdna kernel driver.** Hardware contexts (`struct amdxdna_ctx`)
  per process; Phoenix/Hawk = 6, Strix = 16. Scheduling firmware maps
  user-queue MQDs onto HW HQDs "based on priority and time quanta", but
  the upstream kernel doc does *not* document a userspace ioctl to set
  per-context priority — workload metadata declares column count, the
  Resource Solver does spatial+temporal placement.
  - `https://docs.kernel.org/accel/amdxdna/amdnpu.html`
  - `https://deepwiki.com/amd/xdna-driver/2-architecture`
  - DRM-misc 7.2: default scheduler policy moves FIFO → "fair".
    `https://www.phoronix.com/news/Linux-7.2-Initial-DRM-Misc-Next`
  - **Takeaway:** application-level priority is the right place to
    enforce QoS — kernel side is opaque and FCFS-fair-ish.

- **NVIDIA MPS.** `set_default_client_priority`: 0 normal, 1 below-
  normal; "a hint, not a guarantee."
  - `https://docs.nvidia.com/deploy/mps/index.html`
  - No documented AMD ROCm or XDNA analog.

- **Wayland focus.** No standard CPU-priority bump for focused clients
  in sway/niri docs. The desktop-priority analog is *application-level*.

### 2.3 Cancellation primitives

- ONNX Runtime exposes `RunOptions::SetTerminate(true)` (C++) /
  `terminate=True` (Python/JS/C#); callable from another thread to
  abort an in-flight `Session::Run()`; can be cleared and reused.
  - `https://onnxruntime.ai/docs/api/c/struct_ort_1_1_run_options.html`
  - `https://onnxruntime.ai/docs/api/python/api_summary.html`
- VitisAI EP issue tracker has no "cancel" or "interrupt" entry — the
  ORT-level `Terminate` is the only documented mechanism, and its
  effect on a VAIP-partitioned subgraph mid-flight is undocumented.
  Practical fallback: kill the worker process, restart cold from cache.

### 2.4 Priority classes — prior art

- **Linux ioprio:** RT / BE / IDLE with 8 levels in RT and BE.
  `https://docs.kernel.org/block/ioprio.html`
- **Kubernetes PriorityClass:** integer (≤ 1e9), preempts lower
  priority pods, graceful termination period default 30 s.
  `https://kubernetes.io/docs/concepts/scheduling-eviction/pod-priority-preemption/`
- **macOS DispatchQoS:** 4 bands — user-interactive, user-initiated,
  utility, background.
  `https://developer.apple.com/documentation/dispatch/dispatchqos`
- **Android oom_adj:** FOREGROUND_APP_ADJ=0; cached/idle ≈ 800-1000;
  bands drive both LMK and CPU throttling.
  `https://android.googlesource.com/platform/frameworks/base/+/master/services/core/java/com/android/server/am/OomAdjuster.md`

Three is the recurring count; macOS adds a 4th *user-initiated* band
between interactive and utility. For sy with VAD requiring <30 ms p99
wakeword latency, a 4th `realtime` tier is justified.

### 2.5 Cold-start cost on Ryzen AI

- **First compile:** "a few minutes"
  (`https://ryzenai.docs.amd.com/en/latest/modelrun.html`).
- **Cached load:** "instantaneous" per AMD docs; observed in
  RyzenAI-SW examples as sub-second to low-seconds depending on
  partition size.
- **Inference (warm):** sub-10 ms for typical embed/rerank shapes
  on Phoenix/Hawk (10 TOPS) and Strix (16 TOPS) per AMD's published
  TOPS-vs-7040 numbers.
  `https://www.amd.com/content/dam/amd/en/documents/partner-hub/ryzen/amd-ryzen-8040-series-quick-reference-guide.pdf`
- **Implication:** cold-start dominates by ~3 orders of magnitude.
  Always-warm policy for interactive workloads is non-optional.

## 3. Proposal

### 3.1 Data model — `aiplane::scheduler::Request`

```rust
pub struct Request {
    pub id: RequestId,             // ULID
    pub workload: WorkloadKind,
    pub input: WorkloadInput,
    pub class: PriorityClass,      // see below
    pub deadline: Option<Instant>, // absolute wall-clock
    pub queued_at: Instant,
    pub cancel: CancellationToken, // tokio_util
    pub respond: oneshot::Sender<Result<WorkloadOutput>>,
}

pub enum PriorityClass {
    Realtime,    // VAD frame, eye-track tick — strict-latency
    Interactive, // STT chunk, MCP search, focused-app rerank
    Background,  // knowledge daemon embed batches
    Batch,       // FullResync, large OCR sweeps
}
```

Rationale: 4 tiers, not 3. VAD's frame budget (≈30 ms at 16 kHz with
512-sample windows) cannot share latency goals with the rest of the
interactive class; carving it out matches macOS QoS and ioprio RT.

### 3.2 Queue topology

Per-class bounded MPMC queue (`crossbeam_channel`) with caps:

| Class | Cap | Default timeout | Timeout action |
|---|---|---|---|
| Realtime    |  4 |  50 ms | REJECT (`IpcError::Overloaded`) |
| Interactive | 32 | 500 ms | REJECT  |
| Background  | 256| 30 s   | DELAY (drop if still queued at shutdown) |
| Batch       | 4096| none  | DELAY |

Mirrors Triton's `ModelQueuePolicy` (REJECT/DELAY).
`https://docs.nvidia.com/deeplearning/triton-inference-server/user-guide/docs/user_guide/model_configuration.html`

Dispatcher: strict priority across classes (Realtime > Interactive >
Background > Batch). Within a class, FIFO. No multi-level feedback —
classes are caller-declared and the scheduler trusts the IPC tag
(MCP/CLI cannot escalate themselves; the CLI flag `--class` maps to
allowed values per caller).

### 3.3 Admission rules

1. Drop on overflow per class cap → return `Overloaded { retry_after }`.
2. Reject on deadline already in the past.
3. Reject if workload `WorkloadState != Ready` and class is Realtime
   (no cold-start blocking the audio loop).
4. Soft cap on `inflight` per workload (= AMD's 8-session limit minus
   safety margin). Even though we run process-per-workload, sessions
   *inside* one worker still share an HW context.

### 3.4 Preemption

NPU `Session::Run()` is not safely preemptable in the general case
(see VitisAI EP issue #223 — even *concurrent* sessions destabilise).
Strategy:

- **In-class preemption:** none. Within a class we serialise per
  worker; preemption is achieved by *not running* lower-class work
  while higher-class is queued.
- **Cross-class preemption:** the dispatcher takes the NPU mutex only
  when the higher-priority queue is empty. A long Batch job continues
  to run; a Realtime arrival waits for the next `Run()` boundary —
  same model as Triton dynamic batching boundaries.
- **Hard preempt:** call `RunOptions::SetTerminate(true)` on the
  Batch worker's run-options handle when an Interactive request has
  been queue-waiting > 200 ms; if the worker doesn't yield within
  500 ms, SIGKILL the worker child — supervisor restarts it from the
  warm VitisAI compile cache (sub-second).
  `https://onnxruntime.ai/docs/api/c/struct_ort_1_1_run_options.html`

### 3.5 Warm-pool policy

Process-per-workload (matches sy's `src/aiplane/worker/` direction and
sidesteps the multi-session crash in RyzenAI-SW#223). Each worker
owns one VitisAI session.

| Workload | Policy | Reason |
|---|---|---|
| VAD, EyeTrack | always-warm | realtime; cold-start kills the loop |
| STT, Embed (knowledge) | warm-on-activity, idle 15 min | hot path |
| Rerank, OCR, CLIP, TTS | lazy + LRU, max 3 warm | NPU has 6 HW ctxs on Phoenix |
| Denoise | warm only while a denoise consumer is registered | rarely needed |

Cap loaded workers at `n_hw_ctx - 2` (4 on Phoenix, 14 on Strix);
evict LRU on load. Sticky-warm slots count against the cap.
Driven by `WorkloadKind`-keyed `LruCache<RequestKey, Instant>` in
the supervisor, evicted via `Workload::unload()` (already in the
`Workload` trait — see `src/aiplane/registry.rs:222`).

### 3.6 Caller → class mapping

| Caller | Default class | Overridable to |
|---|---|---|
| Aiplane internal VAD loop | Realtime | — |
| `sy knowledge search` CLI | Interactive | `--class background` |
| MCP `knowledge_search` | Interactive | request `class` field |
| `sy knowledge daemon` index pass | Background | `--class batch` for resync |
| `sy knowledge sync` full resync | Batch | — |

### 3.7 Fallback to CPU

Trigger CPU fallback when:
- worker is in `Failed` state and class is Realtime (don't block VAD);
- per-class queue depth > 80% cap;
- aiplane reports `WorkloadState::Unavailable` for the kind.

Fallback runs the same ONNX model through ORT's CPU EP. For embed-sized
e5-base shapes on Ryzen AI 7040/8040, the CPU path is roughly 5-10×
slower than NPU at p50 (no first-party number; inferred from AMD's
own 10/16 TOPS NPU vs ~1-2 TOPS CPU INT8 equivalent). Acceptable for
Background/Batch, unacceptable for Realtime — if the NPU is down,
Realtime workloads return `Err(NpuUnavailable)` rather than degrading.

## 4. Anti-goals

- No multi-level feedback queue. Caller declares the class; no
  automatic demotion.
- No remote inference fallback. Single-host rice rule.
- No per-process renice / cgroup CPU bumping for the supervisor —
  Wayland focus does not bubble up to NPU scheduling in any compositor
  we surveyed.
- No attempt to expose XDNA HW priorities — the upstream kernel doc
  does not document a userspace ioctl for it.

## 5. Open questions

- Is the VitisAI compile cache actually sub-second on cold reload of
  a worker? Needs measurement.
- Does `RunOptions::SetTerminate` actually unblock a VAIP-partitioned
  graph mid-run? Filed as a measurement task.
- Strix Point's 16 HW contexts — does the AMD runtime time-share them
  fairly across processes, or do early bookings starve late arrivals?
