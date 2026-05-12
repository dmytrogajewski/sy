---
name: journey
description: Capture a user journey for a sy feature before writing code
---

# Journey Authoring

<constraints>
Do not write code in this skill. The output is a journey document at
`specs/journeys/JOURNEY-<dt>.md`. Implementation comes after `/roadmap`
breaks the journey into steps and `/implement` lands them.
</constraints>

<role>
You are a product-minded systems engineer. You translate a user request
into the minimum viable journey a real user would walk, capturing the
"happy path" and the obvious failure modes that have to work.
</role>

A "journey" is a single user-visible workflow on sy — e.g. "add a new
aiplane workload", "search across a freshly indexed corpus", "recover
from a daemon crash". One journey per document. Multiple journeys =
multiple documents.

---

## Phase 1: Identify the actor and the goal

In one sentence each:

- **Actor**: Who is doing this? (Power user of the rice? An agent
  invoking MCP? The daemon supervisor itself recovering from a crash?)
- **Goal**: What outcome do they want?
- **Constraint**: What's the hardest requirement (latency, isolation,
  reversibility, NPU availability)?

If any of these is unclear, ask the user. Do not invent.

---

## Phase 2: Walk the happy path

Number the steps the actor takes. Each step has:

- The action (`sy aiplane install-service`, `sy knowledge search "Q"`, etc.).
- What sy must do under the hood (which daemon op, which IPC message,
  which workload).
- What the actor sees (stdout text, waybar tile change, MCP tool result).

Keep it under 8 steps. If it's longer, you're describing more than one
journey.

---

## Phase 3: Edge cases that must work

Enumerate the failure modes the actor would hit in practice, with the
expected handling:

- **Resource exhaustion**: NPU busy, qdrant fd cap, memory pressure.
- **State drift**: daemon down, stale state file, qdrant collection at
  wrong dim, model cache absent.
- **Privilege**: missing `CAP_IPC_LOCK`, missing systemd unit, SELinux
  context wrong.
- **Concurrency**: two CLIs running the same op simultaneously.
- **Cancellation**: ^C mid-pass, daemon SIGTERM during embed.

Each entry: trigger + expected sy behaviour + how the actor learns
about it (stderr, exit code, journal log, waybar class).

---

## Phase 4: Acceptance criteria (DoDs)

The journey is "done" when:

- Each happy-path step is exercised by a test (unit, integration, or
  end-to-end) or an explicit manual verification recipe.
- Each edge case from Phase 3 is exercised or explicitly out of scope
  (with a reason).
- `cargo clippy --all-targets -- -D warnings` is green.
- Status/MCP/waybar surfaces correctly reflect the new behaviour where
  applicable.
- `README.md` and (if user-facing) the workload's own SKILL doc are
  updated.

---

## Phase 5: Write the journey doc

Path: `specs/journeys/JOURNEY-<dt>.md` where `<dt>` is
`YYYYMMDD-HHMM`.

```markdown
# JOURNEY-<dt>: <one-line title>

## Actor & Goal
- Actor: <who>
- Goal: <one sentence>
- Hardest constraint: <latency / isolation / cost / NPU avail / …>

## Happy Path
1. <action> → <under-the-hood> → <visible outcome>
2. …

## Edge Cases
- **<trigger>**: <expected sy behaviour> (surfaced via: <stderr | exit code | log | waybar>)
- …

## Acceptance Criteria
- [ ] <test or manual recipe per happy-path step>
- [ ] <test or manual recipe per edge case>
- [ ] `make lint && make test` green
- [ ] README / SKILL doc updates landed

## Out of Scope
- <thing not addressed by this journey and why>

## Open Questions
- <anything ambiguous after Phase 1; surface for the user>
```

---

<rules>
1. **One journey per document.**
2. **No code in this skill.** Output is markdown only.
3. **Be specific.** "Faster search" is not a goal; "rerank top-50 hits
   under 200 ms p99 on NPU" is.
4. **Edge cases are not optional.** Every journey enumerates at least
   3 failure modes.
5. **Cite existing code.** When a happy-path step references an
   existing function/IPC op/workload, link it by `file:line`.
</rules>
