---
name: Feature request
about: Propose a new capability for sy — a new plane, a new workload, a new subcommand, a config-file key, or a behaviour change to an existing plane
title: 'feat: '
labels: enhancement
assignees: ''
---

<!-- Rendered from the `/documenter issue-templates` skill. Anchored on
     the project's journey-first workflow: significant features start
     with a journey doc under `specs/journeys/`, decompose into a
     roadmap under `specs/roadmaps/`, and ship via micro-TDD per
     `AGENTS.md`. This template captures the inputs that journey
     needs. -->

> **Before you file:** search existing issues, discussions, and
> [`specs/journeys/`](../../specs/journeys/) for prior art. If
> something close exists, comment there instead.

### Problem

<!-- One paragraph. What user-visible problem are you trying to solve?
     Who has it? When does it bite? Avoid jumping to a solution here. -->

### Proposed solution

<!-- The shape of the change. Name the plane / surface you'd touch and
     sketch the user-visible contract. -->

- **Affected plane / surface:** <!-- aiplane | agt | knowledge | power | stack | syauth | supervision | `sy apply` | CLI | IPC envelope | docs -->
- **User-visible change (CLI / config / IPC):** <!-- e.g. "new subcommand `sy knowledge import <path>`", "new key `[aiplane].session_pool_size` in `configs/sy/aiplane.toml`", "new IPC op `aiplane.reload_workload`". -->
- **Acceptance shape:** <!-- One end-to-end example a reader could paste to confirm the feature works. -->

```bash
<example invocation showing the feature in use>
```

### Alternatives considered

<!-- At least one. "Do nothing" is a valid alternative — say why it
     fails. If you considered a manual / snowflake workaround, name it
     and explain why the change belongs in `sy` per CLAUDE.md's
     "no snowflakes" rule. -->

1.
2.

### Link to relevant SPEC, journey, or ADR (optional)

<!-- If a journey, SPEC, or ADR already covers part of this proposal,
     link it. Examples:
     - `specs/journeys/JOURNEY-<slug>.md`
     - `specs/research/<area>/SPEC.md`
     - `docs/adr/NNNN-<slug>.md` (once the ADR directory exists)
     - A related `specs/roadmaps/<slug>/ROADMAP.md` -->

-

### CLIG + agent-friendly checklist (only if the change adds or modifies a CLI subcommand or flag)

<!-- Per CLAUDE.md, every command surface in `sy` is both human-first
     and agent-friendly. Tick each item the proposal already addresses
     or commit to addressing in the PR. -->

- [ ] Complete `--help` text with at least one example
- [ ] `--json` (or `--output json`) output with a documented schema
- [ ] Logs to stderr, primary output to stdout
- [ ] Non-interactive by default when stdin is not a TTY
- [ ] `--dry-run` for any state-changing command
- [ ] Stable, documented exit codes
- [ ] Every flag also settable via an `SY_*` env var

### Additional context

<!-- Anything else: screenshots, links to upstream issues (AMD, ONNX
     Runtime, niri), prior-art links, performance budgets. -->
