---
name: qa-audit
description: Audit existing tests — verify they catch real bugs, not just compile
---

# QA Audit

<constraints>
Read-only by default. You may add new tests or strengthen weak
assertions, but do not change production code in this skill — bug
fixes go through `/bug`, feature work through `/implement`.
</constraints>

<role>
Skeptical QA engineer. Tests that always pass are decoration. Your job
is to find tests whose assertions are too weak to catch the bugs they
purport to prevent, and to fill the obvious coverage holes.
</role>

---

## Phase 1: Inventory

1. Run `cargo test --no-run --all-targets 2>&1 | rg '^test '` to
   enumerate test names.
2. `find src tests -name '*.rs' | xargs grep -lE '#\[(test|cfg\(test\))\]'`
   for the modules with tests.
3. List modules in `src/` that have **zero** tests. Especially flag:
   - `src/aiplane/ipc.rs` (wire format!)
   - `src/aiplane/registry.rs` (workload dispatch correctness)
   - `src/aiplane/session.rs` (NPU mutex semantics)
   - `src/knowledge/chunk.rs` (sliding-window correctness)
   - `src/knowledge/manifest.rs` (qdr.toml parsing)

---

## Phase 2: Weakness scan

For each existing test, check for these anti-patterns:

- **`.is_ok()`-only assertions**: the function returned without
  panicking but you didn't check the *value*. Strengthen to assert
  the actual content.
- **Single-input tests**: only one input exercised; no boundary
  cases (empty, len=1, len=MAX, whitespace, unicode).
- **Round-trip without invariants**: `serialise → deserialise → eq`
  is fine, but if there's a documented invariant (vector norm = 1,
  chunk overlap = 64), assert it explicitly.
- **Magic-number expectations**: hardcoded expected outputs that
  silently track an implementation change. Where possible, derive
  the expected value from a named constant.
- **No negative-path test**: every `Result`-returning function should
  have at least one test for the `Err` path.
- **Mocked-too-deep**: a "unit test" that mocks the IPC socket, the
  qdrant HTTP client, and the embedder produces nothing useful. If
  the test mocks more than 2 boundaries, consider an integration
  test with the fake workload instead.

Document each weakness as `src/path.rs::test_name — <reason>`.

---

## Phase 3: Coverage gaps

For each module identified in Phase 1 with zero tests, write down the
**minimum useful test** that would catch a real bug:

- `ipc.rs`: `Op` JSON ser/de roundtrip with every variant;
  `Req::Run`/`Resp::Run` roundtrip; malformed-bytes parse failure.
- `registry.rs`: register a `FakeWorkload`, call `run` with each
  `WorkloadInput` variant, assert dispatch picked the right impl.
- `session.rs`: two threads both holding `run_on_npu` — assert
  serialisation (one waits for the other) via a timing fence.
- `chunk.rs`: empty input → empty output; len < window → one chunk;
  overlap matches the documented constant.
- `manifest.rs`: a fixture `qdr.toml` parses; a malformed one yields
  the expected error.

---

## Phase 4: Land the additions

For each gap and each weakness:

- New test: write it via the `/implement` skill's Small-Change Fast
  Path. One test per commit (logical unit, not chronological).
- Strengthened assertion: same.

After landing:

- `make test` green.
- `make lint` green.
- Coverage measurably improved: count of tests, count of
  assertions per test, count of modules with at least one test.

---

<rules>
1. **Don't change production code in this skill.** Tests only.
2. **Don't add a test you wouldn't want to run on CI.** No 30-second
   tests; gate slow ones behind feature flags.
3. **Don't mock what you can fake.** A `FakeWorkload` is preferable
   to a `mock_embed_returns(...)` for daemon-level tests.
4. **Document what each new test prevents.** A 1-line comment above
   the `#[test]` saying what regression it catches.
5. **Property tests are bonus, not substitute.** Add example tests
   first.
</rules>
