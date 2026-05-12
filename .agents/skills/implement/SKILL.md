---
name: implement
description: Micro-TDD implementation flow for a single roadmap step
---

# Micro-TDD Implementation

<constraints>
Follow AGENTS.md non-negotiables. No git commands. Run `make lint` and
`make test` before considering the step complete. No TODO/FIXME/stub
language in committed code — the post-edit-check hook enforces this.
</constraints>

<role>
Rust systems engineer landing one roadmap step at a time. You work in
ultra-small steps: one failing test, one minimal code change,
self-reflection, repeat. You optimise for green tests and zero clippy
warnings at every checkpoint, not at the end.
</role>

---

## Decide: Small Change Fast Path or Full Workflow

**Small Change Fast Path** if ALL of:
- Net diff < 15 lines (test + code)
- No new public API, no new trait, no new module
- No new dependencies
- No systemd / config file changes

→ Write the test, write the code, `make lint && make test`, done.

**Full Workflow** otherwise. Continue below.

---

## Phase 1: Re-read the spec

1. Open the roadmap step from `specs/roadmaps/<name>/ROADMAP.md`.
2. Re-read AGENTS.md non-negotiables.
3. Locate the files the step touches. Read them. Understand the
   surrounding code's contracts before you change them.

---

## Phase 2: The micro-TDD loop

Repeat until the step's DoDs are met:

### 2.1 Write ONE failing test

- Test behaviour, not internals. Test the public surface.
- One assertion per test where possible.
- Use named constants (`const SEQ_LEN: usize = 512;`), not magic
  numbers.
- For NPU-backed workloads, prefer a `FakeWorkload`-based test over a
  real ONNX session — fast, hermetic. Real-NPU tests go behind
  `#[cfg(feature = "test-npu")]`.

Run it: `cargo test -- <pattern>`. It must fail. If it passes, your
test isn't testing the new behaviour.

### 2.2 Write the minimal code to make it pass

- ≤ 15 modified lines.
- No new abstractions yet — duplicate first, refactor later.
- No `unimplemented!()` / `todo!()` / `panic!("not yet")`. Pre-edit
  hook will flag it.

Run: `cargo test -- <pattern>`. It must pass.

### 2.3 Refactor

- Extract helpers only after you've seen the duplication twice.
- Rename for clarity, not novelty.
- Run `cargo clippy --all-targets -- -D warnings`. Zero warnings.
- Re-run the test. Still green.

### 2.4 Self-reflection

Before the next iteration:

- Is the test name a sentence that says what behaviour is preserved?
  ("`embedder_returns_unit_norm_vector`" ✓; "`test1`" ✗).
- Did I add `#[allow(dead_code)]`? Delete it; delete the unused code.
- Did I leave a TODO/FIXME? Replace with the real implementation now,
  not later.
- Did my refactor change behaviour? If so, I introduced a regression
  while the tests were green — re-test.

---

## Phase 3: Acceptance

When the step's DoDs are all green:

1. `make lint` — `cargo clippy --all-targets -- -D warnings` + `cargo
   fmt --check`. Both pass.
2. `make test` — all tests pass, no flakes (run twice to catch flake).
3. Run the manual verification recipe from the journey doc if there
   is one.
4. Tick the DoD checkboxes in the ROADMAP step.
5. If user-facing behaviour changed, update README / SKILL doc.

---

<rules>

1. **One failing test before any production code.**
2. **One behaviour per loop iteration.**
3. **Public surface tests over internal tests.**
4. **Named constants over magic literals.**
5. **No `#[allow(dead_code)]` outside `#[cfg(test)]`.** Delete instead.
6. **No `git` commands** unless the user explicitly asks.
7. **Property tests after example tests, not before.**
8. **NPU-backed tests behind `#[cfg(feature = "test-npu")]`.**
9. **Leave the area cleaner than you found it** — fix lint warnings,
   dead code, and minor issues near the code you touch.

</rules>

---

## NPU-plane micro-norms

- New `Workload` impl ships with: a `FakeWorkload`-backed unit test
  (no NPU), a `#[cfg(feature = "test-npu")]` integration test (real
  NPU), and a smoke-test recipe in the workload's SKILL doc.
- New IPC `Req`/`Resp` variant: round-trip serialise/deserialise test
  in `aiplane::ipc::tests`, daemon handler test, CLI consumer test.
- Cap/limit changes to `sy-aiplane.service`: add a test that asserts
  `/proc/<daemon-pid>/limits` or `/proc/<daemon-pid>/status` has the
  expected value, gated on the integration-test feature.
