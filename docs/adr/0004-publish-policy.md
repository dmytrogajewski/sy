# 0004 — `publish = false` on the root `sy` package

- Status: accepted

Who this is for: people wondering why `sy` is not on crates.io.

> Template: [MADR 4.0](https://adr.github.io/madr/).

## Context and Problem Statement

The root `sy` `[package]` block in `Cargo.toml` had no `publish`
field, which made it nominally publishable to
[crates.io](https://crates.io). The three workspace crates
(`sy-core`, `sy-ipc`, `sy-testutils`) already carried
`publish = false` explicitly, but the binary crate did not — only a
comment in the `[workspace]` header mentioned that workspace
members are "lockstep-versioned, `publish = false`". The audit
caught the mismatch in `R-ECO-04` (see
`specs/docs-audit/AUDIT-full.md`).

`sy` is a Fedora-coupled Agentic OS layer, not a reusable library
crate. The binary expects:

- a working AMD Ryzen AI NPU at `/dev/accel/accel0`;
- AMD's bundled `libonnxruntime.so` reachable via the re-exec dance
  in `aiplane::reexec` (the dynamic linker drops `LD_LIBRARY_PATH`
  when `AT_SECURE` is set, so the binary cannot `setcap` itself —
  the caps live in the user systemd units);
- a user-level systemd graph rooted at `sy.target` with `Type=notify`
  units, journald, polkit, and SELinux file contexts shipped by
  `sy apply`;
- niri / waybar / `sy file` configs from `configs/` to render the
  desktop.

None of those preconditions can be satisfied by a `cargo install
sy` from a generic Rust toolchain. Publishing the binary would
ship a façade that misleads users into thinking they can install
it like any other CLI crate. At the same time, the `R-ECO-05`
audit row wants `readme = "README.md"` declared so `crates.io` /
`docs.rs` / `cargo info` render the README if the publish policy
ever flips.

## Decision Drivers

- **Truth in packaging**: `cargo publish` should fail loudly rather
  than upload a binary the consumer cannot run.
- **Audit closure**: `R-ECO-04` (`SUGGESTED`) flagged the missing
  `publish` field; `R-ECO-05` (`SUGGESTED`) flagged the missing
  `readme` field. Both can close in one pass.
- **No surprise plumbing**: keep `cargo metadata` honest so any
  future tool (release-please, cargo-deny, OpenSSF Scorecard) sees
  the SPDX identifier and the publish posture without guessing.
- **No clocks**: the decision needs to be recorded without a date
  string (project convention; see ADR-0001 Decision Drivers).
- **Reversibility**: if `sy` is ever split into a publishable
  sub-crate (for example, the `aiplane` IPC client), that crate
  flips its own `publish` flag; this ADR is scoped to the root
  binary.

## Considered Options

- **Option 1: `publish = false` on the root `sy` package, plus
  `readme = "README.md"` and `license = "MIT"`.** Add the same
  `license = "MIT"` line to the three existing workspace crates so
  every `[package]` block carries the SPDX identifier.
- **Option 2: Make `sy` publishable.** Add
  `[package.metadata.docs.rs] all-features = true` and
  `readme = "README.md"`, keep the binary `cargo install`-able.
- **Option 3: Status quo.** Leave the root `[package]` block as-is
  (no `publish`, no `readme`, no `license` field on any crate), let
  the workspace-header comment carry the intent.

## Decision Outcome

Chosen option: **Option 1**, because the binary cannot be installed
outside the documented Fedora 43 path (re-exec dance, systemd
units, polkit grants, SELinux contexts), and the audit's
recommendation for `R-ECO-04` explicitly favours `publish = false`
for OS-coupled binaries. Declaring `readme = "README.md"` alongside
is cost-free and future-proof: if a follow-on ADR ever flips the
publish flag, the README is already wired up. Adding
`license = "MIT"` to every `[package]` block makes the SPDX
identifier visible in `cargo metadata` (which is what tools like
`cargo-deny`, OpenSSF Scorecard, and `cargo-license` consume) and
matches the contents of `LICENSE` at the repo root.

The four `[package]` blocks now read:

| Crate | `publish` | `license` | `readme` |
|---|---|---|---|
| `sy` (root) | `false` | `"MIT"` | `"README.md"` |
| `sy-core` | `false` | `"MIT"` | — |
| `sy-ipc` | `false` | `"MIT"` | — |
| `sy-testutils` | `false` | `"MIT"` | — |

`readme` is only declared on the root package because that is the
package that maps to the repo's `README.md`. The three workspace
crates have no per-crate README and do not need one — their
`description` field carries the one-line summary, and the
canonical reference for each lives under `docs/reference/` and in
the SPEC.

## Consequences

- **Good**: `cargo publish` on the root package now refuses
  outright, preventing an accidental crates.io upload of a binary
  that cannot run on a stock Rust toolchain.
- **Good**: `cargo metadata --format-version 1 --no-deps` now
  reports `license: "MIT"` for every workspace member, closing the
  SPDX gap that `R-ECO-04` flagged and giving downstream tools
  (`cargo-deny`, OpenSSF Scorecard, `cargo-license`) a single
  source of truth.
- **Good**: `readme = "README.md"` is in place, so if a future ADR
  flips the publish flag (for example, to publish a thin
  `aiplane`-client sub-crate), the README plumbing already works.
- **Neutral**: no behavioural change to `cargo build`,
  `cargo test`, or any of the runtime planes. Manifest-only edit.
- **Bad**: contributors who add a new workspace member must
  remember to set `publish = false` and `license = "MIT"` on its
  `[package]` block. `CONTRIBUTING.md` already requires an ADR for
  changes that "add or remove a plane", which covers the case where
  someone wants to flip `publish = true` on a future crate.

## Pros and Cons of the Options

### Option 1 — `publish = false` + `readme` + `license`

- Good: closes both `R-ECO-04` and `R-ECO-05` in one pass.
- Good: makes the workspace's intent explicit rather than relying
  on the `[workspace]` header comment.
- Good: zero runtime impact; the change is pure manifest metadata.
- Neutral: requires a future ADR if `sy` ever becomes publishable.

### Option 2 — make `sy` publishable

- Good: would let interested third parties `cargo install sy` and
  inspect what they get.
- Bad: every "install" would fail at runtime because the binary
  needs the AMD venv re-exec dance, user systemd units, polkit
  grants, and SELinux contexts. Shipping a binary that cannot
  start is worse than not shipping at all.
- Bad: opens the door to a security surface (a crate's name on
  crates.io is itself a foothold for typosquatting); the project
  has no release cadence that would justify the maintenance burden
  of a published binary.

### Option 3 — status quo

- Good: no manifest edits.
- Bad: leaves the audit gap open (`R-ECO-04`, `R-ECO-05`); the
  root package remains nominally publishable; `cargo metadata`
  reports `license: null` for every crate, which trips
  OpenSSF / `cargo-deny`.
- Bad: the workspace-header comment claims `publish = false` is
  the policy, but the root package does not enforce it — a future
  contributor reading only the root `[package]` block could
  publish by accident.

## Links

- Template: [MADR 4.0](https://adr.github.io/madr/).
- Companion ADR: [0001 — Use Architecture Decision Records](0001-use-adrs.md).
- Audit rows: `specs/docs-audit/AUDIT-full.md#r-eco-04--suggested`
  and `specs/docs-audit/AUDIT-full.md#r-eco-05--suggested`.
- Roadmap item: `specs/docs-audit/PLAN-full.md` Item 24.
- Cargo manifest reference:
  [`publish` field](https://doc.rust-lang.org/cargo/reference/manifest.html#the-publish-field),
  [`readme` field](https://doc.rust-lang.org/cargo/reference/manifest.html#the-readme-field),
  [`license` field](https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields).
- SPDX identifier: [`MIT`](https://spdx.org/licenses/MIT.html);
  text at `LICENSE`.
