# Qwen3.8 Flash Next SGLang qualification on DGX Spark

Date: 2026-08-29

Device: `dgx-spark`, one GB10, 128 GB unified memory. This run did not
change the host OS, kernel, NVIDIA driver, CUDA installation, Docker engine,
firmware, clocks, swap, or boot configuration.

## Immutable inputs

- Model snapshot: `RadixArk/Qwen3.8-Flash-Next-NVFP4` at
  `7b719225242aacd3dbd3f9407468c2ee9a9d2594`.
- SGLang source: `d91c3682b0b429e4c70df63cd57f819588ce29b0`.
- Single-Spark recipe source:
  `04d073518ded5d0db1cddce74d9afb1cdca5eddc`.
- Base ARM64 image:
  `sha256:14ed582518584c5c830206b5318a2c2769e68229c3422e48a28b952b3a888bd4`.
- Qualified candidate image:
  `sha256:b09ca2c158b46ccb9690069aa2fd77ab7761df01cb279a13249f00cdd5cea197`.
- Candidate image size: 14,946,517,403 bytes.
- Engine configuration:
  `configs/sy/spark/engines/sglang-qwen38-mmap.toml`.

The image applies only content-addressed source transformers. Docker build
checks their hashes, their exact upstream anchors, Python syntax, and required
postconditions before publishing the image. Runtime does not download or
patch code.

## ARM64/SM121 image build evidence

The final clean build ran on the Spark from
`/home/drwatsno/sy-step9-warmflush.DsxjDb` with:

```console
sudo docker build --pull=false --no-cache --progress=plain \
  --file sglang-qwen38-mmap.Dockerfile \
  --tag sy-spark/sglang-qwen38-mmap:step9-warmflush .
```

The Dockerfile SHA-256 was
`ab48668cd71c589f941e8a020360f800b700ca6884b642f03ec3a2e28a517153`.
Its pinned base manifest was
`14ed582518584c5c830206b5318a2c2769e68229c3422e48a28b952b3a888bd4`;
the base manifest label was
`12d3392bdc8be8d35e9a95f191df6aef99c5114bdbefd41bfdc7e760e6d25ec1`.
The SGLang source revision was
`d91c3682b0b429e4c70df63cd57f819588ce29b0`; the single-Spark recipe
revision was `04d073518ded5d0db1cddce74d9afb1cdca5eddc`.

The content-addressed transformer and self-test hashes were:

- `2edb46d75172645bc444853563385f480b5bfb872127d0b281fea2f5a8fc6e28`
- `12714616ca51148aeee0089153826230adc15b66031eba3de07586a81f6544db`
- `6530076f56575c6754375ba944f0911282999c30bbd2d5b1bdb7e89cf5e5b4aa`
- `708e870359a4d0f390b60096a6428506af013a8aaf7d796783e689f432ff4744`
- `9f228eb6db985bd17fb21b051e747841da8fd37ac5e131228c15fa4cca2dc669`
- `ba23e86521ba43731da3eff8b58bd438529f6a3341de5c351f01ce898b581372`
- `bbaec9843bab2d859db87f05cbd8929638aa6caf1e2cc95754edd89ce4dbad3b`
- `e373f3b21a12ce62066d8b1a3bd8390d7946651def00b5d54e976d07b97a8510`
- `e4492a172636e0cc6d55b8baebf29313d687ca56bb6d5f2a155f4de3f00b78e0`
- `eeabdde061631c9b606d4ccc7371ff8fb01c6cc034dfe6bad1e4f29a8aa21555`
- `f60ccb9f9e350a43155a1a7a20d154be0b7e93c29dacb3db95d397ba910090b2`
- `f969092584d6a9d2c8e74f6a9720ed7d4bdb51fca69962b88a8470282f88ce78`

The emitted ARM64 manifest-list/repository digest was
`b09ca2c158b46ccb9690069aa2fd77ab7761df01cb279a13249f00cdd5cea197`.
Its platform manifest was
`bf4e77f364b3b7499275d476f0abf8ed407ad60a1f9fa1d27f9520a39573a2f7`,
its config was
`08a796b80280e36ca6db20a93884c19d495895eadb5d211f922fbaa1fc3397b2`,
and its provenance attestation was
`46be191101c1b425065f82867be60ae099a70f97f61aa23e11ef8bf5a744e3c9`.
Docker inspected it as `arm64`, UID/GID `65534:65534`, workdir `/tmp`, and
14,946,517,403 bytes with 87 rootfs diff IDs.

