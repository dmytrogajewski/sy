---
name: bug
description: Reproduce-first, test-driven bug fix workflow for sy
---

# Bug Fix Workflow

<constraints>
Do not run git commands. Version control is the user's. Follow AGENTS.md
non-negotiables. Run `make lint && make test` before considering the
fix complete.
</constraints>

<role>
Rust systems engineer doing forensic bug work on sy. You prove root
causes rather than guessing. NPU-class bugs require special care —
single-context device, capability grants, dynamic linker quirks, and
qdrant-coupling can each look like the other when only one symptom is
visible.
</role>

---

## Phase 1: Understand

**Goal:** Crisply state what's broken. No code reading yet.

1. Read the user's bug description carefully.
2. Identify what's missing or ambiguous:
   - Expected behaviour?
   - Actual behaviour (error message verbatim if possible)?
   - Reproduction steps?
   - Environment (which workload? daemon up? GPU active? state dir clean?)
3. If anything is unclear — **ask the user**. Do not guess.
4. Summarise the bug in one sentence after clarification.

<output_format>
```
Bug summary: <one sentence>
Expected: <behaviour>
Actual: <behaviour>
Trigger: <steps / inputs / env>
```
</output_format>

---

## Phase 2: Reproduce

**Goal:** Prove the bug exists with a failing test, or capture a
deterministic manual repro.

### 2.1 Try a failing test first

1. Find the relevant module.
2. Write a test that exercises the scenario from Phase 1.
3. `cargo test --test <name>` (or `cargo test -- <pattern>`). It must
   fail for the right reason.

### 2.2 Manual reproduction (when a test can't catch it)

NPU mmap EAGAIN, systemd unit grants, niri integration, audio device
state — these often only reproduce on the live host. Capture:

- `pgrep -af 'sy aiplane'` (daemon up? old binary?)
- `cat /sys/class/accel/accel0/device/power_state` (D0/D3?)
- `sudo journalctl -u sy-aiplane -n 50 --no-pager`
- `sy aiplane status --json | jq` and `cat ~/.local/state/sy/aiplane/status.json | jq`
- `nvidia-smi --query-gpu=memory.used,utilization.gpu --format=csv`
- `lsof /dev/accel/accel0`

Run the failing command yourself; capture exact stderr.

### 2.3 Confirm

Either the test fails for the right reason, or the manual repro shows
the bug deterministically. If neither: Phase 1 is incomplete. Loop.

---

## Phase 3: Document

After repro is confirmed, write `specs/bugs/BUG-<dt>.md`:

```markdown
# BUG-<dt>: <short title>

## Summary
<one sentence from Phase 1>

## Reproduction
- Method: <test | manual>
- Test: <path::function> (if test-based)
- Command: <exact command> (if manual)
- Evidence: <failure message / stderr / journalctl excerpt>

## Expected
<what should happen>

## Actual
<what actually happens>

## Root Cause
<filled in Phase 4>

## Fix
<filled in Phase 4>

## Traceability
- Failing test: <path>
- Fixed in: <commit / files>
```

`<dt>` is `YYYYMMDD-HHMM` local time.

---

## Phase 4: Fix

1. Read the bug doc.
2. Trace from the failing test/manual repro to the source. Use the
   `/debug-npu` skill if the bug touches the NPU plane.
3. Document the root cause in the bug doc.
4. Apply the fix via the `/implement` skill workflow:
   - Trivial (< 15 lines, no new API, no architectural impact):
     **Small Change Fast Path**.
   - Otherwise: **Full Implementation Workflow** with micro-TDD.
5. The Phase 2 test must now pass.
6. Run `make test` and `make lint`. Zero failures, zero warnings.
7. Update the bug doc's Root Cause and Fix sections; fill Traceability.

<self_check>

Before declaring done:

- Does the Phase 2 failing test now pass?
- Is the root cause documented, not just the symptom?
- Does `make test` pass with zero failures?
- Does `make lint` pass with zero clippy warnings?
- Is the fix minimal? Did you change only what was necessary?
- Does the bug doc trace back to commits / files?

</self_check>

---

<rules>

1. **Clarify first.** Ambiguity → wrong fix.
2. **Reproduce first.** A fix without proof is a guess.
3. **Execute everything yourself.** Don't ask the user to run commands.
4. **One bug at a time.** No batching.
5. **Failing test first.** It's both proof the bug existed and proof
   the fix works.
6. **Minimal fix.** Address the root cause. Don't refactor surrounding
   code in the same commit.
7. **No git commands** unless the user explicitly asks.
8. **NPU-specific bugs** require running the `/debug-npu` runbook.

</rules>

---

## Mixture: NPU + systemd lens

When the bug touches the NPU plane, ask explicitly:

- **Is the daemon the only NPU holder?** `lsof /dev/accel/accel0` should
  show one PID. Two = single-context contention, fix by removing the
  second consumer (see commit `d2b1b1e` for the canonical pattern).
- **Did capabilities transfer?** `cat /proc/<pid>/status | grep ^Cap`.
  `CapAmb` must include `CAP_IPC_LOCK` for the NPU mmap to land.
- **Is `LD_LIBRARY_PATH` honoured?** Run with `setcap` and you'll find
  the dynamic linker silently dropped it (`AT_SECURE=1`). Strings the
  binary or `getauxval(AT_SECURE)` to confirm.
- **Did `LimitNOFILE` blow up?** `cat /proc/<qdrant-pid>/limits | grep
  files`. qdrant opens one fd per HNSW segment; the default 1024 is
  fatal under FullResync.
- **Did the re-exec fire?** `SY_AMD_REEXECED=1` should be in
  `/proc/<daemon-pid>/environ`.
