# Qwen3.8 vLLM optimization rollout — 2026-09-20

Production follow-up: normal sampled traffic crashed generation 3 at 17:00 UTC
after this bounded greedy qualification. See the
[sampling incident and recovery](../qwen38-vllm-sampling-20260920/README.md).
The measurements below remain historical evidence, not a current health claim.

This run applies the [research proposal](../../research/qwen38-vllm-optimization-20260920/SPEC.md)
to the existing `qwen38-vllm` instance on the user's DGX Spark. No model
weights, engine family, context limit, client endpoint, host settings or
Sparky files change. This is a bounded deployment qualification, not the
larger multilingual, sampling-distribution and long-running stability study
proposed in the research.

Outcome: the corrected image was retained as managed generation 3 after
passing the live checks below. The first candidate was rejected, fixed and
requalified; its faster measurements alone were not sufficient to keep it.

## Immutable identities

- Model: `RadixArk/Qwen3.8-Flash-Next-NVFP4@7b719225242aacd3dbd3f9407468c2ee9a9d2594`.
- Model artifact fingerprint: `sha256:26d708967d17a7b261018701fffcce2f40f2efa98476905e1a7e365db4872de6`.
- Control image: `sha256:ae03e2a6feecd27520d2598f28dde37c0f7c85c59631d8c488b5803331a6753d`.
- Rejected first candidate image: `sha256:940a052f27ca7b7700127fbfc1786f0ba9fc2ac51da02b35a1916ba017e8d50e`.
- Corrected image: `sha256:8cd67c8d2f84f34b10706df637b44d899ddb74fc495f1d1a99c0caabbbc365b6`.
- Control engine/profile fingerprint: `sha256:38615c8034314d3704b0fa98cdab9001fd4e217cae7dbfaeb9b54e0a8a4892a8`.
- Rejected engine/profile fingerprint: `sha256:6b139b7b451677d907970d1192c866b5f12a80c6f0036a05af1f8418c2a70487`.
- Corrected engine/profile fingerprint: `sha256:a5863009632630158e15f89c02395158a380232d7aa240dd791103f012f96764`.
- Corrected signed release: `0.1.0-737e742f609cc42d6002a93d26cb1873843984fc36cfa9dd9cbd8be30c74da8d`.
- Rejected signed release: `0.1.0-c98fac945c7a97c57e37d4622cc06661c080bc6ea9d66c66cf6db4e656aa2266`.
- Original control signed release: `0.1.0-13d24ef42ad7ea9ef536f3786906b462d3e6b95b2597680f219c64cc7d6b70cf`.
- Executable unchanged: `sha256:998468667074f0c0e69ef7ef876d22b0e2c87e7352790d9c11501193d242ad69`.

## Applied changes

The image retains the same vLLM preview base. Checksum-pinned community
patches update PLE gathering, restrict the MTP draft projection to 65,536
rows, repair recurrent-state copying and block-size selection, correct
GB10 FLA settings, and build deterministic QSA selection for SM121a. The
profile selects pooled PLE reads with random mmap advice, MTP=3, and prefix
caching. Target logits keep the full vocabulary. KV remains BF16/auto with
a fixed 12 GiB allocation; context remains 262,144.

Logs confirm the draft projection's 1,212 → 320 MiB read size and
`QSADET active`. The cache holds 415,179 tokens, versus 433,944 in the
control; this remains above one full-context request but is not enough for
two simultaneous full-context requests. Corrected-image startup took
713.035 seconds (the first candidate took 716.531 seconds).

The newer upstream PLE fused-convolution stride fix was checked for
applicability: this pinned preview uses the older Torch convolution path,
not that fused kernel. No unrelated newer-main patch was imported.

## Reproduction