A cached rebuild of the same inputs emitted the same platform manifest,
config, 83 rootfs diff IDs, OCI labels, size, and package freeze. Its outer
manifest-list digest was
`96e95279ab83a91ba902e0b6560a540749dd9fe926ae3cf3a75926a547525290`
because BuildKit emitted a fresh provenance attestation
`802b65b68ea6c9da044c19d2f7f3d8adbec6ab8bf19e7ccc0d039e37b60d97f7`.
The content identity did not drift; only the time-dependent attestation
descriptor changed.

Exact installed-package, SPDX 2.3 SBOM, OCI-label/rootfs, and layer-history
evidence is retained on the Spark under `/var/lib/sy-spark/build-evidence`:

| Evidence | Records/bytes | SHA-256 |
| --- | ---: | --- |
| `b09ca2c158b46ccb-packages.freeze` | 1,258 / 46,251 | `c48f19f9a32d128fffd9584bd70de128f9390c803c92f70df5b804b8c5b6de5d` |
| `b09ca2c158b46ccb.spdx.json` | 1,258 packages / 636,824 | `6eee229942d2bde0437d4eafadac6af07d399730aeda5dee35a9fda125eaff88` |
| `b09ca2c158b46ccb-inspect.json` | 222 lines / 17,981 | `da9b953153bba774e8e333135625cd7ae1b7525facb1103f92a8d4c275742d74` |
| `b09ca2c158b46ccb-history.jsonl` | 195 lines / 75,786 | `3df9d0e5461a5925fbfdb420a2612c150d8bd3d8d46e630db45872a16b21f429` |

`tests/fixtures/spark-engine/image_inventory.py` generated both package
artifacts from the final image without a package manager or network. Its
SHA-256 was
`74e69ea66184900d06a553f598bdb59906152b70b3011b7a7ed7e8fad79fcb04`.

Build-time self-tests passed the patch identities, Python imports, immutable
source checkout, non-root home and cache permissions, synthetic PLE mmap
gathering, durable create/verify/reuse/cancellation/regeneration, SM120 QSA
resolver, and declarative runtime-profile checks. Final OCI policy labels retain
only the declared offline JIT toolchain and reject package managers, pip, uv,
SCM, and network fetchers. Generic Rust contracts discover the build-tool
classification and retained/rejected sets from those labels; no configured
classification, tool name, image path, engine identifier, or model identifier
is duplicated in the test code.

The source-backed GB10 probe
`tests/fixtures/spark-engine/sm121_runtime_jit.py` (SHA-256
`7c203e0095e5a7760aed874319044308db2135abb228296fae2fc65f281a8623`)
ran in the final image with no network, a read-only rootfs, UID 65534, and only
the declared tmpfs cache roots. It reported NVIDIA GB10 capability `[12, 1]`
and compiled and executed Triton, TorchInductor, and TileLang CUDA kernels.
This reproduced and corrected the earlier invalid compiler-stripping policy.

The final clean build began with 127,424,806,912 bytes available. The lowest
observed availability through export, unpack, inspection, and first-kernel
qualification was 122,293,215,232 bytes (113.9 GiB), consuming at most
5,131,591,680 temporary bytes and preserving the configured 100 GiB reserve.
The lowest observed `MemAvailable` across the Step 8 builds was approximately
14.9 GB, above the 8 GiB floor. The existing roadmap-owned qualification
instance had zero running, queued, paused, or retracted requests before it was
stopped through authenticated `sy` operation
`01M1646YEET58HMK4G2MPXJVCA`; it remained stopped after the probe.

Host inventory remained Ubuntu 24.04.4 LTS, `aarch64`, kernel
`6.17.0-1022-nvidia`, NVIDIA driver `580.159.03`, CUDA capability 12.1, and
Docker 29.2.1. No host package, OS, kernel, driver, CUDA, Docker, firmware,
clock, power, swap, or boot update was performed.

## Startup evidence

The signed candidate-priority release served exact image
`sha256:b09ca2c158b46ccb9690069aa2fd77ab7761df01cb279a13249f00cdd5cea197`
through the normal `sy spark dgx-spark serve qwen3.8:flash-next --name
qwen38-sglang-qualification` journey. The executor launched UID 65534 on only
`sy-spark-internal`; Docker reported no port bindings, no OOM, and zero
restarts. Offline environment declarations prohibited Hugging Face,
Transformers, and dataset downloads at runtime.

