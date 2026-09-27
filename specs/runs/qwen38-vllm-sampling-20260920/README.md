# Qwen sampled-inference incident and optimized-build qualification

## Incident

At 17:00:10 UTC the generation-3 engine died loading `_topk_topp_kernel` during
two concurrent requests. Codex showed HTTP 503. Docker restarted the same
container; sy incorrectly retained its previous healthy route. No OOM kill
was reported. The previous rollout's greedy-only qualification did not cover
normal production sampling. Its earlier green records are historical, not a
claim that this incident did not occur.

## Changes

The experimental image preloads every configured sampler row count and top-k/top-p presence
combination before serving. The isolated GPU regression first demonstrates
that the old 32-row warmup misses the eight-row variant, then verifies 96 cases
with late compilation treated as an error. Default top-k filtering matches
the existing PyTorch reference exactly; the existing approximate top-p-only
algorithm is checked by retained probability mass (absolute tolerance 0.001).
The test also covers a BF16 global default while explicitly warming FP32 logits.

Routes now retain the executor-observed PID and start ticks. Reconciliation
invalidates readiness when Docker replaces that process without changing the
container generation. PID reuse cannot inherit readiness. This uses existing
executor data and does not change the public wire schema.

The current direction retains the optimized image, MTP=3, prefix caching,
reduced draft vocabulary, pooled PLE, CUDA graphs, and 262,144-token context.
The control/graph-free experiments are not being qualified as a solution.
Generation 7 was cancelled before readiness at the user's direction.

A deterministic lifecycle regression now shows that updating Docker's restart
policy removes the GPU access installed by NVIDIA's legacy hook. Explicit
CUDA device mappings preserve access across that exact update without host
driver, Docker, or security reconfiguration. The fix applies to managed and
qualification containers. See the
[device-permission analysis](../../bugs/BUG-20260920-2200-spark-gpu-device-update.md).

## Deployed identities

The signed optimized release is deployed. Generation 8 reached healthy status
at 19:22:45 UTC after 648.327 seconds. GPU access survives readiness promotion;
expanded cache/cancellation acceptance passes. It remains the serving instance.

- Image: `sha256:fa6008389ff17911099e649aeb84b225e71e529c448aa46814ef397dc557f5ff`.
- Image size: 9,727,131,221 bytes.
- Profile: MTP=3, prefix caching, piecewise graphs, BF16 KV, context 262,144.
- ARM64 binary: `sha256:c01dac71cde5a519af3488d1aac55cedac339029a363edce81d16fe70b556f15`.
- Signed release: `0.1.0-5d710a6c595fb1a3b1cd80de4e8d193e55cd8ed2035e6ecf631863c0e4852be4`.
- Engine fingerprint: `sha256:1b878194852da5048ecd7b104b89b0cc42758ac8fd35c97206378b1db4ec0df4`.
- Instance: `qwen38-vllm`, generation 8, serve operation `01M303PWWXR02FK2H7XN15NV7B`.

## Verification

- Experimental image's isolated GPU regression: 96 cases pass, no late sampler compilation.
- PID/start-tick readiness regression passes.
- Optimized-profile tests verify that graphs and all intended optimizations
  remain enabled without global eager CUDA module loading.
- A 92-GiB CUDA allocation plus graph replay, first-use sampler, stable sort,
  and BF16 GEMM probe passes (`memory-probe-92g.log`).
- Device-mapping regression fails before the fix and passes afterward.
- The live lifecycle probe passes (`device-update-probe.log`): hook-only access
  fails after promotion; explicit-device access succeeds. A previously initialized
  CUDA context continues in both cases, while new CUDA contexts fail only in the
  hook-only container. Both exact diagnostic containers were removed.
- `make lint`, `make lint-spark`, `make fmt-check`, `make test` twice,
  `make test-spark`, and `make docs-site` pass. Available `make docs-lint` checks
  pass (958 links, zero errors); markdownlint/cspell/Vale are not installed.
- All 14 mixed sampled requests pass at concurrency 2/4/8, followed by five
  sampled protocol checks (stream/non-stream text, tool call, tool continuation,
  and reasoning).
- A 261,038-input-token request returns the correct access code, completing in
  145.946 seconds with first output at 139.168 seconds. A Codex request was
  submitted concurrently; this is not an isolated prefill speed measurement.
- Two sequential normal `sy spark ... launch codex` smoke tests complete with
  no tools, retries, or errors. A preceding Codex test overlapping the full-context
  prefill reconnects once and then succeeds; that client-observed limitation remains
  recorded in `optimized-codex-hello.jsonl`, not hidden as a clean pass.
- Generation 8 retains its original PID and zero Docker restarts through these
  checks. See `optimized-post-promotion.log` and the native startup capture.

## Optimized decode measurements

The fixed benchmark uses one warmup and three measured 512-token samples per
prompt, temperature zero, with no concurrent qualification traffic. All six
measured outputs reach the intentional output cap (`response.incomplete`).

| Prompt | Recorded control median tokens/s | Optimized median tokens/s |
|---|---:|---:|
| Rust LRU implementation | 32.174 | 39.890 |
| Linux virtual-memory prose | 27.768 | 32.060 |

