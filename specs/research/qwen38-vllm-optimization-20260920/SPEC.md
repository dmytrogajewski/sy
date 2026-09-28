# SPEC: More performance from Qwen3.8-Flash-Next on vLLM

Date: 2026-09-20
Status: research complete; selected patches implemented and deployed on
September 20. See the [identity-bound rollout report](../../runs/qwen38-vllm-optimization-20260920/README.md)
for measured results and remaining qualification limits. The larger experiment
matrix below remains a research proposal, not a claim of completed testing.

## 1. Summary

Request: improve single-user generation speed on the existing DGX Spark,
remaining on vLLM and preserving the exact RadixArk checkpoint and native
262,144-token context. Actor: the user and coding agents using `sy spark`.
Surface: the existing engine image/profile, not another user-facing launcher.

There is credible optimization headroom. Roughly 40–45 coding tokens/s is a
qualification target, not a measured result or guarantee; prose gains are less
certain. Reusing conversation prefixes could improve agent-turn latency much
more than the decode gain. No live engine, model, host setting, or Sparky
configuration was changed during this research.

This updates the vLLM conclusions in the
[August research](../qwen38-flash-next-dgx-spark-performance/SPEC.md).
Its blanket statement that vLLM prefix caching is unsafe needs qualification:
it remains unsafe to enable blindly on our installed image, but newer
community patches provide a concrete candidate fix.

## 2. Background and evidence

### Local baseline

Source files:

- [Engine profile](../../../configs/sy/spark/engines/vllm-qwen38-mmap.toml)
- [Engine Dockerfile](../../../configs/sy/spark/engines/vllm-qwen38-mmap.Dockerfile)
- [Benchmark harness](../../../scripts/benchmark-spark-engine.py)
- [Earlier near-limit result](../../runs/qwen38-vllm-262k-result.json)

| Property | Current value |
|---|---|
| Instance | `qwen38-vllm`, generation 1 during this investigation |
| Checkpoint | `RadixArk/Qwen3.8-Flash-Next-NVFP4@7b719225242aacd3dbd3f9407468c2ee9a9d2594` |
| Image digest | `sha256:ae03e2a6feecd27520d2598f28dde37c0f7c85c59631d8c488b5803331a6753d` |
| Engine/profile fingerprint | `sha256:38615c8034314d3704b0fa98cdab9001fd4e217cae7dbfaeb9b54e0a8a4892a8` |
| PLE patch revision | `d2854bfff0a0b6f46984b0941ed1db6010031295` |
| Context / KV allocation | 262,144 / 12 GiB BF16 (`auto`) |
| Scheduling | 8 sequences; 8,192-token prefill chunks; PIECEWISE graphs |
| Speculation / prefix cache | MTP=2 / disabled |
| PLE | File-backed mmap; 32 workers; prewarm disabled |

The preceding live benchmark used the existing harness's `run_sample`, streamed
Responses through sy, temperature zero, reasoning disabled, one request at a
time, three 256-output-token samples per category. Session measurements:

| Category | Decode samples, tokens/s | Median | Median first-delta latency |
|---|---|---:|---:|
| Rust coding | 33.941, 35.214, 35.149 | 35.149 | 271.605 ms |
| Explanatory prose | 26.829, 27.935, 27.846 | 27.846 | 278.638 ms |

These are short interactive measurements, not a comprehensive quality or
long-context qualification. The harness divides output usage by time after
the first delta; this is an approximate client decode metric, not GPU kernel
throughput. All six requests reached the intentional output cap. Raw prompt
hashes for these interactive samples are not archived here; qualification
must save complete harness artifacts.

Live log windows around 14:41–14:42 UTC show mean MTP acceptance length around
2.8 for code and 2.2–2.3 for prose. Inference: more accepted tokens per engine
step explain much of the category gap; MTP is already helping. The logs are
windowed counters, not exact per-request profiles.

The older, identity-matched near-limit artifact has 261,682 input tokens and
124.05 seconds to its first output, approximately 2,109 input tokens/s. Its
single-token decode figure is not meaningful and must not be advertised.

### Comparable vLLM approaches