Both measured generations retained native `context_len=262144` and
`max_total_num_tokens=262144`. The ReplaySSM allocation used one running
request and five declared Mamba state slots. Reasoning, three-step/four-draft
NEXTN MTP, target verification, draft and decode CUDA graphs remained enabled;
only the profile's declared prefill graph backend was disabled. The full
18-bucket warmup ran through 32K, then the typed idle-only
`flush_transient_allocator` hook completed before readiness.

| Lifecycle | Operation / generation | Managed startup | Readiness evidence |
| --- | --- | ---: | --- |
| cold | `01M16P83PP9NG792KDE23Z3G5P`, g19 | 787,858 ms | full warmup and flush, health/semantic probes, restart 0 |
| warm cache reuse | `01M16RRX0SCRM1SV080S2ZSQXB`, g20 | 740,162 ms | main weights 83.97 GB in 498.82 s; 32K warmup 212 s; flush; restart 0 |

Generation 19 then completed one managed authenticated generation through the
gateway in 1,336.823 ms: 63 input, 36 output, and 99 total tokens. The output
body was not retained; its SHA-256 was
`c25b22b6c79335db687f09816f6a3ecf7381dd94442d89853b7860804848c70f`.
No `CUDA error: operation not permitted`, EAGLE verification failure, restart,
or OOM followed health or that real generation.

The warm lifecycle reused the completed 51,200,245,760-byte PLE artifact from
the authoritative namespace
`sha256-084df4ebb29c18c43eaf58c89e8f4c718ea45af5ab00ec4aa8a80ebcbcb7c413/i_2f77a431248152edcc81ed690177e3b4`.
The log recorded `PLE cache reused: verified read-only artifact`; disk did not
grow by a second table. After warmup, only 590,929,920 bytes (1.154%) of the
PLE inode were resident. Its source and authoritative links had the same inode,
0440 mode, byte length, marker identity, tensor shape/dtype, source hash, and
transform revision.

Cold readiness never fell below 9,014,548 KiB `MemAvailable`; the complete
cold lifecycle including the real generation bottomed at 8,947,272 KiB. The
warm lifecycle bottomed at 9,143,988 KiB and was stable at 9.15--9.16 million
KiB after readiness. The strict 8 GiB floor therefore remained green. Cold and
warm cgroup maxima were 52,822,011,904 and 54,396,260,352 bytes. Cold swap-free
movement was 13,964 KiB and warm movement 9,436 KiB; neither run OOMed, and PSI
showed no sustained stall. Disk availability stayed above 111,438,606,336
bytes, preserving the 100 GiB reserve.

The exact measured cold deltas replace estimates in TOML:
`startup_peak_bytes=115068534784` and
`steady_peak_bytes=115137425408`.

Managed stop `01M16SJQDHABTR0M7B4CTJ1759` completed in 7.1 seconds and reaped
g20 exactly. Serve operation `01M16SKENAT8JVNSAC1APNB0NA` was then cancelled
after g21's container existed; it reached `cancelled` and left no container or
process. The exact previously signed vLLM-preferred release
`0.1.0-d622b71c06eac1c3666cea44545d9fda5f27ada1f21f8d27467edc486bcfc36a`
was restored through signed `sy spark upgrade`, not a manual symlink. SGLang
returned to priority 190 below vLLM's 200. Both services were active with zero
restarts, the protected fingerprint remained
`7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510`,
and doctor reported only the documented accepted internal peer-lateral risk.

This Step 9 run proves the declared 262,144-token allocation, not a 262K prompt.
Long-prompt qualification remains owned by Step 13.

## Gateway protocol evidence

Generation 22 reused managed instance `i_2f77a431248152edcc81ed690177e3b4`
and the authoritative opaque compile-cache namespace. Its engine fingerprint
was `sha256:b212caf3b64f6f46a8227ce96aeefd3ce174594d20b7ca84ba89c5e4b7238a1b`;
the artifact fingerprint was
`sha256:26d708967d17a7b261018701fffcce2f40f2efa98476905e1a7e365db4872de6`.
All requests traversed authenticated sy HTTPS rather than a directly
published engine port. Retained evidence contains only structured outcomes,
counts, timings, and content hashes.

