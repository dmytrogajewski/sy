# Governance

<!-- Rendered from the `/documenter governance` template (CHAOSS
     governance metric; GitHub community-profile guidance). Voice
     anchored on README.md, AGENTS.md, CONTRIBUTING.md, and
     docs/adr/0001-use-adrs.md. Single-maintainer reality, not a
     copy-paste of a multi-maintainer foundation template. -->

`sy` is a **single-maintainer project**. This page tells you who
decides what, what kind of change triggers a written decision
record, and how disputes are resolved. It is intentionally short —
the project has one maintainer, no foundation, no formal voting
body, and no aspirations to grow either.

## Roles

- **Maintainer** — the person with write access to `main`, who
  reviews and merges PRs, cuts releases, and accepts or rejects
  proposed changes. Today that is one person: **Dmitriy Gajewski**
  (`@dmytrogajewski`, <dmytrogajewski@gmail.com>). The maintainer
  is also the security contact (see [`SECURITY.md`](SECURITY.md))
  and the code-of-conduct enforcement contact
  (see [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md)).
- **Reviewer** — anyone the maintainer asks for a second pair of
  eyes on a PR. Reviewers do not have merge rights; their role is
  advisory. The [`CONTRIBUTING.md`](CONTRIBUTING.md) review
  expectations (CI green, docs updated, DCO sign-off, focused
  scope) apply equally to reviewer feedback.
- **Contributor** — anyone who has had a PR merged. Contributors
  do not have merge rights either. Sustained contribution is one
  of the criteria the maintainer would weigh if the project ever
  added a second maintainer (see [Adding maintainers](#adding-maintainers)).

There is no "core team", no working groups, and no governance
board. If the project grows to need one, this page will be the
first thing to change.

## Decision process

The maintainer applies a two-tier model.

### Routine changes — maintainer discretion

Bug fixes, refactors, dependency bumps, docs edits, test additions,
and contained feature work land at the maintainer's discretion
once the gates in [`CONTRIBUTING.md`](CONTRIBUTING.md) pass:
`make lint`, `make test`, docs updated in the same change, DCO
sign-off present. One maintainer approval is sufficient. No
written design artefact is required beyond the PR description and,
for features, a journey doc under `specs/journeys/` (see
[`AGENTS.md`](AGENTS.md) working loop).

### SPEC-level changes — ADR required

A change is **SPEC-level** when it does any of the following:

- introduces or removes a plane (`aiplane`, `agt`, `knowledge`,
  `power`, `stack`, `syauth`, supervision),
- changes the IPC envelope or any wire-format schema,
- changes the supervision shape (`sy.target` group root, unit
  dependencies, socket activation surface),
- adds or removes a dependency that lands on the runtime path
  (ORT EP, qdrant, PAM stack, BlueZ, polkit, SELinux module),
- overrides or supersedes a previous ADR,
- changes a public contract documented in `docs/reference/` (CLI
  flag semantics, config-file key under `configs/sy/`, exit code,
  systemd unit grant).

SPEC-level changes ship a new ADR under `docs/adr/` in the **same
change** that lands the code. The ADR threshold and the MADR 4.0
shape are defined in
[`docs/adr/0001-use-adrs.md`](docs/adr/0001-use-adrs.md); follow
it. The pattern is already exercised in practice — see
[`specs/research/architecture-refactor/SPEC.md`](specs/research/architecture-refactor/SPEC.md),
whose six-zone hardening proposal will land as a series of ADRs
(workspace decomposition, typed IPC, aiplane admission control,
agt sandbox, supervision, observability) rather than as a single
big-bang refactor commit.

The flow:

1. Open an issue or discussion describing the change. The
   maintainer will either ask for an ADR up front or merge a
   non-ADR-gated PR.
2. If an ADR is required, author it via `/documenter adr <slug>`
   and open it as its own PR (or as the lead commit of the
   feature PR). The ADR carries Status / Context / Decision
   Drivers / Considered Options / Decision Outcome / Consequences
   / Links.
3. Land the ADR first (or atomically with the code that depends
   on it). The PR description links the ADR.
4. Subsequent changes that touch the same decision either cite
   the existing ADR or supersede it with a new one — accepted
   ADRs are append-only.

For long-form research that does not yet have a ruling, use
`specs/research/` (the SPEC stays the journal, the ADR is the
short-form ruling — see ADR-0001 §Decision Drivers).

## Disputes

The project is small enough that disputes are rare and resolved by
conversation. If a contributor disagrees with a maintainer
decision:

1. Open a discussion (or comment on the issue) explaining the
   disagreement. State the trade-off, not the personal preference.
2. The maintainer reads, responds, and either changes course or
   restates the reason. The maintainer's call is final.
3. For conduct issues, escalate through
   [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) — the same private
   channel — rather than through this page.

A "lazy consensus" model would be theatre with one maintainer; the
project does not pretend otherwise.

## Adding maintainers

The project would consider a second maintainer when **all** of
these are true:

- Sustained contribution across at least two planes, with PRs
  merged over multiple release cycles.
- Demonstrated alignment with the non-negotiables in
  [`AGENTS.md`](AGENTS.md) (tests-first, zero dead code, no
  snowflakes, fix root causes).
- A real bus-factor case for shared write access — for example,
  the maintainer being unavailable or a plane growing beyond what
  one person can review.

Nomination is by the existing maintainer; there is no application
form. A new maintainer is added by granting `Maintain` access on
the repository and updating the **Roles** section of this page in
the same PR.

## Removing maintainers

A maintainer is removed at their own request, or by mutual
agreement after sustained inactivity. Removal is recorded in the
**Roles** section.

## Changing this document

This page is itself governed by the routine-change tier: open a
PR, the maintainer reviews. A change that adds or removes a role,
or that changes the ADR threshold, is treated as SPEC-level and
requires an ADR superseding the relevant section of ADR-0001.

## See also

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — how to propose a change.
- [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) — conduct expectations.
- [`SECURITY.md`](SECURITY.md) — private vulnerability channel.
- [`docs/adr/0001-use-adrs.md`](docs/adr/0001-use-adrs.md) — the
  ADR process and threshold.
- [`AGENTS.md`](AGENTS.md) — the coding-agent persona, working
  loop, and non-negotiables every contribution (human or agent)
  inherits.
