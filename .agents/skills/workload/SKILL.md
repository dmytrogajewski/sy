---
name: workload
description: Scaffold a new aiplane Workload impl end-to-end
allowed-tools: Edit Write Read Grep Glob Bash(cargo *) Bash(make *) Bash(sy *) Bash(ls *) Bash(cat *) Bash(rg *) Bash(grep *)
---

# Add a new aiplane Workload

<constraints>
Follow AGENTS.md non-negotiables. Run `make lint && make test` before
declaring the workload done. The Workload impl ships with: a FakeWorkload-
backed unit test (no NPU), a real-NPU test gated behind `#[cfg(feature =
"test-npu")]`, an MCP tool surface entry, and CLI wiring.
</constraints>

<role>
sy-aiplane workload author. You take a new NPU workload from "we want
to add VAD" to "`sy aiplane run --workload vad` works end-to-end on the
host, tests green on CI without a real NPU, MCP exposes it as a tool".
</role>

A new workload is **eight artefacts**, not one Rust file. Forget any of
them and the daemon, CLI, MCP server, or tests will be silently broken.

---

## The 8-artefact checklist

For workload `<kind>` (lowercase, snake-case):

1. **Enum variant**: add `<Kind>` to `aiplane::registry::WorkloadKind`
   in `src/aiplane/registry.rs`. Update the `Display` impl + the
   `FromStr` parser (used by the CLI `--workload` flag).

2. **Input/output variants** (if novel): add to `WorkloadInput` /
   `WorkloadOutput` in `src/aiplane/registry.rs`. Reuse existing
   variants where possible (most rerankers use `TextPair`; VAD/STT
   use `Audio`).

3. **Workload impl**: create `src/aiplane/workloads/<kind>.rs`
   implementing the `Workload` trait. Reference shape — see
   `aiplane/workloads/embed.rs` as the canonical example.

4. **Registry boot**: add a line to
   `src/aiplane/workloads/mod.rs::register_all()` so the daemon
   knows about the new kind on startup.

5. **CLI dispatch**: extend `sy aiplane run --workload <kind>` in
   `src/aiplane/cli.rs` (the JSON input shape is parsed from
   `--in` / `--in-file` based on the workload's expected
   `WorkloadInput` variant).

6. **MCP tool surface**: add `tool_<kind>` to
   `src/knowledge/mcp.rs::tools()`, with a JSON schema and an
   ipc::request() call to `Req::Run { workload: <Kind>, input: ... }`.

7. **Prep script entry**: add a `--workload <kind>` arm to
   `scripts/prep_npu_workload.py` with the model id, shape,
   tokenizer, and quant preset. See `/npu-prep` skill.

8. **Tests**:
   - `src/aiplane/workloads/<kind>.rs::tests` — unit test using the
     workload's pure logic (input encoding, output decoding) with no
     ORT session.
   - `tests/workload_<kind>.rs` — integration test, gated `cfg(feature
     = "test-npu")`, that runs end-to-end against the prepared model
     cache. Skips with a clear message if the cache is absent.
   - If a `FakeWorkload`-style stand-in makes sense (deterministic
     CPU output): a daemon-in-thread test in `tests/daemon_smoke.rs`
     verifying `Req::Run { <Kind>, ... }` round-trips through the IPC.

---

## Phase 1: Spec the workload

Before any code, write a tiny in-line spec answering:

- **Model id + revision**: e.g. `snakers4/silero-vad@main`.
- **Input shape**: e.g. `(1, 1536)` i16 PCM at 16 kHz.
- **Output shape**: e.g. `(1, 1)` speech probability per 96 ms frame.
- **Tokenizer / preproc**: none (raw audio) | XLM-RoBERTa BPE | spec
  preproc (mel filterbank, image resize, etc.).
- **EP preference**: `Vitisai` (default) or `Cpu` (tiny models that
  don't benefit from NPU).
- **Latency budget**: e.g. "p99 ≤ 50 ms on Strix Point steady state".
- **MCP tool name**: `aiplane_<kind>`.

If any answer is "I don't know yet" → stop, research, fill in.

---

## Phase 2: Land it bottom-up

Order matters. Each step ships its own test.

1. **Add the enum variants** (`WorkloadKind`, optionally
   `WorkloadInput`/`Output`). Unit test: enum roundtrip via JSON
   ser/de.
2. **Implement the `Workload` trait** with `load()` returning `Err`
   for now (no real ONNX) and `run()` returning a deterministic stub.
   Unit test: trait methods invoke correctly through the registry.
3. **Wire CLI dispatch**: `sy aiplane run --workload <kind>` now
   produces the deterministic stub. Manual smoke: shell test exits 0
   with the expected JSON.
4. **Wire MCP tool**: `mcp__sy-aiplane__aiplane_<kind>` returns the
   stub. Test via daemon-in-thread.
5. **Add the prep script arm** + run it on the host to produce the
   model artifact + compile cache. (Use the `/npu-prep` skill.)
6. **Replace the stub `run()` body** with the real ORT session +
   input encoding + output decoding. Behind `cfg(feature =
   "test-npu")`: add an integration test asserting the real model
   produces sane output (e.g. VAD on a known speech WAV returns
   probability > 0.5 in the right time windows).
7. **Bench**: optional but recommended — `benches/<kind>.rs` for
   throughput regressions on this workload.

---

## Phase 3: Acceptance

The workload is done when:

- [ ] `sy aiplane run --workload <kind> --in <json>` works on the
      host with a warm cache.
- [ ] `sy aiplane status --json` shows the workload as registered
      (loaded? loaded once invoked).
- [ ] `sy aiplane bench --workload <kind> --n 32` reports a sensible
      throughput.
- [ ] `make test` green (FakeWorkload-backed test passes without
      NPU).
- [ ] `make test-npu` green (real-NPU test passes against the cache).
- [ ] `make lint` green.
- [ ] MCP tool surface: `mcp__sy-aiplane__aiplane_<kind>` resolves
      from Claude Code after `/mcp reconnect sy-aiplane`.
- [ ] Workload docstring filled in per `/npu-prep` Phase 5.

---

<rules>
1. **Don't skip artefacts.** All 8 land in the same PR. Half-wired
   workloads are worse than no workload.
2. **Stub then fill.** Trait conformance + tests first; real ONNX
   session second. The stub phase keeps the build green so reviewers
   can see the surface separate from the inference.
3. **Reuse `WorkloadInput`/`Output` variants.** Don't invent a new
   variant for "text but a little different" — re-use `Text` or
   `TextPair` with documented semantics.
4. **EP preference is per-workload.** Tiny models (silero-vad ~2 MB)
   run faster on CPU than NPU; declare `Cpu` and don't pay the NPU
   mutex cost.
5. **The `FakeWorkload` is sacred.** It enables CI tests of the entire
   daemon plumbing without `/dev/accel/accel0`. Don't break it when
   refactoring the trait.
</rules>