These are client-observed decode rates, about 24% and 15% above the historical
control on the same fixture. Median first-output latency is 266.703 ms and
269.432 ms respectively. This small paired workload is not a universal speedup
or tail-latency claim. The control was not redeployed for this follow-up.
Exact request hashes, metadata, and samples are in `optimized-benchmark.json`
and `optimized-comparison.json`.

## Prefix-cache and stress acceptance

All 12 prefix-suite results complete with correct answers and identical text
on repeated prompts (`optimized-prefix.jsonl`).

| Input tokens | First-output time, first request | First-output time, repeat |
|---|---:|---:|
| 31,851 | 13.263 s | 1.488 s |
| 127,853 | 56.360 s | 1.840 s |
| 239,453 | 114.324 s | 1.963 s |
| 261,053 | 130.665 s | 1.879 s |

Two concurrent 63,050-token retrieval requests and their repeats pass as well.
These are individual observations, not latency distributions. The repeat of
one concurrent request takes 11.582 seconds while competing with its peer;
prefix caching does not promise latency isolation between concurrent requests.

`optimized-edges.jsonl` records successful prefix revisits after aggregate input
exceeds cache capacity, prefix growth, client disconnect after three deltas,
and a successful subsequent response. A final sampled protocol rerun also passes.
Native inspection after stress confirms the same PID 2985242, zero restarts,
no OOM event, `unless-stopped` policy, and working NVML. The installed executable
hash matches the signed artifact (`optimized-final-native-status.log`).
The raw native capture is preserved in `optimized-native.log`.

## Rejected candidates

Generation 4 used release
`0.1.0-ae62e2ed166e83a9c1ce2c56b5330ede055e4296bb2a93951f4df789050b80ce`
and fingerprint
`sha256:dba09f5f4adb34246c08f0d541bd637f7d98c856fa87c3f4d2c2773654b7250c`.
It passed the startup semantic probe, but the first long/short sampled batch
failed in the shared-expert gate's BF16 cuBLAS operation at 17:41:48 UTC.
See `second-crash.log`. Sampler-only warmup is insufficient. The new route
identity check correctly invalidated health during that Docker restart.

The first Codex smoke invocation also exposed a test-setup issue: using
`--ignore-user-config` restored Codex's default web-search tool, which this
gateway intentionally rejects. Its `unsupported_tool` result is not a passing
test or another CUDA failure. The corrected inference-only smoke explicitly
disables web search; no gateway allowlist is weakened.

Generation 5 used the same sampler-warmup image
`sha256:fa6008389ff17911099e649aeb84b225e71e529c448aa46814ef397dc557f5ff`
with `CUDA_MODULE_LOADING=EAGER` and `CUDA_LOG_FILE=stderr`, retaining CUDA graphs.
Its profile fingerprint was
`sha256:362d78c1b95254985e463432e9ed3c01e8d7b8e2f27d6aa1d74aae00b8215346`
and release
`0.1.0-0a42a9fad61988d4501f66338f69be3b49eb11a06d62f2fe070bd4dbfd6d18be`.
It spent over 16 minutes in API-server CUDA-library initialization, before
loading model weights, and filled about 1 GiB of CUDA disk cache. It was
explicitly stopped as operationally unsuitable before the 30-minute startup
deadline, with no native restart or OOM kill. This is not evidence that eager
loading fixes inference; it never reached that test.
The captured diagnostic output is in `eager-startup.log`.

Generation 6 restored the exact control profile, with release
`0.1.0-11cf3e67ef75a6e2bb0b27469765590d9896e2b0ebac5b7d9cf49a6e9907b637`.
It started in 620.317 seconds and passed all 14 mixed sampled requests and five
sampled protocol checks (`recovery-sampling.jsonl`, `recovery-protocol.jsonl`).
The near-limit context request then failed. Docker recorded an initial exit
with code zero at 18:28:21 UTC, one restart, and removal after the restart-failure
budget was exhausted at 18:28:27. There was no new resource-emergency journal
record. Native logs disappeared with the container, so the precise engine
failure is unknown; see `generation6-exit-events.jsonl` and
`generation6-context.stderr`. It is not a qualified recovery.

The first generation-6 Codex invocation also failed as a test setup issue:
its combination of ignored user configuration and a subcommand-level `-c`
override lost the managed provider settings and attempted the default cloud
provider. That result is not evidence about Spark inference. Follow-up client
testing must retain the managed provider and verify the selected endpoint.

## Limits

The permission-loss trigger is deterministic; the original late kernel failure
is not. An already initialized CUDA context can continue working after access
is revoked, so successful short requests do not prove the container retained
its permissions. The lifecycle probe checks both NVML and a fresh CUDA context,
then exercises the existing context. Full optimized-model acceptance must run
after sy's readiness/restart-policy promotion and include sampled concurrency,
near-limit context, prefix-cache reuse, protocol/tools, and the managed client.
The generation-8 records above cover these paths, but do not establish
long-duration stability or eliminate every possible upstream CUDA defect.

Historical control images and signed bundles remain available as evidence;
they are not the current development or deployment direction. See the earlier
[optimization report](../qwen38-vllm-optimization-20260920/README.md).

See [sampling analysis](../../bugs/BUG-20260920-2015-qwen-sampling-startup.md)
and [stale-health analysis](../../bugs/BUG-20260920-2020-spark-restart-health.md).
