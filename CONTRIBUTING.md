# Contributing to sy

<!-- Rendered from the `/documenter contributing` template
     (Good Docs Project / Standard CONTRIBUTING shape per GitHub's
     community-profile guidance:
     https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/about-community-profiles-for-public-repositories).
     Voice anchored on README.md, AGENTS.md, and SECURITY.md. -->

Thanks for your interest in `sy`. This guide tells you how to ask a
question, file a bug, propose a change, run the tests, and get a
patch merged.

`sy` is a modular desktop and agent environment for Fedora, built
from separately packaged feature executables and declarative configs.
The product story is
[`docs/explanation/what-sy-is.md`](docs/explanation/what-sy-is.md);
the docs map is [`docs/intro.md`](docs/intro.md).
See [`README.md`](README.md) for the planes and [`AGENTS.md`](AGENTS.md)
for the coding-agent persona (tests-first, zero dead code, no
snowflakes).

## Code of conduct

This project follows the [Contributor Covenant 2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/).
Participation in the project means you agree to uphold it. See
[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) for the enforcement
contact. For enforcement concerns, use the contact listed in
[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).

## How to ask a question

- Prefer a **GitHub Discussion** on the repository over an issue.
- If discussions are not enabled, open a regular issue and prefix the
  title with `question:` so it can be triaged separately from bugs
  and feature requests.
- Do **not** email the maintainer for usage questions. The private
  email channel is reserved for vulnerability reports (see
  [`SECURITY.md`](SECURITY.md)).

## How to file a bug

1. Search existing issues first. If you find a match, add a comment
   with your reproduction and environment instead of opening a new
   one.
2. Open a new issue using the **bug report** template under
   `.github/ISSUE_TEMPLATE/`. If it is unavailable, use a plain issue
   with the sections below.
3. Include:
   - **What you expected to happen.**
   - **What actually happened**, including the exact command, stderr,
     and the relevant exit code.
   - **Reproduction steps**, ideally a single `sy ...` invocation or
     a minimal `configs/` diff.
   - **Environment**: `sy --version`, `rustc --version`,
     `cargo --version`, `cat /etc/fedora-release`, and — for `aiplane`
     bugs — the output of `sy aiplane status --json` and
     `sy doctor --json`.
4. If the bug involves the NPU plane, mention whether
   `/dev/accel/accel0` is present, whether
   `/opt/AMD/ryzenai/venv` exists, and whether the daemon is running
   under `systemctl --user`.