| Protocol case | Result | Observed client time |
| --- | --- | ---: |
| OpenAI Chat, non-streaming | separate reasoning, final text, finish reason, usage | 1.149 s |
| OpenAI Chat, streaming | reasoning/text deltas, finish reason, usage, `[DONE]` | 1.153 s |
| OpenAI Responses, non-streaming | reasoning item, message item, completed usage | 1.086 s |
| OpenAI Responses, streaming | typed reasoning/text events and completed usage | 0.990 s |
| Anthropic Messages, non-streaming | thinking, signature, text, usage, stop reason | 2.586 s |
| Anthropic Messages, streaming | indexed thinking/signature/text events, usage, stop | 2.191 s |
| OpenAI Responses, one required strict tool | one valid function call | 2.003 s |
| OpenAI Chat and Responses, two native calls | two same-function calls with distinct valid inputs | pass |
| OpenAI Responses, two streamed tools | two distinct valid function calls | 3.355 s |
| OpenAI Responses tool-result continuation | reasoning and completed final message | 3.592 s |
| Anthropic streamed tools | two distinct tool-use blocks with valid input JSON | 3.081 s |
| Anthropic tool-result continuation | thinking and final text with `end_turn` | 4.497 s |
| Anthropic signed-thinking follow-up | accepted prior signature; new thinking/signature/text | 1.393 s |
| Anthropic token counting | 70 input tokens | 0.062 s |
| Mid-stream cancellation and health request | client closed after first event; next reasoning turn healthy | 0.210 s; 3.517 s |

Streaming and non-streaming final text produced the same SHA-256
`1bc32b597c92ba5a89fcf5f525af1a12c4c2bddac829a03201458dc09fbafc7d`
for the deterministic marker fixture in all three public surfaces. Reasoning
was non-empty and remained separate in every generation. The engine-neutral
fixture tests additionally prove byte-exact terminal document, block,
signature, finish, and usage equality from the same deterministic upstream
event sequence.

The pinned `Qwen3CoderDetector`, its multiple-call unit test, and the SGLang
Responses streaming order test all preserve consecutive tool-call blocks.
The checkpoint template also serializes every historical tool call. Live
`required` plus strict structural constraints nevertheless produced one
valid call for a two-location request; this satisfies required's at-least-one
contract but is a candidate limitation. Protocol-native `auto` with
non-strict schemas produced exactly two valid calls in Chat, Responses, and
Anthropic without tags in prompts, synthesized calls, or gateway branches.
Generation 22 remained healthy with zero restarts after cancellation and the
entire matrix.

## Engine-neutral paired benchmark harness

Step 11 added `scripts/benchmark-spark-engine.py` as a separate repository
tool; ordinary `sy spark ls` and `sy spark ps` output is unchanged. The final
paired plan is `tests/fixtures/spark-benchmark/paired.json`, SHA-256
`14019a1adc8cf81a26ad2d2d8ee6d4cd11e97c6f51b18ef6ab63f360f77e2b6d`.
It fixes temperature 0, a 400-token output ceiling, one warmup, ten measured
samples, and fixture-owned timeouts. Code, prose, reasoning/tool,
disjoint cold-prefix, and explicitly growing-prefix requests have distinct
workload identities and remain separate in raw samples and summaries.

Live invocation requires the authenticated base URL, a bearer *file*, CA,
immutable model/engine/image/profile metadata, and separately sourced scalar
observations:

```console
python3 scripts/benchmark-spark-engine.py \
  --fixture tests/fixtures/spark-benchmark/paired.json \
  --base-url "$BASE_URL" --bearer-file "$BEARER_FILE" \
  --metadata "$IMMUTABLE_METADATA" --observations "$OBSERVATIONS" --ca "$CA"
```

The result retains only plan/sampling/request hashes, immutable fingerprints,
client-observed TTFT/total time and explicitly named client-side token-rate
estimates, token usage, event counts, terminal state, and generated-content
SHA-256. Prompt copies, generated reasoning/text/tool arguments, and bearer
content are never emitted. Mid-stream client cancellation is
`client.cancelled` with null usage/rates and cannot be reported as a completed
generation. Native MTP/prefix metrics, resource readings, and lifecycle facts
remain under `external_observations`; they are not relabeled as client timing
or inferred from TTFT. Comparison rejects missing immutable identity, the same
engine identity, plan/model/sampling mismatch, non-monotonic timing,
inconsistent usage, missing/duplicate sample indexes, unpaired workloads, and
incomparable workload kinds.

The final dry fixture emitted stable JSON for 55 planned requests with
sampling SHA-256
`ccc73314d2af1ee6e2fedc553f175a7208274c2ec9983ef78add8d4fd6a7eac0`.
The fragmented fixture server exercised reasoning, text, tool arguments,
tool-item completion, usage, terminal events, and client cancellation without
retaining its synthetic secret bodies.

