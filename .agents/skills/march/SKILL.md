---
name: march
description: Roadmap-driven orchestrator — walks a ROADMAP.md top-to-bottom and ships each unchecked step by delegating to /implement in a fresh `implementer` subagent, with idempotent resumption and an audit-trail run log
---

# Agent Instructions: `/march` — Roadmap Orchestrator

<constraints>
Do not run git commands. All version control is handled by the user.
Follow the persona and non-negotiables defined in AGENTS.md.
Run `make lint` and `make test` after every step; never tick a checkbox
without on-disk evidence of green gates.
This skill suppresses clarifying questions during normal operation.
Continue with a documented assumption logged to the run log. Stop only
on the hard-stop conditions below.
Never push code, create tags, or perform any destructive action. Those
are user-driven.
Never write implementation code directly — only `/implement`, via the
`implementer` subagent, does that.
</constraints>

<role>
You are a delivery foreman: you do not lay the bricks, but you read
the blueprint, hand each section to the right specialist, verify what
came back, and keep the log honest. You walk the roadmap top to
bottom, one step at a time, until done or blocked.
</role>

You are an orchestrator. The roadmap is the source of truth for WHAT;
`/implement` (via the `implementer` agent) is the source of truth for
HOW. Your job is sequencing, verification, audit, and a clean resume
on interrupt.

---

## When to use this skill

Use `/march` when:
- A `specs/roadmaps/<name>/ROADMAP.md` exists and the user wants its
  unchecked steps shipped end-to-end.
- An interrupted roadmap run needs to resume from the first unchecked
  step.
- A specific roadmap range needs running (`--from`, `--to`).

Do NOT use this skill for:
- Authoring a roadmap (use `/roadmap`).
- Single-step work (invoke `/implement` directly).
- Bug fixes outside a roadmap (use `/bug`).
- Open-ended research / spec drafting (use `/research`).

---

## Operating Principles

1. **Forward motion over perfection.** When a soft decision blocks a
   step, pick the most conservative option, log the assumption,
   continue.
2. **The roadmap checkbox is the idempotency key.** A ticked `- [x]`
   means the bullet is done; an unticked `- [ ]` means it needs work.
   Never tick without on-disk evidence.
3. **Subagents do the work.** The orchestrator decides the next step;
   the `implementer` subagent owns the micro-TDD code change. Do not
   inline `/implement` logic.
4. **One run log, append only.** Every action, assumption, retry, and
   skill transition appends to `specs/runs/RUN-<datetime>.md`. The
   user reads this to see exactly what happened.
5. **Hard gates, soft prompts.** `make lint` + `make test` failures
   halt the loop after one retry. Style preferences get a default and
   a log line.
6. **Self-contained subagent prompts.** Every subagent is invoked with
   a prompt that includes its own mandatory reading list and full
   context — no implicit knowledge.

---

## Invocation

```
/march [roadmap-path] [--from N] [--to M] [--parallel K] [--isolation worktree]
```

- `roadmap-path` (optional). Defaults to:
  1. If exactly one `specs/roadmaps/*/ROADMAP.md` has unchecked DoD
     bullets, use it.
  2. Otherwise hard-stop with `error: ambiguous roadmap — pass an
     explicit path` and list all `specs/roadmaps/*/ROADMAP.md`.
- `--from N`. Skip steps numbered < N (still records them as `[s]`
  superseded in the run log if previously unchecked).
- `--to M`. Stop after step M (the next call resumes at M+1).
- `--parallel K`. Run up to K steps concurrently. **Defaults to 1.**
  When >1, `--isolation worktree` is required for safety.
- `--isolation worktree`. Spawn each subagent in a fresh git worktree
  so concurrent edits never collide. Only meaningful with
  `--parallel >1`.

---

## Discovery + Pre-flight

Before the loop:

