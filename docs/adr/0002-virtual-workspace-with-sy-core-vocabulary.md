# 0002 — Virtual workspace with a `sy-core` vocabulary crate

- Status: accepted

> Template: [MADR 4.0](https://adr.github.io/madr/).
> Lifted from: `specs/research/architecture-refactor/SPEC.md` §3.2 row K1.

## Context and Problem Statement

`sy` started life as a single binary crate. As the planes grew —
`aiplane` (NPU inference daemon), `knowledge` (qdrant + embed
consumer), `agt` (sandboxed agent runner), `stack` (layer-shell
bar), `power` (governor + bandit), `syauth` (PAM + BlueZ +
Android) — everything ended up under `src/` with implicit
coupling. The aiplane scheduler types could be reached from the
stack-bar UI; the knowledge daemon could import aiplane internals
directly instead of going through IPC; a touch in `src/main.rs`
recompiled half the workloads. The friend's review (cited inline
in the SPEC) flagged this as a god-binary anti-pattern with the
highest-risk coupling between `aiplane` and `knowledge`.

A workspace split is the obvious answer, but Rust workspaces have
non-obvious failure modes: a "hub" crate that every other crate
depends on becomes a rebuild bottleneck, and per-crate `publish`
discipline is heavy ceremony for a single-maintainer rice that
nobody installs from crates.io. The decision is the *shape* of
the workspace: how many crates, who depends on whom, what is
versioned how.

## Decision Drivers

- **Decouple `aiplane` from `knowledge`**: they must talk through
  the IPC envelope, never through direct crate dependency. This
  is the load-bearing decoupling the architecture-refactor SPEC
  is built around.
- **Keep `main.rs` parsing-only**: the binary entry should be a
  clap router; behaviour lives in library crates so tests can
  link them directly without a subprocess.
- **Avoid a hub-rebuild penalty**: a shared "vocabulary" crate is
  unavoidable (everything needs `Priority`, `WorkloadKind`,
  `ErrorCode`), but it must stay tiny and dependency-light or
  every touch invalidates the whole workspace's incremental
  cache.
- **Match the Rust convention for single-binary applications**:
  matklad's flat-workspace pattern, jj / helix / zellij
  conventions. `sy` is not a library ecosystem; it is one
  binary with a workspace's worth of internal modules.
- **No premature crate publication**: `sy` is a Fedora-coupled
  OS layer with NPU re-exec requirements. End users will not
  `cargo install sy`. Lockstep versioning is correct; per-crate
  semver is over-engineered.

## Considered Options

- **Option 1: Virtual workspace, thin `sy` binary + six lib
  crates + `sy-testutils`** — `sy-core` (types / errors /
  policy schema), `sy-ipc` (transport + envelope), `sy-aiplane`
  (scheduler + workers + workloads), `sy-knowledge` (qdrant +
  embed client + MCP), `sy-stack` (bar + clip), `sy-agt`
  (agent client + ACP + policy eval + sandbox), plus dev-only
  `sy-testutils` for the daemon-in-thread harness. All
  `publish = false`, lockstep-versioned via
  `version.workspace = true`.
- **Option 2: Keep a single crate** — preserves the god-binary
  anti-pattern.
- **Option 3: Three crates** (`sy` / `sy-core` / `sy-engine`) —
  smaller blast radius, but doesn't separate aiplane from
  knowledge.
- **Option 4: Ripgrep-style public semver per crate** — each
  crate publishable, each on its own version, full API
  discipline.

## Decision Outcome

Chosen option: **Option 1, virtual workspace with seven
internal crates and a tiny `sy-core` vocabulary**, because it
is the smallest split that:

1. Forces `sy-aiplane` and `sy-knowledge` to communicate over
   `sy-ipc` rather than direct dependency (the load-bearing
   decoupling).
2. Keeps `src/main.rs` a clap router (each plane's behaviour
   lives in its own crate, addressable for direct unit / integration
   tests).
3. Bounds the hub-rebuild cost: `sy-core` exports only
   `Priority`, `WorkloadKind`, `Request`, `Response`,
   `ErrorCode`, and IPC envelope types — no heavyweight deps,
   no business logic.

