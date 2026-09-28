# Qwen sampling kernel first-load failure during normal client requests

## Summary

Normal sampled Responses requests can kill the optimized vLLM engine with
`RuntimeError: Triton Error [CUDA]: operation not permitted`, producing 503s.

## Reproduction

- Live incident: 2026-09-20 17:00:10 UTC, generation 3, image
  `sha256:8cd67c8d2f84f34b10706df637b44d899ddb74fc495f1d1a99c0caabbbc365b6`.
- Two running requests, eight scheduled verification tokens, MTP=3.
- `_topk_topp_kernel` first-compilation warning immediately precedes failure
  in Triton's `load_binary`; Docker restarts the engine (not OOM-killed).
- Captured original log: `specs/runs/qwen38-vllm-sampling-20260920/crash.log`.
- Fresh-process GPU sampling and concurrent sampled requests after restart
  pass. The fatal driver error is intermittent, not deterministically reproduced.

## Expected

Default sampling, tools, reasoning, and concurrency work without engine death;
startup covers the finite sampler variants used by the configured profile.

## Actual

Greedy qualification passed but disabled top-k/top-p sampling. The first real
client session reached an unwarmed sampler variant. Process status stayed
misleadingly healthy during the Docker-owned restart.

## Root Cause

The V2 warmup exercises eight ordinary requests (FlashInfer) and 32 speculative
verification rows (Triton). Two MTP=3 requests produce eight verification rows,
crossing the Triton dispatch threshold. The kernel's runtime batch dimension
still specializes on divisibility by 16, so warming 32 does not warm eight.
Other top-k/top-p presence combinations also select different specializations.
The initial investigation did not establish why CUDA rejected the module load.
The subsequent [container lifecycle regression](BUG-20260920-2200-spark-gpu-device-update.md)
directly reproduced GPU-access loss when sy promotes the restart policy.

Upstream corroboration, not proof of the low-level cause:
<https://github.com/vllm-project/vllm/issues/52877> reports the same GB10 failure
signature, including the same preview vLLM revision on Qwen in a follow-up.

## Fix

Preload the bounded sampler shape/filter matrix during engine startup. Keep
sampling semantics, model weights, MTP=3, prefix caching, and native context.
Validate no first-use sampler compilation after warmup and compare GPU masks
against the existing PyTorch reference; requalify sampled live traffic. The
existing approximate top-p-only path is compared by retained probability mass
(absolute tolerance 0.001), while top-k and combined filtering match exactly.

The isolated GPU test reproduces the warmup gap with vLLM's JIT monitor set to
error, then passes all 96 shape/filter combinations with the new warmup. A
separate BF16-default regression proved the helper must allocate FP32 logits
explicitly, as the real sampler does. This is not a deterministic reproduction
of the underlying intermittent CUDA driver error.

## Follow-up: sampler-only candidate failed

Generation 4 completed sampler warmup at 17:39:38 UTC and passed readiness.
At 17:41:48, the first mixed long/short sampling qualification killed it in
`torch.nn.functional.linear` for the shared-expert gate, with
`CUBLAS_STATUS_EXECUTION_FAILED`. The batch contained 8,000 prefill and four
speculative-decode tokens. There was no late sampler-compilation warning.
The complete trace is in the run's `second-crash.log`. Sampler warmup alone
is therefore not a sufficient production fix.

The same BF16 GEMM row sizes pass in an isolated GPU process. No GPU Xid or
OOM kill was observed. This does not prove the failing GEMM itself is wrong.
The next historical candidate set `CUDA_MODULE_LOADING=EAGER` and enabled CUDA error
logging. NVIDIA documents that lazy library loading can fail when prior
allocations leave insufficient memory for modules, and can introduce context
synchronization on first use:
<https://docs.nvidia.com/cuda/cuda-programming-guide/04-special-topics/lazy-loading.html>.
This is a hypothesis under live test, not an established explanation for the
cuBLAS failure. It retains CUDA graphs; it is not vLLM's `--enforce-eager`.

Generation 5 was explicitly stopped after over 16 minutes of CUDA-library
initialization before model loading began. It had no restart or fatal Python
traceback, and the CUDA disk cache reached about 1 GiB. This was an operational
rejection before the 30-minute deadline, not a proven crash or timeout. No live
inference result exists for this candidate; the lazy-loading hypothesis remains
unproven. It is not part of the current optimized profile.

An attempted pre-optimization recovery also failed (generation 6 below).
The graph-free experiment was cancelled before readiness. The current direction
retains every performance optimization and fixes the directly reproduced
container device-permission loss; no rollback profile is being qualified.

## Traceability

- Live qualification: `specs/runs/qwen38-vllm-optimization-20260920/qualify.py sampling`.
- Follow-up to `BUG-20260920-1848-qwen-mixed-metadata.md` and the optimization run.
- Implementation: image runtime helper, hash-guarded startup patch, and
  `configs/sy/spark/engines/checks/qwen38_sampling.py`.
- Generation 6: restored control started in 620.317 seconds and passed 14
  sampled concurrent requests plus five sampled protocol checks. At 18:28:21 UTC
  Docker recorded exit code zero, then restarted it. Reconciliation exhausted
  the restart-failure budget and removed it six seconds later. No new emergency
  journal record exists. The near-context-limit request failed before completion;
  native logs were lost with container removal, so its precise failure site is
  unknown. Restoring the control alone is insufficient.
- Generation 7: same control image with `--enforce-eager` and CUDA error logging;
  native logs are captured continuously. This disables compilation/CUDA graphs
  (distinct from generation 5's eager module loading), following vLLM's
  [diagnostic guidance](https://docs.vllm.ai/en/latest/usage/troubleshooting/).
  Cancelled before readiness at the user's direction; no inference acceptance
  result exists. It is not the active development or deployment direction.
- Follow-up: `BUG-20260920-2200-spark-gpu-device-update.md` records the directly
  reproduced restart-policy/GPU-permission fault and its optimized-image fix.