1. **Resolve the roadmap path.** Read it. Parse steps by regex:
   - Step heading: `^## Step (\d+)\s+—\s+(.+?)$` (h2, em-dash
     separator — matches the canonical sy format under
     `specs/roadmaps/<name>/ROADMAP.md`).
   - Inside the step, a DoD block opens at `**Definition of Done:**`
     and contains bullets `^- \[ \]` (open) or `^- \[x\]` (closed).
     A step is **complete** when every DoD bullet is `[x]`. If any
     DoD bullet is `[ ]`, the step is **unchecked**.
   - Files-touched hint: `**Files:**` block lists the files the step
     is expected to modify.
   - If a step has no DoD block at all, log a warning to the run log:
     `step N: malformed (no DoD block) — skipped`. Track in the final
     summary as `skipped`.
2. **Choose a run log.** If `specs/runs/RUN-*.md` exists and its last
   `Status` is not `complete`/`blocked`, append to it as a
   resumption. Otherwise create `specs/runs/RUN-<datetime>.md`
   (creating the `specs/runs/` directory if it doesn't exist) with
   the standard header below.
3. **Pre-flight gate.** Run `make lint` and `make test` once. They
   MUST pass cleanly before the first step — if not, the workspace is
   already broken and `/march` hard-stops with cause
   `pre-flight gate red`.
4. **Record baselines.** Note the current `make test` total count.
   This becomes the delta against which subagent reports are
   validated (no regressions allowed).

If discovery or pre-flight fails: write `BLOCKED` to the run log and
return the compact final summary. Do not enter the loop.

---

## The Loop

For each unchecked step in order (subject to `--from`/`--to`):

### 1. Plan
- Read the step's **Goal**, **Files:**, **Tests:**, **Definition of
  Done:**, and any **Risks / unknowns**.
- If the step's Goal text says it depends on prior steps that are not
  all `[x]`, hard-stop with cause `dependency not satisfied for
  step N`. Do not skip.
- Append to run log: `[N] start at <ts>`.

### 2. Delegate
- Spawn ONE subagent using the canonical prompt template (see
  §Subagent Prompt Template below).
- The subagent's `subagent_type` is `implementer` (sy's purpose-built
  agent at `.claude/agents/implementer.md` — owns the
  failing-test → minimal-code → lint → green loop).
- Wait for the subagent's final message. Do not interleave other
  work for this step.

### 3. Verify (mandatory — never skip)
The subagent's claim of success is necessary but not sufficient.
Verify against disk:
- `make lint` MUST exit 0.
- `make test` MUST exit 0 AND the total test count MUST be ≥ the
  pre-step baseline (regressions are forbidden).
- If the step's `**Files:**` block names specific files, at least one
  of them MUST have been modified or created (otherwise the step
  produced no observable change).
- All bullets in the step's DoD MUST be representable as `[x]` — if
  the subagent left some unchecked, complete the tick yourself only
  if the evidence is on disk; otherwise the step is NOT done.
- The `post-edit-check.sh` and `stop-verify.sh` hooks must not have
  surfaced any violation (TODO/FIXME/`unimplemented!`/`todo!`/
  `#[allow(dead_code)]` outside `#[cfg(test)]`). If a hook fired,
  treat it as a verification failure.

If any check fails → §4 Retry. If all checks pass → §5 Commit.

### 4. Retry (at most once per step)
- Append to run log: `[N] retry — cause: <one-line>`.
- Build a state-aware preamble: include (a) the partial state the
  previous subagent left on disk, (b) the exact failure message,
  (c) an instruction to inspect rather than rewrite.
- Spawn one more `implementer` subagent with the canonical prompt
  plus the preamble.
- If the second attempt also fails verification → §6 Hard-Stop with
  cause `repeated red gate on step N`.

### 5. Commit
- Mark every DoD bullet `[x]` in the ROADMAP.
- Append to run log: `[N] done <ts> — tests N→M (+Δ)`.

### 6. Hard-Stop
Conditions:
- Pre-flight gate red.
- Step's stated dependency on a prior step that's not all `[x]`.
- Same step failed verification twice (repeated red gate).
- Subagent reported a `Spec gap` or `External dependency missing`
  blocker.
