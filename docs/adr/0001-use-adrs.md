# 0001 — Use Architecture Decision Records

- Status: accepted

> Template: [MADR 4.0](https://adr.github.io/madr/).

## Context and Problem Statement

`sy` accumulates load-bearing architectural choices — the
single-process NPU plane, the JSON-RPC-over-UDS wire format, the
four-class scheduler, the in-process sandbox, the systemd `--user`
supervision tree, the on-device observability stack. Today those
decisions live in long-form research artefacts under `specs/` (most
notably `specs/research/architecture-refactor/SPEC.md`), in
`CLAUDE.md` / `AGENTS.md`, and in commit messages. The shapes are
inconsistent, the rationale is interleaved with implementation
detail, and "why did we pick this?" is hard to answer without
re-reading a multi-thousand-line SPEC.

A new contributor (human or agent) reads `README.md`, then
`AGENTS.md`, then jumps to code. The "why this and not that"
context never reaches them. When a decision needs revisiting,
there is no single page to supersede; you re-open the SPEC, find
the relevant `K`-row, and hope nobody else is editing the same
file. `sy` needs a documented, append-only home for architectural
decisions that is independent of the SPEC's lifecycle.

## Decision Drivers

- **Traceability**: every load-bearing choice (NPU EP, IPC format,
  scheduler shape, sandbox layering, supervision model) should be
  reachable from a single index.
- **Append-only history**: superseded decisions stay readable;
  the project's reasoning is its own audit trail.
- **Voice fidelity with `specs/`**: ADRs lift decisions out of
  `specs/research/**` without duplicating the research itself.
  The SPEC remains the long-form journal; the ADR is the
  short-form ruling.
- **Agent-friendly**: one decision per file, predictable filename
  shape (`NNNN-<slug>.md`), MADR-shaped H2 sections, so
  `/documenter`, `/march`, and downstream LLM consumers can index
  them.
- **No clocks**: the project bans date strings in generated
  docs; the ADR format must work with status-only front matter.

## Considered Options

- **Option 1: MADR 4.0** — Markdown ADRs with Status, Context
  and Problem Statement, Decision Drivers, Considered Options,
  Decision Outcome, Consequences, Pros and Cons of the Options,
  Links. Numbered `NNNN-<slug>.md` under `docs/adr/`.
- **Option 2: Nygard's original template** — the canonical short
  form: Status, Context, Decision, Consequences. Same location,
  same numbering.
- **Option 3: Status quo** — keep all decisions inside
  `specs/research/**` SPEC documents and inline rationale.
- **Option 4: RFC-style proposals** — pull-request-driven RFCs
  under `specs/rfc/`, longer-form, multi-author review.

## Decision Outcome

Chosen option: **Option 1, MADR 4.0**, because it preserves Nygard's
shape (Status / Context / Decision / Consequences) while adding
the two sections the project already writes in practice when
lifting a SPEC `K`-row: explicit *Decision Drivers* and
*Considered Options*. MADR 4.0 also tolerates the project's
"no dates" rule: Status alone is sufficient front matter.

ADRs live at `docs/adr/NNNN-<slug>.md`, numbered monotonically,
authored via `/documenter adr <slug>`. The first three are this
file, [0002 — Virtual workspace with `sy-core` vocabulary](0002-virtual-workspace-with-sy-core-vocabulary.md),
and [0003 — VitisAI EP, not CUDA, for on-device embedding](0003-vitisai-ep-not-cuda-for-on-device-embedding.md).

Going forward, any change that (a) introduces a new dependency, (b)
adds or removes a plane, (c) changes the IPC envelope, (d) changes
the supervision shape, or (e) overrides a previous ADR ships a new
ADR in the same change.

## Consequences

- **Good**: load-bearing decisions become individually
  addressable. A contributor can read one ADR rather than a
  multi-thousand-line SPEC. Supersession is a link, not a
  rewrite.
- **Good**: `/documenter adr` becomes a first-class authoring
  flow; `/march` can lift remaining SPEC `K`-rows into ADRs
  incrementally without a big-bang migration.
- **Good**: aligns with the OpenSSF Best Practices
  Badge's "documentation of design" expectation (`R-COMPLY-01`
  in `specs/docs-audit/AUDIT-full.md`).
- **Neutral**: `specs/research/**` SPECs continue to exist; they
  are the long-form research, ADRs are the rulings. The two are
  cross-linked, not merged.
- **Bad**: there is now a second place where architecture is
  written down. Contributors must remember to update the ADR
  index when they add a new decision, and to write a superseding
  ADR rather than editing an accepted one in place.

## Pros and Cons of the Options

### Option 1 — MADR 4.0

- Good: explicit Decision Drivers and Considered Options match
  how the SPEC's `K`-rows are already written (`Reasoning` +
  `Alternatives considered` columns).
- Good: tolerates date-free front matter; works with the
  documenter skill's "no clocks" rule.
- Good: widely adopted; tooling (`adr-tools`, `log4brains`,
  `dotnet-adr`) speaks MADR.
- Neutral: slightly longer per file than Nygard's original.

### Option 2 — Nygard's original

- Good: minimal — four sections, easy to dash off.
- Bad: collapses Decision Drivers into Context, which is
  exactly the conflation the SPEC's `K`-rows already split apart
  (Choice vs Reasoning vs Alternatives). MADR matches the
  project's existing voice better.

### Option 3 — Status quo (SPECs only)

- Good: no new artefact type to maintain.
- Bad: every decision is buried inside a multi-thousand-line
  research doc. Cannot supersede a single decision without
  rewriting a section of the SPEC, which makes the SPEC's git
  history hostile to bisect.
- Bad: no per-decision Status field; "is this still the
  current ruling?" is a `git log` archaeology problem.

### Option 4 — RFC-style proposals

- Good: heavier review process for the heaviest decisions.
- Bad: RFCs are a *proposal* artefact, not a *ruling* artefact;
  they answer "should we?" rather than "we did, here is the
  record." `sy` is single-maintainer, so the proposal/ruling
  split is overhead. RFCs and ADRs are complements, not
  substitutes; introducing both at once is premature.

## Links

- Template: [MADR 4.0](https://adr.github.io/madr/).
- Companion: [Nygard's original ADR template](https://github.com/joelparkerhenderson/architecture-decision-record/blob/main/locales/en/templates/decision-record-template-by-michael-nygard/index.md).
- Audit row: `specs/docs-audit/AUDIT-full.md#r-adr-01--should`.
- Roadmap item: `specs/docs-audit/PLAN-full.md` Item 16.
- Skill: `.agents/skills/documenter/SKILL.md`.
- Source SPEC kept for long-form research:
  `specs/research/architecture-refactor/SPEC.md`.
- Lifted decisions: [ADR-0002](0002-virtual-workspace-with-sy-core-vocabulary.md),
  [ADR-0003](0003-vitisai-ep-not-cuda-for-on-device-embedding.md).