| Approach | Evidence and fit |
|---|---|
| Official vLLM recipe | Native MTP and 262k support; published hardware examples are larger multi-GPU systems, not a drop-in Spark memory recipe. [Official recipe](https://recipes.vllm.ai/Qwen/Qwen3.8-Flash-Next) |
| Updated blazux patch stack | Closest to our existing image. Examined revision `5be66376e8beaf96655f2d5682c82d538a970e66`, dated September 18, is 60 commits ahead of our pin. [Comparison](https://github.com/blazux/qwen3.8-Flash-DGX/compare/d2854bfff0a0b6f46984b0941ed1db6010031295...5be66376e8beaf96655f2d5682c82d538a970e66) |
| Staged-read vLLM implementation | Reports 44.3 coding and 29.0 prose tokens/s on one Spark, using NVIDIA weights, MTP=3, FP8 KV, staged PLE reads, and 262k context. Different checkpoint and several changed variables: evidence of feasibility, not our expected A/B gain. [Pinned report](https://github.com/tonyd2wild/Qwen3.8-Flash-Next-NVFP4-DGX-Spark/blob/6ad1c8f15cbab1ababd2048e8e5f94094dbfc4a0/single-spark-vllm-tp1/README.md) |
| Low-M matrix-kernel tuning | A same-hardware investigator found draft shortlisting benefits shrank after fixing kernel selection. Useful counterevidence against treating a vocabulary-size win as universally transferable. [Author's experiment](https://huggingface.co/blog/krisbailey/shortlist-mtp-mostly-worked-around-a-bad-kernel-ch) |

Both examined recipe repositories supply Apache-2.0 licenses; preserve notices
and verify licenses for each imported asset. Their launch scripts are not an
alternative appliance control plane.

### Candidate mechanisms

**PLE reads.** Newer code deduplicates requested embedding rows, reuses pinned
staging, and asynchronously copies the gathered rows. It also supports
`MADV_RANDOM` and controlling the inline-versus-thread-pool threshold. It still
synchronizes to obtain row IDs: this is not complete CPU/GPU overlap.
The module's `FAST_ROWS` default is 512, so the recipe's newer choice of zero
must be explicit in sy's environment. These variables do not retrofit missing
code into our old image.
[Pinned implementation](https://github.com/blazux/qwen3.8-Flash-DGX/blob/5be66376e8beaf96655f2d5682c82d538a970e66/src/vllm_ple_mmap.py).

The recipe reports 35.1 → 37.9 tokens/s from always using the worker pool;
that experiment used NVIDIA-derived weights, hybrid quantization and MTP=3.
Its sequential tuning series reports 33.5 → 38.5 after reduced-vocabulary
drafting, then 41.2 with MTP=3. These are different conditions from ours and
the gains are not additive. The same report gives repeated-20k-prefix latency
around 14 → 1.4 seconds. Its newer default also changes checkpoint, which we
must not import silently.
[Pinned results](https://github.com/blazux/qwen3.8-Flash-DGX/blob/5be66376e8beaf96655f2d5682c82d538a970e66/README.md).

**Draft head.** The patch retains the target's full vocabulary and creates a
65,536-row draft-only copy. Calculated from the code's BF16 dimensions, each
projection reads about 1.18 GiB for the full head versus 320 MiB for the slice.
It adds a 320 MiB resident copy; it does not free the target head. It currently
uses generic `torch.nn.functional.linear`, making kernel selection a genuine
confounder. Rejection verification preserves the target distribution in the
algorithm; finite-precision and implementation behavior still need testing.
[Pinned patch](https://github.com/blazux/qwen3.8-Flash-DGX/blob/5be66376e8beaf96655f2d5682c82d538a970e66/src/patch_mtp_draft_vocab.py).

The counterexperiment reports a roughly 6.9% single-stream step-rate benefit
from low-M kernel tuning, with only about 4% extra from shortlisting at MTP=2
after matching kernels. Different weights and workloads prevent direct
comparison. Hypothesis: optimize projection dispatch and vocabulary jointly,
not vocabulary size in isolation.
[Matched-kernel experiment](https://huggingface.co/blog/krisbailey/shortlist-mtp-mostly-worked-around-a-bad-kernel-ch).

**Prefix caching and correctness.** The recipe traces corrupt restores to
using the QSA ring block size where the Mamba state block size was required.
It patches worker restoration and scheduler alignment. Deterministic QSA
selection is a separate correctness dependency; bounds guards alone did not
fix cache-hit output corruption. Its cached-versus-cold validation is promising
but does not establish our full-context safety.
[Root-cause analysis](https://github.com/blazux/qwen3.8-Flash-DGX/blob/5be66376e8beaf96655f2d5682c82d538a970e66/docs/HOW-IT-WORKS.md).

As checked September 20, [the original issue #54173](https://github.com/vllm-project/vllm/issues/54173)
remains open, [deterministic top-k PR #55122](https://github.com/vllm-project/vllm/pull/55122)
is unmerged, and [FP8 KV PR #54846](https://github.com/vllm-project/vllm/pull/54846)
is unmerged. [PLE state-stride PR #55375](https://github.com/vllm-project/vllm/pull/55375)
merged September 5. Check applicability to the exact base instead of assuming
that a release upgrade incorporates every fix.

Prefix caching accelerates shared-prefix processing, not generation of new
tokens. Keep TTFT and decode claims separate.
[Official APC documentation](https://docs.vllm.ai/en/latest/features/automatic_prefix_caching/).

## 3. Proposal and decisions

| Decision | Recommendation | Alternative and tradeoff |
|---|---|---|
| Runtime | Keep vLLM; qualify a checksum-pinned patch image | Moving engines violates this request |
| Checkpoint and context | Keep exact RadixArk weights, 262k, BF16 KV and fixed 12 GiB budget | Hybrid side-layer quantization or FP8 KV changes the numerical comparison |
| PLE | Compare updated mmap implementation against the control; test 32/64 workers and explicit inline threshold | Staged `preadv`/decode graphs is a separate experimental arm, not a flags-only shortcut |
| Drafting | Compare full/65k vocabulary with appropriate kernels; MTP=2 versus 3 | More speculative tokens can waste work when acceptance falls |
| Prefix reuse | Enable only with applicable state/selection fixes and cache-hit correctness tests | Leaving caching off protects the control but retains repeated-prefill latency |
| Base upgrade | Treat patched v0.29.0 as a separate compatibility comparison | Current pinned preview base isolates patch effects with less simultaneous change |

Scope includes patch provenance, image build, declarative configuration,
correctness repairs required by chosen optimizations, performance evidence,
full-context qualification, safe release/rollback, and developer documentation.
Research alone does not authorize executing those changes; the linked rollout
followed the user's subsequent request to apply them.

Anti-goals: replacing vLLM; silently changing weights, reducing context or
disabling reasoning for production; bypassing memory admission; host swap,
clocks, kernel or driver tuning; importing another project's launcher; changing
Sparky. These would change the user's contract, the comparison, or the
appliance trust boundary.

FP8 side layers/KV remain separate numerical-change proposals, not part of the
recommended unchanged-checkpoint candidate. Increasing concurrency is not a
single-user tokens/s optimization. Persistent compile caches already exist in
our profile; their presence is not a missing decode optimization.

## 4. Technical design and qualification

### Integration

Keep changes in `configs/sy/spark/engines/vllm-qwen38-mmap.Dockerfile`, its
TOML profile, pinned build assets and their contract tests. No model-name
branches, new IPC operations, Rust dependencies, database migrations or CLI
flags are required by the proposal. Preserve non-root execution, image and
artifact identity checks, private networking, authenticated routes and signed
release inventory. Bake draft vocabulary into the image with a checksum.

Use the existing release process in
[Develop and release the Spark plane](../../../docs/how-to/develop-spark.md).
Observe actual startup/steady memory, compilation storage and free disk before
declaring new resource envelopes. Preserve admission's 8 GiB reserve and
100 GiB disk floor; do not assume the current 114 GB engine envelope is valid
after adding buffers. No two full-size engines may bypass admission to run
side-by-side. Preserve the previous image and record an explicit rollback.

Sparky's LM-head benchmark documentation is useful experimental context, not
evidence of a qualified vLLM speedup. Its catalog, engine code and UX remain
independent.

### Experiment sequence

1. Archive a fresh control with exact identities, request hashes and sampling.
   Include both deterministic performance prompts and production sampling.
2. Qualify necessary correctness fixes with prefix caching still disabled;
   compare them separately so incorrect old outputs are not the golden oracle.
3. Test the PLE implementation and row-pool settings, then draft head/kernel
   combinations, then MTP depths. Hold other variables constant per comparison.
4. Test repaired prefix caching against the corrected uncached candidate,
   including eviction, cancellation, concurrent prefills and context growth.
5. Evaluate staged reads/graph mode and the newer base as separate arms.
   Select by complete agent workload results, not kernel-only speedups.
6. Promote only an identity-bound passing candidate through the signed release
   workflow after explicit deployment authorization.

### Tests and acceptance

- **Measurements:** at least ten diverse prompts per category, three measured
  repetitions after warmup, 512–1,024 generated tokens for sustained decode;
  code, prose, JSON/tool calls, reasoning and relevant non-English text.
  Record first answer/reasoning delta, completion time, decode rate, MTP
  acceptance, cache hits, PLE gather/copy time and failures. Separate single
  stream from aggregate throughput at concurrency 2/4.
- **Comparison hygiene:** alternate control/candidate order across repeated
  runs, record cache state and concurrent traffic; do not flush host caches.
  Save complete JSON results using the existing harness. Report medians and
  tail latency with enough observations; do not infer p99 from three samples.
- **Promotion proposal:** at least 10% median code-decode improvement across
  repeated comparisons, with no more than 5% prose or cold-prefill regression.
  For cached repeated 32k/128k prefixes, target at least 2x TTFT improvement.
  These are proposed gates, not observed results; expand samples if variance
  makes the comparison inconclusive.
- **Context:** exercise growing 32k/128k/240k histories and a near-262k request
  with reserved output room; evaluate retrieval and multi-turn state, not just
  startup capacity. Repeat with reasoning and tool continuation enabled.
- **Correctness:** bit-exact PLE gathers over repeated/random/cross-shard IDs;
  draft vocabulary range/special-token coverage; deterministic cache-hit versus
  cold outputs and logprob tolerances against the corrected control; seeded
  sampling distributions and task-level quality, not unconditional same-seed
  text identity across differing kernels.
- **Protocol:** Responses streaming/nonstreaming, reasoning separation, tool
  arguments and continuation, literal tool-marker text, cancellation and
  terminal events. A faster broken stream fails qualification.
- **Safety:** zero OOMs, restarts, CUDA faults or state-copy guard skips; no
  swap-in dependence or admission-floor violations during the bounded suite.
  Retain longer-running stability evidence before making a production default.
- **Repository gates for implementation:** extend
  `tests/spark_engine_image_contract.rs`, `tests/spark_release_catalog_boundary.rs`
  and `tests/spark_benchmark_harness.rs` where behavior changes; run default
  lint/tests plus `make lint-spark` and feature-minimal Spark tests. Research-only
  documentation does not assert those implementation gates have passed.

### Compatibility and observability

Keep `sy spark ... serve`, `launch`, `logs`, `status` and client endpoints as
the supported surface. Existing JSON schemas and exit codes need no change.
Expose only bounded, redacted counters through existing permitted surfaces;
do not publish the engine's private debug/metrics routes. New image/profile
fingerprints distinguish evidence and compilation caches.

## 5. User journey

The developer builds and qualifies the candidate, the operator reviews its
measured tradeoffs, a signed release installs the chosen profile, and the user
continues launching the same model through `sy spark`. Regressions select the
recorded prior image rather than requiring manual container repair.

| Friction | Required handling |
|---|---|
| Long restart and rebuild cycle | Scheduled authorized transition; persistent compile cache; explicit readiness |
| Published results use different weights/settings | Exact identity and per-category control comparisons |
| Memory permits only one full engine | Serialized admission; no hidden competing model process |
| Independent Sparky research profile | No dependency, default selection change or extra user launcher |

## 6. Risks and open questions

The principal risk is trading correctness or full-context stability for a
short-prompt speedup. Cache/state tests and long-context protocol journeys are
mandatory. Draft shortlisting may lower acceptance on unfamiliar languages;
test the user's language mix. Patch licensing/provenance and compatibility
must be checked asset by asset.

Unresolved by research: our actual PLE versus projection time split; the best
low-M kernel on this pinned build; vocabulary/depth wins under production
sampling; cache-hit correctness at 262k; and the new resource envelope.
Public reports cannot answer these without device qualification.

## 7. Hand-off

Research recommends qualification of an optimized vLLM image, not an engine
switch or immediate live update. The next authorized implementation should
turn these gates into tests and preserve the simple `sy spark` user surface.
