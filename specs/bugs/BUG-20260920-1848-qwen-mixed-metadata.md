# Qwen3.8 mixed-request metadata sort aborts on Spark

## Summary

Two concurrent long requests cause the optimized vLLM preview engine to abort
when a speculative decode shares a batch with a prefill.

## Reproduction

- Method: live inference against image `sha256:940a052f27ca7b7700127fbfc1786f0ba9fc2ac51da02b35a1916ba017e8d50e`.
- Recipe: `specs/runs/qwen38-vllm-optimization-20260920/qualify.py prefix`.
- Trigger: submit the two independent 63k-token requests simultaneously after
  the sequential 32k/128k/240k/261k cached-prefix tests.
- Evidence: `short_conv_attn.py:366`, `torch.argsort(token_group, stable=True)`
  raises `torch.AcceleratorError: CUDA error: operation not permitted`.
- The native process restarted inside the same container. `sy ps` still
  reported generation 2 healthy and zero restart failures; native logs are
  necessary evidence here. The candidate was stopped through managed `stop`.

## Expected

Concurrent requests queue or complete correctly without killing inference.

## Actual

Both Responses streams fail with `upstream stream failed`; the engine reloads.
Single-request protocol, decode and near-limit cache checks had passed.

## Root Cause

The mixed speculative/plain-decode/prefill branch constructs request groups
and stably sorts their expanded token indices on CUDA. All classifications
and query lengths are already available on CPU. Pure speculative batches
bypass this branch, explaining why single-request checks missed it. The
captured exception localizes the unsupported operation to the CUDA sort;
the lower-level CUDA implementation restriction is not yet independently
isolated. No memory-exhaustion error appears in the trace.

## Fix

Build the same stable permutation from the existing CPU metadata, then copy
only the final index tensor to the query device. Preserve group order
`speculative → plain decode → prefill` and token order within each request.
Fail the image build if the pinned source no longer matches the patch target.
Validate exact permutations on CPU, then repeat the failing live workload.

The corrected image `sha256:8cd67c8d2f84f34b10706df637b44d899ddb74fc495f1d1a99c0caabbbc365b6`
passed the concurrent workload: both 63,050-token requests and their cached
repeats returned the expected independent codes. Native logs showed two
running requests in one batch; Docker reported zero restarts afterward.
Streaming, nonstreaming, reasoning, function calls and tool continuation
also passed. Default tests passed twice, and Spark tests and both lint gates
passed. Full-context requalification is recorded in the run report.

## Traceability

- Failing live recipe and captured trace: the linked optimization run folder.
- Image patch: `configs/sy/spark/engines/patches/sha256-4ba17608fbb8908c6dfb2e796b8eb9aa4517cdcf6f261c40e8d9d8583b283e25.py`
  and `configs/sy/spark/engines/checks/qwen38_optimization.py`.
- Related journey: `specs/journeys/JOURNEY-20260920-vllm-optimization.md`.