- Subagent requested or performed a destructive action (must never
  happen but defensive).
- User interrupt (Ctrl-C) detected between steps.

On hard-stop:
- Write a `## BLOCKED` section to the run log with `cause`, `last
  successful step`, `proposed next action`, exact error text.
- Emit the compact final summary and return.

### 7. Completion
When every unchecked step is now `[x]`:
- Write `## Final Run Summary` to the run log with total steps
  completed, total tests delta, total retries, total assumptions,
  status `complete`.
- Emit the compact final summary and return.

---

## Subagent Prompt Template

When delegating step N, the subagent prompt MUST include these
sections, in this order. Substitute placeholders from the parsed
roadmap step.

```
You are landing Roadmap Step <N> end-to-end. Driven by `/march`.

Mandatory reading:
1. /home/dmitriy/sources/sy/AGENTS.md
2. /home/dmitriy/sources/sy/CLAUDE.md
3. /home/dmitriy/sources/sy/.agents/skills/implement/SKILL.md
4. <absolute path to the roadmap file>
5. <absolute paths to any spec sections this step points at>
6. <any other files explicitly referenced in the step's Goal /
   Files / Tests blocks>

Scope: ONLY Step <N> — "<step heading>". Do NOT touch later steps.
Stop at Step <N>'s Definition of Done.

### Goal
<verbatim Goal from the roadmap step>

### Files (likely affected)
<verbatim **Files:** bullets>

### Tests (required)
<verbatim **Tests:** bullets>

### Definition of Done (acceptance criteria — tick each on completion)
<verbatim **Definition of Done:** bullets>

### Constraints
- Each micro-step ≤ 15 LOC of changed code (per `/implement`).
- TDD: failing test first, minimal code to green, refactor.
- No `.unwrap()` / `.expect()` in production code; tests OK.
- No `unimplemented!()` / `todo!()` / `panic!("not yet")` /
  `#[allow(dead_code)]` outside `#[cfg(test)]`. The
  `post-edit-check.sh` hook enforces this.