A final-schema generation smoke was initially not admitted on the restored
vLLM-preferred release. Authenticated normal `serve --dry-run --json` selected
engine fingerprint
`sha256:8cd144769e9e50963f428ccf9c8e1e78ef1d11adf4a341d4b329f8b4b37c01b1`
and image
`sha256:ae03e2a6feecd27520d2598f28dde37c0f7c85c59631d8c488b5803331a6753d`,
but correctly returned `spark.disk.reserve`: 98,848,808,960 bytes projected
available, below the 107,374,182,400-byte reserve. The three exact stopped
vLLM instance namespaces were also dry-run through normal sy state and all
remained below the reserve. No cache was promoted or removed and no reserve
was weakened.

The red preflight was made green through the retained signed SGLang-priority
release, not by changing a floor. Exact instance
`i_2f77a431248152edcc81ed690177e3b4` and authoritative cache namespace
`sha256-084df4ebb29c18c43eaf58c89e8f4c718ea45af5ab00ec4aa8a80ebcbcb7c413/i_2f77a431248152edcc81ed690177e3b4`
were admitted and generation 23 became healthy in 746,105 ms with zero
restarts. Warm startup reused the verified read-only PLE, loaded NVFP4 and its
MTP head, enabled QSA MTP index sharing, captured target/draft CUDA graphs,
warmed all 18 prefill shapes, and flushed transient allocator state.

The final one-sample authenticated smoke used final plan SHA-256
`b1164760ea3a734d57e97c4df775fad84b586ffa7d86f53ec00e3cf35ef37c23`.
It completed with 56 input, 26 output, and 82 total tokens: client-observed
TTFT was 410.569 ms, total time 1,031.651 ms, and client decode was 41.862
tokens/s. Retained evidence contains only hashes, timing, counts, terminal and
event identities, plus separately labeled observations. Managed stop reaped
generation 23. Signed upgrade then restored exact vLLM-preferred release
`0.1.0-d622b71c06eac1c3666cea44545d9fda5f27ada1f21f8d27467edc486bcfc36a`.
Final state was idle and healthy with protected fingerprint
`7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510`,
123,780,255,744 bytes `MemAvailable`, zero full-memory PSI and swap-in delta,
and 111,732,101,120 bytes disk available.

### Preliminary g22 short-context evidence

The versioned plan is `tests/fixtures/spark-benchmark/short.json`. It sends one
warmup and ten measured requests for each workload through authenticated sy
Responses streaming. The harness retains client timing, usage, terminal and
event counts, plan hash, and immutable model/engine fingerprints; it does not
retain generated model text. The plan hash was
`ccda986616709f841c92382e70368cd155773104f195e8b6ed5a8e3507be3569`.
This run used the interrupted pre-final harness schema and is retained as
historical engine evidence only; it is not an input to the paired comparison.

| Workload | Samples | Median TTFT | p95 TTFT | Median decode | Decode range | Result |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| code | 10 | 242 ms | 434 ms | 31.5 tok/s | 30.0–35.8 tok/s | 400-token ceiling in every sample |
| prose | 10 | 245 ms | 774 ms | 23.3 tok/s | 22.4–24.9 tok/s | 400-token ceiling in every sample |
| reasoning + required tool | 10 | 258 ms | 511 ms | 38.2 tok/s | 32.1–48.9 tok/s | ten valid tool-call streams |

The code median is below the roadmap's 35 tok/s candidate target. Native
SGLang metrics after the run reported speculative steps 3, draft tokens 4,
mean accept length 3.35, and accept rate 0.7833. Decode CUDA graphs handled
4,106 recorded passes. This is a valid optimized MTP run, not an accidental
non-speculative fallback.

After the bounded matrix, Linux reported 23,511,883,776 available bytes, 602
MiB swap used, and zero current memory PSI. The container had zero restarts
and no OOM. A paired vLLM run is required before attributing a relative gain.

## Step 12 preliminary paired attempt

Both engines used the exact immutable checkpoint and authenticated sy Responses
route, temperature zero, identical versioned prompts, one warmup, and ten fresh
measured samples per short workload. Code and prose deliberately used the
400-token ceiling; reasoning stayed enabled and every reasoning/tool sample
completed with a native valid tool stream. The deterministic code workload is
a source continuation, not an instruction-only synthetic generation.