5. Title prefix: `bug:` to match the project's Conventional Commits
   convention (see [Commit policy](#commit-policy) below).

For **security-sensitive** bugs (anything affecting the `syauth` PAM
module, `CAP_*` ambient grants on the user daemon, SELinux policy
under `configs/selinux/`, or the polkit rule under `configs/policy/`),
follow [`SECURITY.md`](SECURITY.md) instead — do not open a public
issue.

## How to propose a change

1. **Open an issue or discussion first** for anything bigger than a
   one-file tweak. The maintainer would rather discuss a design than
   close a PR.
2. **Fork** the repo and branch from `main`. Use a descriptive
   branch name (`feat/<slug>`, `fix/<slug>`, `docs/<slug>`).
3. **Follow the working loop from [`AGENTS.md`](AGENTS.md)**: read
   related code, write tests first, implement the minimal change,
   run `make lint && make test`, refactor for clarity, re-run the
   gates. Steps under 15 lines per TDD iteration; one behaviour per
   iteration.
4. **Update docs** in the same change. If you change user-visible
   behaviour, update `README.md`, the relevant `docs/` page, and add
   an entry under `## [Unreleased]` in `CHANGELOG.md`.
5. **Sign your commits** with `git commit -s` to certify the DCO
   (see [Commit policy](#commit-policy)).
6. **Open a pull request** using the PR template. Link the issue
   with `Closes #<n>`. Keep the PR focused — one logical change per
   PR.

For changes that touch a plane's public contract (the IPC envelope,
a CLI flag's semantics, a config-file key in `configs/sy/*.toml`, or
the systemd unit grants) the maintainer may ask for an
[ADR](docs/adr/) before approving. See [`GOVERNANCE.md`](GOVERNANCE.md)
for decision ownership.

## Development setup

For Fedora packaging, use a release in the RPM build matrix listed in
the README. For code that does not depend on Fedora, any Linux host
with a recent Rust toolchain is suitable.

```bash
# Stable Rust 2024 edition. Use rustup for parity with CI.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add clippy rustfmt

# Clone and build.
git clone https://github.com/dmytrogajewski/sy.git
cd sy
cargo build --release
```

For package installation, follow the [README install guide](README.md#install-from-rpms).
For AMD NPU setup, follow the [NPU guide](docs/how-to/set-up-npu.md).

The "no snowflakes" rule (see [`CLAUDE.md`](CLAUDE.md)) is binding
on every contribution: any environment change must be encoded in
`configs/` or in the `sy` binary so that
`cargo build --release && ./target/release/sy apply` on a fresh
machine reproduces it. PRs that ask the user to run a one-off
manual command will be sent back.

## Tests, lint, style

The [`Makefile`](Makefile) wraps the common gates. Run them locally
before opening a PR.

| Command | What it does |
|---|---|
| `make build` | `cargo build --workspace` (debug). |
| `make release` | `cargo build --workspace --release`. |
| `make test` | `cargo test --workspace --all-targets`. Fast — uses `FakeWorkload` for the NPU plane, no `/dev/accel/accel0` required. |
| `make test-npu` | `cargo test --workspace --all-targets --features test-npu`. Runs gated tests against the real NPU. The device is single-context, so run this only when the AI service is not using the NPU and the test host is available for hardware testing. |
| `make lint` | `scripts/check_main_rs_loc.sh` + `cargo clippy --workspace --all-targets -- -D warnings`. **Zero warnings.** |
| `make fmt` | `cargo fmt --all`. |
| `make fmt-check` | `cargo fmt --all -- --check`. Local formatting check. |
| `make audit` | `cargo deny check` if `cargo-deny` is installed; skipped otherwise. |
| `make bench` | `cargo bench --all-targets`. |

The non-negotiables from [`AGENTS.md`](AGENTS.md) apply:

- **Zero clippy warnings.** `cargo clippy --workspace --all-targets -- -D warnings` must pass.
- **Zero `#[allow(dead_code)]`** outside `#[cfg(test)]`.
- **No `TODO` / `FIXME` / `unimplemented!()`** in committed code. (The
  `post-edit-check.sh` hook blocks this on every edit.)
- **Tests come first or alongside the implementation.** No PR ships
  code without coverage of the new behaviour.
- **Flaky tests are bugs.** Fix or quarantine immediately.
- **Unsafe code is denied by default.** Each `unsafe` block requires
  a comment justifying it.
- **Fix root causes, not symptoms.** If a fallback chain triggers,
  the question is "why did the primary path fail", not "how do I
  make the fallback look prettier".

## Documentation expectations

Documentation is a deliverable, not an afterthought.

Documentation templates and other third-party material are listed in
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). Preserve the attribution
comments at the top of derived documentation pages when editing them.

- Every PR that changes user-visible behaviour ships a docs update
  in the same change. That can be `README.md`, a page under
  `docs/tutorials/`, `docs/how-to/`, `docs/reference/`, or
  `docs/explanation/`, or an entry under `## [Unreleased]` in
  `CHANGELOG.md`.
- Per-item Rust doc comments (`///`) and crate-level inner docs
  (`//!`) live in the source. `cargo doc --no-deps` should stay
  clean under `RUSTDOCFLAGS="-D warnings"`.
- Long-form design lives under [`specs/`](specs/) (journeys, bugs,
  roadmaps, research SPECs). User-facing prose lives under `docs/`.
  The browsable site is built from that tree by Docusaurus in
  [`website/`](website/); preview with `cd website && npm start`,
  or `make docs-site` for a production build. GitHub Pages deploys
  from `.github/workflows/docs-site.yml` on push to `main`. The
  workflow cannot flip repository Settings: set **Pages → Source**
  to **GitHub Actions** once, then the next `main` push publishes
  `https://<owner>.github.io/sy/`.
- `make docs-lint` runs markdownlint, Vale, cspell, and lychee when
  those tools are installed. CI runs the documentation checks for
  documentation changes.

If your change adds or renames a CLI subcommand or flag, update the
[CLI reference](docs/reference/cli.md), the relevant feature guide,
and the per-command `--help` text. CLIG and agent-friendly conventions
(see [`CLAUDE.md`](CLAUDE.md)) are non-negotiable: `--json` on every
command that produces output, `--dry-run` on every command that
changes state, stable exit codes, and every flag also settable via
an `SY_*` env var.

## Commit policy

### Conventional Commits

Commit subjects follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/):

```
<type>(<scope>): <imperative subject>

<optional body — what and why, not how>

<optional footer — BREAKING CHANGE: ..., Signed-off-by: ..., Closes #N>
```

Types in active use: `feat`, `fix`, `refactor`, `docs`, `test`,
`chore`, `perf`, `build`, `ci`. Scopes match the plane or module
touched (`aiplane`, `agt`, `knowledge`, `power`, `stack`, `syauth`,
`supervision`, `configs`, `docs`, ...). Recent examples from the
log:

```
fix(supervision): enable the user target during apply
refactor(aiplane): route knowledge requests through the daemon
fix(desktop-shell): align the top panel with the desktop edge
```

Breaking changes carry a `!` after the type/scope (`feat(ipc)!: ...`)
**and** a `BREAKING CHANGE:` footer that explains the migration
path. The `CHANGELOG.md` `Changed` / `Removed` entries are derived
from these.

### Developer Certificate of Origin (DCO)

Every commit must be signed off under the
[Developer Certificate of Origin 1.1](https://developercertificate.org/).
The sign-off is your statement that you have the right to submit
the contribution under the project's licence
([MIT](LICENSE)) — it is **not** a copyright assignment and **not**
a contributor licence agreement.

Sign off with the `-s` flag:

```bash
git commit -s -m "fix(knowledge): close qdrant fd on cancel"
```

This appends a trailer of the form:

```
Signed-off-by: Your Name <your.email@example.com>
```

Configure `user.name` and `user.email` in `git` once
(`git config --global user.name "..."`, `git config --global user.email "..."`)
and `-s` will fill them in for every commit. If you forget the
sign-off before publishing your branch, amend the commit locally.
For a branch that others already use, coordinate with them or add a
new signed commit; do not rewrite shared history.

There is **no CLA**.

## Versioning (SemVer)

`sy` follows [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html):

- **MAJOR** — incompatible changes to a public contract (a CLI
  flag's semantics, the IPC envelope shape, a config-file key
  under `configs/sy/`, a systemd unit grant, the IPC v1
  request/response schema, or the on-disk layout of
  `~/.cache/sy/`).
- **MINOR** — backwards-compatible additions (a new subcommand, a
  new optional config key, a new plane, a new workload).
- **PATCH** — backwards-compatible fixes (bug fix, performance
  improvement, dependency bump that does not change behaviour).

While the project is pre-`1.0.0` (currently `0.1.0`), the SemVer
spec permits breaking changes in MINOR cuts. In practice the
maintainer treats `0.x` cuts the same as `1.x`: a breaking change
calls for an explicit `BREAKING CHANGE:` footer in the commit, a
`Changed` or `Removed` entry in [`CHANGELOG.md`](CHANGELOG.md), and
release notes under `docs/release-notes/` once that directory
exists.

## Getting your PR merged

- CI must be green, including `make lint`, `make test`, and the docs
  workflow when documentation changes.
- All review comments addressed or explicitly deferred with a link
  to the follow-up issue.
- For a feature, a journey doc under `specs/journeys/` is expected;
  for a bug, a `specs/bugs/BUG-<slug>.md` reproduction note.
- One maintainer approval is sufficient for most changes; ADR-gated
  changes need the ADR merged first.

Thanks for contributing. If something in this guide is wrong or
unclear, open a PR against this file — that is itself a perfectly
good first contribution.