Layout:

```
sy (thin binary)
  depends on
  sy-core   sy-ipc   sy-aiplane   sy-knowledge   sy-stack   sy-agt

sy-core   — types, errors, policy schema (vocabulary only)
sy-ipc    — transport, envelope, SO_PEERCRED, blob channel
sy-aiplane — scheduler, worker pool, workloads
sy-knowledge — daemon, qdrant, embed client, MCP server
sy-stack  — bar UI, clip, ontology
sy-agt    — agent client, ACP, policy eval, sandbox
sy-testutils — devdeps only, daemon-in-thread harness
```

Discipline:

- `sy-knowledge` MUST NOT depend on `sy-aiplane` directly. They
  speak through `sy-ipc`.
- All workspace crates set `publish = false`.
- Lockstep versioning via `version.workspace = true`.
- A `cargo-deps` / CI check fails the build if `sy-core` gains
  a heavyweight dep.

## Consequences

- **Good**: aiplane / knowledge coupling is a compile-time error,
  not a code-review reminder.
- **Good**: `main.rs` stays small and parse-only; every plane is
  unit-testable without spawning a subprocess.
- **Good**: matches the matklad / jj / helix / zellij convention
  any Rust contributor already recognises.
- **Good**: lockstep versioning makes release cuts a single
  `cargo release` invocation, no per-crate negotiation.
- **Neutral**: `sy-testutils` is dev-only, but it has to be a
  proper crate (not `#[cfg(test)]`) so integration tests in
  multiple member crates can share the daemon-in-thread harness.
- **Bad**: a workspace migration produces a noisy diff. Mitigation
  per the SPEC: land Zone 1 as two commits — (a) introduce the
  workspace shell + `sy-core` + binary with no module moves;
  (b) move modules one at a time in follow-on PRs.
- **Bad**: `sy-core` is now a contract surface. Adding a field
  to `Request` extends the protocol; we cannot silently rename
  things. This is a feature, not a bug, but it raises the cost
  of touching `sy-core` by design.

## Pros and Cons of the Options

### Option 1 — Virtual workspace, seven crates, tiny `sy-core`

- Good: smallest split that achieves the aiplane/knowledge
  decoupling.
- Good: `sy-core` stays parse-only and tiny, capping the
  rebuild blast radius.
- Good: each plane's tests link the plane's crate directly.
- Neutral: seven Cargo manifests to maintain.

### Option 2 — Single crate

- Good: zero ceremony.
- Bad: god-binary anti-pattern. Aiplane and knowledge can import
  each other freely; the IPC contract becomes advisory rather
  than enforced.

### Option 3 — Three crates (`sy` / `sy-core` / `sy-engine`)

- Good: smaller manifest count than Option 1.
- Bad: lumps aiplane and knowledge into `sy-engine`, which is
  exactly the coupling the refactor is trying to break.

### Option 4 — Ripgrep-style public semver per crate

- Good: each crate is independently versionable and
  publishable.
- Bad: over-engineered for a single-maintainer rice that
  nobody installs from crates.io. Per-crate semver implies
  API stability guarantees `sy` has not committed to and does
  not need.

## Links

- Source: `specs/research/architecture-refactor/SPEC.md` §3.2
  row K1, §3.3 Zone 1, §4.1 architecture diagram, §6 anti-goals
  ("No premature crate publication").
- Related decision: [ADR-0001 — Use ADRs](0001-use-adrs.md).
- Companion decision the IPC envelope rests on:
  `specs/research/architecture-refactor/SPEC.md` §3.2 row K2
  (JSON-RPC 2.0 + `sy.v1` envelope; lift into a future ADR
  when stable).
- Convention precedents: matklad's flat-workspace pattern,
  the [jj](https://github.com/martinvonz/jj),
  [helix](https://github.com/helix-editor/helix), and
  [zellij](https://github.com/zellij-org/zellij) workspaces.
- Audit row: `specs/docs-audit/AUDIT-full.md#r-adr-01--should`.
- Roadmap item: `specs/docs-audit/PLAN-full.md` Item 16.