Run from the repository root. Build and deploy using
[the developer runbook](../../../docs/how-to/develop-spark.md#rebuild-the-qwen38-vllm-image).
Run the paired benchmark with the checked-in
`tests/fixtures/spark-benchmark/qwen38-optimization.json` and
`scripts/benchmark-spark-engine.py`, supplying the exact metadata and
resource observations for each image. It uses one warmup and three measured
512-output-token samples for each of two fixed prompts, temperature zero
and reasoning disabled. Production sampling and reasoning defaults are
unchanged. These two prompts do not establish a universal speedup.

Replay the inference-only checks against a healthy instance:

```bash
python3 specs/runs/qwen38-vllm-optimization-20260920/qualify.py protocol \
  --base-url https://SPARK:9843/openai/qwen38-vllm/v1 \
  --bearer-file /path/to/token --ca /path/to/ca.pem
python3 specs/runs/qwen38-vllm-optimization-20260920/qualify.py prefix \
  --base-url https://SPARK:9843/openai/qwen38-vllm/v1 \
  --bearer-file /path/to/token --ca /path/to/ca.pem
python3 specs/runs/qwen38-vllm-optimization-20260920/qualify.py edges \
  --base-url https://SPARK:9843/openai/qwen38-vllm/v1 \
  --bearer-file /path/to/token --ca /path/to/ca.pem
```

The prefix checks use a synthetic access-code retrieval task and compare
identical cold/cached text at temperature zero. They cover long requests
and simultaneously submitted independent requests; they do not establish broad task
quality or sampling-distribution equivalence.
Run `edges` after `prefix`: it revisits a prefix after aggregate inputs
exceed cache capacity, grows a common prefix, and closes a stream during
generation before verifying a subsequent request. “Cold” in the script
means the first request of each pair, not that host or prefix caches were
flushed; growing/revisited requests may reuse retained blocks.

## Decode results

The corrected image's paired results are in `final-benchmark.json` and
`final-comparison.json`. The earlier `candidate-*` and `comparison.json`
preserve the rejected first experiment, not the final image.

| Workload | Control median tokens/s | Candidate median tokens/s | Change |
|---|---:|---:|---:|
| Rust LRU implementation | 32.174 | 42.438 | +31.9% |
| Linux memory explanation | 27.768 | 32.579 | +17.3% |

All measured responses reached the intentional 512-token output cap. The
client metric divides output tokens by elapsed time after the first delta;
it is not a GPU kernel measurement. Coding first-delta median was
274.547 → 230.065 ms; prose was 242.022 → 266.375 ms (+24.353 ms), so the
decode improvement is not a claim that every first-token latency improved.
Three repeats of one prompt per category are insufficient for a population
tail-latency claim or general quality comparison. The same request hashes,
sampling and artifact identities are archived in the paired results.

## Prefix and context results

These results belong to the corrected image (`final-prefix.jsonl`). The
rejected first candidate's earlier measurements remain in `prefix.jsonl`.

| Input tokens | First request TTFT | Identical repeat TTFT | Answer |
|---:|---:|---:|---|
| 31,851 | 13.321 s | 1.499 s | Correct and identical |
| 127,853 | 56.468 s | 2.312 s | Correct and identical |
| 239,453 | 114.466 s | 2.015 s | Correct and identical |
| 261,053 | 130.821 s | 2.150 s | Correct and identical |

These are one pair per size, not latency distributions. Each request returned
the access code from the start of the prompt with output room reserved below
the native context limit. Native logs also report prefix-cache hits. The
older control's different 261,682-token fixture took 124.048 seconds; this
is not a matched cold-prefill comparison and the current run does **not**
establish the research proposal's at-most-5% cold-prefill regression gate.
No claim of faster cold full-context prefill is made.

The concurrent pair passed again after this context sweep. The edge-case
pass then revisited the older 32k request after cache turnover, grew a common
history from 31,847 to 127,847 tokens, and verified matching repeated answers.
The grown request's first-delta latency was 45.224 seconds and its identical
repeat took 1.829 seconds. After closing a stream at its third output delta,
a subsequent request completed correctly in 279 ms to first output; native
logs then showed zero running and zero waiting requests. See
`final-edges.jsonl` and `final-native.log`.

## Concurrency regression and correction

After those sequential checks, two simultaneous 63k-token requests triggered
`CUDA error: operation not permitted` in the preview's short-conv metadata
builder. The CUDA stable sort runs only when speculative decode and regular
prefill/decode share a batch, so sequential checks missed it. See the
[captured trace](concurrency-failure.log) and
[bug analysis](../../bugs/BUG-20260920-1848-qwen-mixed-metadata.md).

The corrected image builds the same permutation from already-available CPU
metadata and transfers only the final indices. Its patch is content-addressed
and checks the original source hash before editing. The image regression test
fails on the unpatched source and passes on the corrected source, including
an 8,192-token mixed batch. Model computation and concurrency limits are not
changed. Live requalification is required in addition to this CPU test.

The corrected image passed the formerly failing pair: both 63,050-token
requests and their cached repeats returned their own expected codes.
Native logs showed two running requests and Docker's restart count stayed
zero. Responses streaming/nonstreaming, separated reasoning, parsed function
arguments and tool-result continuation also passed (`final-concurrent.jsonl`
and `final-protocol.jsonl`).

The original engine process restarted inside its Docker container, while
`sy ps` still showed the old healthy state and zero supervisor failures.
Inspect native Docker `RestartCount`, process logs and live requests as well;
the supervisor counter alone did not establish stability in this run.

## Safety and rollback

The control was stopped through `sy spark` only after it had no running or
queued requests. The release manifest was signed and verified before the
supported upgrade. Admission retained the 8 GiB memory reserve and 100 GiB
disk floor. No manual live-catalog edits or security-policy changes were
used. The old image and predecessor release remain available.

For the corrected image, 20 resource snapshots during validation recorded a
minimum 12,365,701,120 available bytes and 124,165,320,704 free disk bytes,
both above their reserves. Full-memory PSI peaked at 0.34%; sampled swap-in
deltas ranged from zero to two host-wide pages. This is not a claim of
strictly zero swap activity or attribution of those pages to vLLM. No OOM,
CUDA fault, state-copy guard skip or native restart appeared in the corrected
inference window. This bounded run is not a long-duration stability study.

After the corrected release, the immediate predecessor is the **rejected**
first candidate: do not use a blind `rollback --yes`. The original control's
signed bundle was recovered from its retained release directory, and both
its signature and all seven inventory hashes were verified locally under
`target/qwen38-vllm-opt.3clcY1/control-release`. Restore that bundle with the
normal signed `upgrade` workflow (dry run first), then serve
`qwen3.8:flash-next --name qwen38-vllm` again. The bundle's `sy-aarch64` is
the unchanged executable listed above. If local scratch is cleaned, recover
the original manifest, signature and configs from the exact retained control
release, not from the current/preceding symlinks. Never stop unrelated work
or bypass admission to fit two copies.

## Repository verification

Passed: ARM64 image build and its CPU behavior/non-root loading checks;
12 optimization contract tests; engine-image and release-catalog tests;
`make lint`; `make lint-spark`; `make fmt-check`; `make test` twice;
`make test-spark`; and `make docs-site`.
The available `make docs-lint` gate also passed its final link check (946
links, zero errors); markdownlint, cspell and Vale were not installed and
were reported as skipped, not passed.