- No `git` commands.
- `make lint` and `make test` MUST both be clean at the end of the
  step (run `make test` twice if there's any chance of a flake).
- Update the roadmap: tick every DoD bullet you actually achieved;
  leave others unticked.

### Final report shape (mandatory)
Your final message MUST include exactly these fields:
- Files created: <list>
- Files modified: <list>
- `make lint` last 20 lines (verbatim).
- `make test` last 30 lines + total count (verbatim).
- One-paragraph summary, with any deviations explicit.
- Verification probes: any commands you ran to confirm correctness,
  with their exit codes.

### Hard-blocker protocol
If you cannot complete due to a hard blocker (toolchain missing,
ambiguous DoD, spec gap), STOP — do NOT partial-implement. Report
the blocker with the exact error message and which DoD bullets
remain open.
```

### Retry preamble (added on the second attempt only)

```
### Resumption context
A prior attempt failed verification. On-disk state:
- Files that were touched: <list>
- Files that were created: <list>
- `make lint` exit at failure: <code>
- `make test` exit at failure: <code>
- Failing test names (last 20): <list>

Inspect the on-disk state FIRST. Do not start over — read what is
there, decide what is missing or wrong, and address only that. The
original brief is below.
```

---

## Decision Defaults (replacing user clarifying questions)

When a subagent would normally prompt the user, the orchestrator's
standing decisions apply:

| Decision point | Default |
|---|---|
| Test framework | `cargo test` for unit/integration; `proptest` only after an example test (per `/implement`) |
| NPU-backed test | `FakeWorkload`-based unit test (fast, hermetic); real-NPU tests behind `#[cfg(feature = "test-npu")]` |
| New dependency | Prefer crates already in the workspace; if none fits, reject and write minimal in-house |
| Lint warning that looks pre-existing | Fix it (AGENTS.md non-negotiable: leave the area cleaner than you found it) |
| Unrelated failing test exposed during work | File a `specs/bugs/BUG-<ts>.md`, continue (do not silently fix unrelated tests) |
| Performance regression detected | Halt the loop, surface as hard-stop with cause `performance regression` |
| Roadmap step DoD ambiguous | Adopt strictest reasonable interpretation; log the assumption |
| Subagent left `#[allow(dead_code)]` outside `#[cfg(test)]` | Verification fails (AGENTS.md non-negotiable). Retry once with a preamble that names the offending site. |

Any decision not on this list and not obvious from AGENTS.md /
CLAUDE.md / the SPEC the step points at: pick the most conservative
option, log the assumption, continue.

---

## Run Log Format

Append to `specs/runs/RUN-<datetime>.md`:

```markdown
# March Run: <datetime>

## Mode
march

## Roadmap
<absolute path>

## Starting condition
<one sentence>

## Plan
<numbered list of steps the loop will run>

## Decision defaults captured
<table of relevant defaults>

## Assumptions
- A1 <assumption>
- A2 <assumption>
- …

## Timeline
- <ts> [discovery] roadmap=<path>, unchecked=<N>
- <ts> [pre-flight] make lint=ok, make test=ok (count=<N>)
- <ts> [step 1] start
- <ts> [step 1] subagent done → tests <N>→<M>, make lint ok
- <ts> [step 1] verified ok
- <ts> [step 1] done
- …

## Completed
- Step 1 — <one-line summary> (<ts>)
- …

## Blocked (if hard-stop)
- Cause: <one sentence>
- Last successful step: <ref>
- Proposed next action: <one sentence>

## Final Run Summary
- Mode: march
- Roadmap: <path>
- Steps completed: <count>/<total>
- Steps skipped: <count> (with reasons)
- Retries: <count>
- Assumptions logged: <count>
- Tests: <baseline>→<final> (+Δ)
- Status: complete | blocked: <cause>
```

---

## Output Format (per `/march` invocation)

The final message to the user is ≤10 lines:

```
Mode: march
Roadmap: <path>
Run log: specs/runs/RUN-<datetime>.md
Completed: <N>/<total>
Skipped: <count>
Retries: <count>
Tests: <baseline>→<final>
Status: <complete | blocked: <cause>>
Next: <one sentence>
```

Anything longer goes in the run log.

---

## Cadence Rules

- **Every roadmap step:** one `implementer` subagent, one verified
  `make lint` + `make test` pass, one run-log entry.
- **Every hard-stop:** a `BLOCKED` section plus the compact final
  summary.

Do not bundle multiple roadmap steps into one subagent. Do not skip
lint gates to "make progress."

---

<self_check>

Before reporting `complete`:
- Every previously-unchecked DoD bullet is now `[x]` with on-disk
  evidence?
- `make lint` and `make test` are clean at the workspace level (not
  just the last step's scope)?
- Every assumption is in the run log?
- Every retry is in the run log with a reason?
- The run log's final `Status` is `complete`?

Before reporting `blocked`:
- The `BLOCKED` section names the cause, the last successful step,
  and a proposed next action?
- The proposed next action is concrete enough that a user can act on
  it without re-deriving context?
- The run log is up-to-date through the blocked step?

</self_check>

<rules>

1. **Do not write code directly.** Only `/implement` (via the
   `implementer` subagent) writes code.
2. **Tick checkboxes only with evidence.** No subagent self-claim is
   sufficient.
3. **One subagent per step.** No bundling, no fan-out within one
   step.
4. **One retry per step.** Second failure is a hard-stop.
5. **One run log per invocation chain.** Append, never rewrite
   earlier sections.
6. **Self-contained subagent prompts.** Subagents see only what you
   pass them.
7. **No destructive actions.** No pushes, no force-anything, no tag
   creation, no commits.
8. **Honor user interrupts cleanly.** Let the in-flight subagent
   finish; stop at the next step boundary.
9. **The run log is the contract.** If it's not in the log, it didn't
   happen.

</rules>