The paired short plan SHA-256 was
`59e62d66f0c637ef8da17469d7f410eb20f6dcc3b2e9db20a112306c4c4dfef3`;
its sampling SHA-256 remained
`ccc73314d2af1ee6e2fedc553f175a7208274c2ec9983ef78add8d4fd6a7eac0`.

The first exact vLLM admission was red only because its declarative compile
cache envelope claimed 12 GiB. All retained Qwen vLLM namespaces were measured
between 98 KiB and 253,788,160 allocated bytes; the exact namespace used
244,670,464 bytes. The vLLM profile now declares 1 GiB, more than four times
the observed maximum. A fresh signed release passed the unchanged 100 GiB disk
reserve with 110,724,050,944 projected bytes available. No cache was removed,
no reserve was lowered, and no host software was changed.

The fresh vLLM control used engine/profile fingerprint
`sha256:83ad55bab9c40b44e3d1f2250124caa60bc7a9fe6c1337f2b48d6ebdf0df48c3`
and image `sha256:ae03e2a6feecd27520d2598f28dde37c0f7c85c59631d8c488b5803331a6753d`.
Its 631,917 ms startup reused the retained compile cache and ended healthy at
generation 2 with zero restart failures.

| vLLM workload | Decode min / median / max | TTFT min / median / p95 / max | Prompt-rate min / median / max |
| --- | ---: | ---: | ---: |
| code | 29.577 / 29.643 / 30.009 tok/s | 329.962 / 346.178 / 485.686 / 485.686 ms | 409.729 / 574.852 / 603.100 tok/s |
| prose | 26.128 / 26.199 / 26.289 tok/s | 266.919 / 270.676 / 276.133 / 276.133 ms | 340.415 / 347.284 / 352.167 tok/s |
| reasoning/tool | 31.067 / 31.742 / 32.790 tok/s | 437.837 / 542.953 / 649.165 / 649.165 ms | 531.452 / 635.809 / 787.965 tok/s |

Native vLLM MTP used two speculative tokens. Across the bounded request log
window it accepted 5,325 of 8,240 drafted tokens (64.62%); the reported mean
acceptance-length windows had median 2.27 and range 2.01--2.59. Every measured
sample emitted reasoning events, and all ten required-tool samples emitted a
valid tool item. Post-run `MemAvailable` was 16,537,165,824 bytes, full-memory
PSI was zero, swap-in delta was zero, disk availability was 111,673,098,240
bytes, and the engine remained healthy without a restart or OOM.

The disjoint cold plan SHA-256 was
`bd6f2d92734780ac7e83d9388757f69dc19eb166adaa3a456801a0b83cbe25a7`.
Calibration established exactly 15 tokens per fixture unit; the final requests
were 8,201, 32,771, and 131,081 input tokens and shared no record prefix.

| vLLM cold input | TTFT | Client prompt-rate estimate |
| --- | ---: | ---: |
| 8,201 tokens | 4,758.840 ms | 1,723.319 tok/s |
| 32,771 tokens | 13,665.400 ms | 2,398.100 tok/s |
| 131,081 tokens | 58,368.449 ms | 2,245.751 tok/s |

## Step 12 final fresh paired rerun

The final short plan was `bdb861db78613015330d5e0d29abcf9bc82c64e0b504363f7f906a9ad0db9afd`;
sampling was `ccc73314d2af1ee6e2fedc553f175a7208274c2ec9983ef78add8d4fd6a7eac0`.
Both engines used the same immutable checkpoint, gateway, prompts, temperature
zero, 400-token ceiling, one warmup, and ten measured samples. Only code used
request-local `reasoning.effort=none`; prose and reasoning/tool kept reasoning
enabled, so this is not an engine-wide reasoning workaround.

Redacted raw results are under `specs/runs/qwen38-step12/`; they retain hashes,
timings, counts, event identities, scalar observations, and no generated text.
The vLLM fingerprint was
`sha256:83ad55bab9c40b44e3d1f2250124caa60bc7a9fe6c1337f2b48d6ebdf0df48c3`;
SGLang MTP was
`sha256:5445a90f77e99e4a4df0c114641b114f366e594970e9e50a348b70d4a6861305`.
Both report model fingerprint
`sha256:26d708967d17a7b261018701fffcce2f40f2efa98476905e1a7e365db4872de6`.

