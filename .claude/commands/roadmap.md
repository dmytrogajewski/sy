---
name: roadmap
description: Decompose a journey into ordered, testable roadmap steps
---

# Roadmap Decomposition

<constraints>
Do not write code in this skill. Output is one roadmap document under
`specs/roadmaps/<name>/ROADMAP.md`. Implementation happens via
`/implement` per step.
</constraints>

<role>
Senior engineer chunking work into reviewable steps. You optimise for:
each step lands green on its own, ships test coverage for the behaviour
it introduces, and leaves the codebase in a buildable state.
</role>

---

## Phase 1: Read the source material

1. Read the journey doc at `specs/journeys/JOURNEY-<dt>.md` (or the
   bug doc, or the user-supplied spec).
2. Read AGENTS.md non-negotiables and working_loop.
3. Skim the relevant existing code (e.g. `src/aiplane/`,
   `src/knowledge/`, the existing systemd unit) so you decompose
   against reality, not a green-field abstraction.

---

## Phase 2: Slice the work

Break the journey into **ordered, atomic steps**. Each step:

- Lands as one PR-shaped commit (or one merged PR).
- Has its own failing tests at the start, green at the end.
- Doesn't break the build mid-step (no half-refactored module).
- Has explicit Definition-of-Done bullets.

Slicing heuristics:

- **Plumbing before consumers.** If a step introduces a new IPC op,
  ship the wire format + ser/de tests first, then the daemon
  worker, then the CLI/MCP consumer.
- **Stub then fill.** A new `Workload` impl can land its trait
  conformance + `FakeWorkload`-backed test first; the real ONNX
  session + prep script second.
- **Refactor in single direction.** When lifting code from one
  module to another, do the lift first (mechanical rename),
  generalisation second.
- **Migration last.** Rename-the-systemd-unit-type steps land after
  the new code path is green so users on the old name keep working.

A step is too big if it touches > ~300 lines or > 5 files. Split it.

---

## Phase 3: Write the roadmap doc

Path: `specs/roadmaps/<name>/ROADMAP.md`.

```markdown
# ROADMAP: <name>

Source: specs/journeys/JOURNEY-<dt>.md (or BUG-<dt>.md, or user spec link)

## Overview
<2-3 sentences: what we're building, why now, what shape the
end-state has.>

## Step 1 — <imperative title>
**Goal:** <one sentence>
**Files:** `src/aiplane/foo.rs` (new), `src/aiplane/mod.rs` (modified)
**Tests:**
- `src/aiplane/foo.rs::tests::<name>` — exercises <behaviour>
**Definition of Done:**
- [ ] tests above pass
- [ ] `make lint` green
- [ ] no `#[allow(dead_code)]` introduced
- [ ] AGENTS.md / README updated if user-facing
**Risks / unknowns:** <e.g. "depends on ort 2.0-rc.12 exposing X">

## Step 2 — …

…

## Cross-cutting Definition of Done
- [ ] All step DoDs satisfied
- [ ] End-to-end journey works on a clean checkout: <command sequence>
- [ ] `sy aiplane status` reflects the new state
- [ ] MCP tool surface updated (if applicable)
- [ ] Waybar tile renders correctly (if applicable)

## Out of Scope
- <things explicitly deferred>
```

---

## Phase 4: Self-check

- Could a fresh engineer pick up Step N (without context) and land it?
  If not, expand the step description or split it.
- Is each step independently revertable? If not, reorder.
- Does the cross-cutting DoD trace back to the journey's acceptance
  criteria? If a journey AC isn't covered by any step, add a step.
- Are risks identified? Unknown unknowns are fine to flag — that's
  what "Risks" exists for.

---

<rules>
1. **Output is one ROADMAP.md.** No code, no test files in this skill.
2. **Atomic steps.** If a step has more than one DoD that depends on
   another, split it.
3. **Tests are listed in every step.** "Add tests later" is not a step.
4. **Don't invent files.** Reference real `file:line` for where each
   step touches existing code.
5. **The roadmap is a living doc.** As `/implement` lands steps, it
   ticks DoD checkboxes. The roadmap survives until the cross-cutting
   DoD is complete.
</rules>
