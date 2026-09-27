# Faster Qwen3.8 on the existing vLLM service

## Actor & Goal

- Actor: the user launching coding agents through `sy spark`.
- Goal: apply and verify the researched PLE, draft-vocabulary, MTP and prefix-cache optimizations.
- Hardest constraint: preserve the exact checkpoint, 262,144-token context, correctness and resource reserves.

## Happy Path

1. Build the checksum-pinned [engine image](../../configs/sy/spark/engines/vllm-qwen38-mmap.Dockerfile), including its self-tests.
2. Validate the [profile](../../configs/sy/spark/engines/vllm-qwen38-mmap.toml) and archive control measurements through the existing benchmark harness.
3. Package/sign the release and inspect the supported upgrade dry run; the user-authorized change permits a controlled model restart.
4. Start the same model/instance with the updated profile; wait for native and protocol health checks.
5. Verify decode, prefix reuse, long context, reasoning and tool continuation; retain only a passing candidate and record rollback identity.

## Edge Cases

- Memory or disk pressure: admission rejects the start; never lower the safety floor or terminate unrelated work.
- Failed build or missing patch: stop before activation; retain the existing serving image.
- Cache-hit corruption, CUDA errors or protocol regression: reject the candidate and restore the recorded control profile/image through signed release management.
- Concurrent user inference: do not interrupt an active request; check activity before the planned restart.
- A speed improvement confined to one prompt: report category results and repeat measurements rather than advertising a universal gain.

## Acceptance Criteria

- [x] Image and profile contract tests pass; imported assets are immutable and self-tested.
- [x] Default and Spark lint/test gates pass.
- [x] Managed candidate serves the unchanged model at 262,144 context.
- [x] Performance, cache correctness, context and protocol checks are archived.
- [x] Deployment and rollback identities and any unqualified behavior are documented.

## Out of Scope

Changing engines, model weights, host OS settings or Sparky is excluded because the user requested optimization of this vLLM service.

## Open Questions

The retained profile uses MTP=3 and repaired prefix caching. See the
[rollout evidence and limits](../runs/qwen38-vllm-optimization-20260920/README.md).
The first candidate exposed a mixed-request CUDA metadata-sort failure;
the corrected image passed the same concurrent workload and the full
bounded context/protocol suite. Broad multilingual quality, sampling
distributions, 4/8-way concurrency and a long-running soak remain outside
this deployment qualification, as does a matched cold-prefill regression gate.