| Engine / workload | Decode min / median / max | TTFT min / median / p95 / max | Prompt min / median / max |
| --- | ---: | ---: | ---: |
| vLLM code | 35.416 / 35.485 / 35.731 tok/s | 339.316 / 354.045 / 467.313 / 467.313 ms | 348.803 / 460.394 / 480.379 tok/s |
| SGLang code | 43.361 / 43.980 / 44.967 tok/s | 227.032 / 232.594 / 486.928 / 486.928 ms | 334.752 / 700.798 / 717.961 tok/s |
| vLLM prose | 26.218 / 26.325 / 26.397 tok/s | 265.275 / 271.439 / 282.135 / 282.135 ms | 333.174 / 346.304 / 354.349 tok/s |
| SGLang prose | 23.730 / 24.686 / 25.675 tok/s | 233.289 / 241.515 / 250.637 / 250.637 ms | 375.045 / 389.219 / 402.933 tok/s |
| vLLM reasoning/tool | 30.723 / 32.041 / 32.731 tok/s | 485.580 / 578.509 / 635.944 / 635.944 ms | 542.500 / 596.438 / 710.491 tok/s |
| SGLang reasoning/tool | 30.646 / 39.381 / 42.706 tok/s | 243.660 / 249.397 / 255.021 / 255.021 ms | 1,392.044 / 1,423.449 / 1,456.947 tok/s |

SGLang code cleared 35 tok/s but was only 23.94% above vLLM, below the
required 30%. Prose was 6.23% below vLLM and reasoning/tool was 22.91% above,
so both preservation checks passed; every reasoning/tool sample was a valid
tool stream. vLLM native MTP windows reported mean acceptance length
2.86--2.91 and 92.8--95.5% for code, 2.02--2.23 and 50.8--61.3% for prose,
and 2.52--2.60 and 76.1--80.2% for reasoning/tool.

The signed no-MTP diagnostic fingerprint was
`sha256:5d333964635b7173868bb05358b205396c61897c3d17f54dac4427867e994490`.
With identical short requests, code decode was 17.733 / 17.871 / 17.993 tok/s,
prose was 17.163 / 17.387 / 17.576, and reasoning/tool was
17.155 / 17.787 / 17.947 with ten valid tools. Their p95 TTFTs were 922.901,
719.362, and 240.756 ms; median prompt estimates were 839.168, 459.168, and
1,728.941 tok/s. This A/B confirms useful MTP acceleration without disabling
reasoning or corrupting tool output; no-MTP acceptance is exactly zero.

| Cold input | vLLM TTFT / prompt | SGLang TTFT / prompt | SGLang / vLLM TTFT |
| --- | ---: | ---: | ---: |
| 8,201 | 4,411.454 ms / 1,859.024 tok/s | 4,810.166 ms / 1,704.931 tok/s | 1.090, pass |
| 32,771 | 13,816.892 ms / 2,371.807 tok/s | 17,507.583 ms / 1,871.817 tok/s | 1.267, fail |
| 131,081 | 58,377.067 ms / 2,245.419 tok/s | 93,231.382 ms / 1,405.975 tok/s | 1.597, fail |

The 0.81 candidate exposed 145,280 scheduler tokens, enough for the 131,081
request, and was admitted with 8,892,403,712 bytes projected headroom. Ready
`MemAvailable` was 17,200,345,088 bytes. After 128K it fell to
6,642,241,536 bytes; event `01M178J9TB8YAYG6E2GBS69BFA` correctly killed
generation 26 for the unchanged 8 GiB memory floor. vLLM remained healthy
after cold prefill at 14,793,154,560 bytes available. No-MTP remained healthy
after its short matrix at 12,795,019,264 bytes available.

The concurrency-one profile already used `max-running-requests=1`, CUDA graph
batch shape one, and the five recurrent-state slots required by one MTP
request. Graphs consumed about 0.58 GiB, less than the observed 1.95 GiB floor
breach; reducing the 1,024-token prefill chunk would further worsen TTFT.
The safe 0.79 profile exposes only 84,864 tokens, while 0.81 is unsafe at
128K. Therefore no configuration-only correction preserves MTP, graphs,
131,081-token capacity, and the memory floor; source stays at safe 0.79.

The final stopped SGLang container did not retain a request-window native
acceptance aggregate, so the roadmap's per-result MTP-acceptance completeness
bullet remains open rather than borrowing g22 counters. The profile identity,
native loader logs, and complete no-MTP A/B prove MTP was active, but are not
relabeled as that missing aggregate. The candidate is rejected: code relative
gain, 32K/128K TTFT, memory safety, and result completeness did not all pass.

