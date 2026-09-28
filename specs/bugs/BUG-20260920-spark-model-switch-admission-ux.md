# Spark model switching hides reservations and actionable admission failures

## Status

Reproduced on DGX Spark on 2026-09-20. Deployment recovery uses existing
commands; the CLI behavior described below still needs an implementation fix.

## User expectation

Use `sy spark` to start, resume, or switch models without inspecting Docker,
the database, or SSH services. Developing a separate engine/profile in
`~/sources/sparky` must not add steps to this normal workflow.

## Reproduction and evidence

The verified alias `qwen3.8:flash-next` selects
`vllm-qwen38-mmap-arm64` with context 262144 and a declared startup peak of
114000000000 bytes. Its model revision is
`7b719225242aacd3dbd3f9407468c2ee9a9d2594`.

1. `sy spark dgx-spark ps --json` returned an empty instance list.
2. `show qwen3.8:flash-next --json` still listed
   `qwen38-vllm-35-control` in `active_instances`.
3. `stop qwen38-vllm-35-control --dry-run --json` exposed desired `running`,
   observed `absent`, five restart failures, and `restart_suppressed=true`.
4. Starting a differently named instance failed admission with aggregate
   cold-start bytes 228000000000 against capacity 119838044160.
5. Explicitly stopping the old instance succeeded in operation
   `01M2ZJGV32PH67S9XQSEY5969Z`. Weights and caches were retained.
6. Aggregate demand became 114000000000 bytes. Admission still rejected
   the launch: available-after-start was 8545684480 bytes against the
   8589934592-byte system reserve, a deficit of 44250112 bytes.
7. After the user authorized closing GNOME Settings, admission passed with
   available-after-start 8679304192 bytes. Serve operation
   `01M2ZJTE79HMKT1MTATVGNEDJ3` was accepted for `qwen38-vllm`.

The transcript also included client network sandbox failures reported as
generic unreachable/TLS errors. Authorized network access resolved those;
certificate rotation or service restarts were unnecessary.

## Code paths

- `src/spark/cli.rs`: process-list rendering removes instances observed as
  absent or failed, including entries still relevant to admission.
- `src/spark/state.rs`: `desired_resource_envelopes` includes all entries
  with desired state `running`, including suppressed entries.
- `src/spark/resources.rs`: `evaluate_admission` uses the same memory problem
  code for aggregate-capacity and live-memory failures. Numeric fields exist
  in JSON, but the CLI's error does not identify the specific deficit.
- Admission identifies replacement by instance name. A different name for
  the same model represents additional demand, not an automatic replacement.

Counting desired workloads protects restart capacity; simply ignoring absent
instances would break that invariant. The defect is an incomplete recovery
and switching workflow, including inconsistent visibility and diagnostics.
There is no evidence here that Sparky qualification jobs caused the failure.

## Acceptance criteria for a focused fix

- Make reservations and suppressed instances discoverable through `sy spark`
  with the instance name, desired/observed state, reason, and recovery command.
- Explain aggregate versus live-memory admission failures, exact deficit,
  selected profile/context, and named reservations in human and JSON output.
- Define and document an explicit model-switch operation that previews its
  affected instances and orders stop/start safely. Do not silently stop an
  unrelated workload when a plain serve call requests additional capacity.
- Support explicit recovery of an existing suppressed instance without
  requiring the user to discover an internal instance name through stop.
- Reuse verified weights and compatible compile caches where identities allow.
- Keep research profiles behind the same catalog/lifecycle interface; profile
  development must not require routine SSH repair of normal serving state.
- Cover absent/suppressed reservations, same-model recovery, low-memory
  messages, switching failure, and unrelated active workloads with regression
  tests before changing the deployed behavior.

## Deployment recovery

The operational recovery uses the existing signed engine configuration and
preserves its 262144-token context, memory reserve, and image digest. It does
not constitute a fix for the CLI issues above.

Serve operation `01M2ZJTE79HMKT1MTATVGNEDJ3` succeeded. Instance
`qwen38-vllm` generation 1 became healthy at 2026-09-20T14:29:09.535Z,
with startup time 742158 ms and zero restart failures. The native engine
reported 433944 KV-cache tokens and 1.66 concurrent requests at the
262144-token limit. The model configuration's `max_position_embeddings`
also equals 262144.

Authenticated non-streaming Responses returned `Spark ready` with
`status=completed`; a separate streaming request emitted
`response.created`, `response.in_progress`, text deltas, and
`response.completed`. Final status reported no degraded reasons and
16014319616 bytes of available memory.