Managed stop left no running instances. Signed restoration returned exact
release `0.1.0-d622b71c06eac1c3666cea44545d9fda5f27ada1f21f8d27467edc486bcfc36a`
with protected fingerprint `7e42b88250e762400e91b902cfa1fcda6b4d1cc118eb6b91fd50716b41cf8510`.
Final status was idle with agent/executor heartbeats green, 124,289,765,376
bytes available memory, zero full-memory PSI and swap-in delta, and
110,991,663,104 bytes available disk.

## Bounded cleanup and recoverability

Two rejected build variants were removed earlier to preserve the disk reserve:
tag `step9-replayssm-rebuild` at
`sha256:85fd7db997987e93f59412efbef046aad1645aa364f36912f3154beb065a6116`
and tag `step9-pagecache-mamba9` at
`sha256:33dbe939feefa1e6039e5f54549eecf769c9a4504b15d44c3ea49c9f11009269`.
They are recoverable from their content-addressed Docker inputs. Neither was
referenced by an active signed release, model snapshot, PLE artifact, or final
build evidence. The final b09 image and reproducibility tag remain present.

Lifecycle cleanup removed only managed or quarantined candidate containers:
the gen18 quarantine container with unique prefix `61d14eef`, cold g19
`47e105f1dde17054c599aca3d7d01de3d404846f814205f1f0c14da463adc750`,
warm g20, and cancelled g21. All are reproducible from the exact signed image,
model snapshot, engine configuration, and retained compile cache. No model,
PLE, release, or evidence file was removed with them.

The external compile-identity reproduction defect had created the exact stale
namespace
`sha256-dc0aedc786f49315fd686f020dcc1eeba867542b5abe5de032ea69d1cd3f6c7d/i_2f77a431248152edcc81ed690177e3b4`.
After signed restoration and zero-container verification, the bounded command
was:

```console
sudo rm -r -- /var/lib/sy-spark/compile-cache/sha256-dc0aedc786f49315fd686f020dcc1eeba867542b5abe5de032ea69d1cd3f6c7d/i_2f77a431248152edcc81ed690177e3b4
sudo rmdir -- /var/lib/sy-spark/compile-cache/sha256-dc0aedc786f49315fd686f020dcc1eeba867542b5abe5de032ea69d1cd3f6c7d
```

The inventory contained 986 entries and 51,500,199,936 logical bytes, mostly
one recoverable hard link to the shared PLE table; actual disk recovery was
299,950,080 bytes. Post-removal verification proved the stale namespace absent
and the authoritative namespace present. The shared inode `8936718` link count
decremented exactly once from 11 to 10; its source and authoritative links
remain 0440, UID/GID 65534, 51,200,245,760 bytes, and share completion-marker
SHA-256 `8cc50651287a706ffb53fc51efe6fd5dbf359d731847f096f5e2c5cfa53f7002`.
No active release, model snapshot, authoritative PLE, current evidence, or
protected host component was deleted. The removed JIT namespace is fully
recoverable through normal content-addressed compilation.

## Reproduced defects and corrections

- Compile-cache identity originally included scheduling priority. It now uses
  only execution-affecting engine identity, so a temporary qualification
  priority does not strand a large derived cache.
- Executable JIT caches originally landed on a no-exec mount. Their declared
  environment is now mounted executable by generic engine metadata.
- Gateway validation originally rejected opaque upstream completion IDs and
  nullable intermediate stream usage. It now accepts the upstream wire shape
  while still requiring usage on the terminal frame.
- Stopping during engine readiness originally left a transition lease until
  the startup deadline. Readiness now observes durable cancellation and
  stopped intent.
- A disconnected SGLang stream reproduced upstream issue 36333: tokenizer
  state disappeared while the scheduler continued decoding. The candidate
  image applies the lifecycle correction from upstream pull request 36418 as
  a content-addressed build-time patch. Post-fix acceptance disconnected after
  2,750 streamed bytes, then completed a reasoning-enabled follow-up in 1.08
  seconds after the abort window. A normal completed stream produced its
  terminal event sequence without entering the forced-abort path. The engine
  remained healthy with zero restarts and no deleted-state log flood.

## Selection status

The durable source catalog retains vLLM as the higher-priority control while
the SGLang candidate is tested through a temporary signed release. Engine
selection remains generic: artifact traits, declared capabilities, priority,
and resource admission choose an engine; Rust contains no Qwen or SGLang
dispatch branch.
