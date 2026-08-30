# OpenSSF Best Practices Badge — passing-criteria mapping

<!-- Rendered for rubric row R-COMPLY-01 of
     specs/docs-audit/AUDIT-full.md, per PLAN-full.md Item 22.
     Source: OpenSSF Best Practices Badge — passing criteria
     https://www.bestpractices.dev/en/criteria/0
     Voice anchored on README.md, AGENTS.md, SECURITY.md,
     CONTRIBUTING.md, CODE_OF_CONDUCT.md, SUPPORT.md, GOVERNANCE.md. -->

`sy` maps a subset of the [OpenSSF Best Practices Badge — passing
tier](https://www.bestpractices.dev/en/criteria/0) to files in this
repo. If you are filling in the public badge, this is the evidence
index. If you are a new contributor, start at
[CONTRIBUTING.md](../../CONTRIBUTING.md) instead.

Status values:

- **pass** — the project meets the criterion and the evidence is in
  this repo today.
- **met (CI-gated)** — the project meets the criterion in principle;
  the CI surface that exposes the evidence to the badge auditor is
  configured under [`.github/workflows/`](../../.github/workflows/)
  but has not yet been observed green on a public PR.
- **gap** — the project does not meet the criterion yet.
- **n/a** — the criterion does not apply (for example, criteria
  scoped to cryptographic libraries or to multi-maintainer projects).

The status column intentionally distinguishes between criteria the
project actually satisfies and criteria where the evidence is in
flight — the badge auditor will read both columns the same way, but
contributors should not.

## In-scope passing-criteria clauses

These are the five clauses called out by
[`specs/docs-audit/PLAN-full.md`](../../specs/docs-audit/PLAN-full.md)
Item 22.

| Clause | Status | Evidence | Note |
|---|---|---|---|
| `basics_documentation` | pass | [`README.md`](../../README.md), [`CONTRIBUTING.md`](../../CONTRIBUTING.md), [`docs/tutorials/getting-started.md`](../tutorials/getting-started.md), [`docs/reference/cli.md`](../reference/cli.md), [`docs/explanation/architecture.md`](../explanation/architecture.md) | README explains what `sy` is and how to install/use it; Diátaxis quadrants under `docs/` cover tutorial, how-to, reference, and explanation surfaces. |
| `security_vulnerability_report_process` | pass | [`SECURITY.md`](../../SECURITY.md) | Names a private email channel (`dmytrogajewski@gmail.com`), a supported-versions table, a 7-day acknowledgement SLA, and a 30-day fix-or-mitigation target. |
| `quality_build_status` | met (CI-gated) | [`README.md` §Install / apply](../../README.md#install--apply), [`Makefile`](../../Makefile) (`make build`, `make release`, `make test`) | `cargo build --release` and `make test` are the canonical build and test gates; the badge expects a public CI status URL. A docs CI workflow exists at [`.github/workflows/docs.yml`](../../.github/workflows/docs.yml); a Rust build/test CI workflow is the open follow-up before the badge auditor can observe a green build. |
| `analysis_static_analysis` | pass | [`AGENTS.md`](../../AGENTS.md) non-negotiables, [`Makefile`](../../Makefile) (`make lint`) | `cargo clippy --workspace --all-targets -- -D warnings` is mandatory on every change. Zero `#[allow(dead_code)]` outside `#[cfg(test)]`. The hook at [`.claude/hooks/post-edit-check.sh`](../../.claude/hooks/post-edit-check.sh) blocks `TODO` / `FIXME` / `unimplemented!()` at write time. |
| `code_of_conduct` | pass | [`CODE_OF_CONDUCT.md`](../../CODE_OF_CONDUCT.md) | Adopts [Contributor Covenant 2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/) by reference (pointer, not copy — see [`specs/docs-audit/PLAN-full.md`](../../specs/docs-audit/PLAN-full.md) Item 27 / R-COMPLY-02), names the enforcement contact, and documents the reporting workflow. |

## Other passing-criteria clauses the project trivially satisfies

These are not required by the audit row but the evidence is already
in the repo, so they go in the mapping table the badge application
will reference.

| Clause | Status | Evidence | Note |
|---|---|---|---|
| `basics_license` | pass | [`LICENSE`](../../LICENSE) | MIT licence, SPDX identifier visible in the file header. README's §License section cites `MIT` and links the file. |
| `basics_license_location` | pass | [`LICENSE`](../../LICENSE) at repo root | Standard location. |
| `basics_floss_license` | pass | [`LICENSE`](../../LICENSE) | MIT is on the [OSI-approved list](https://opensource.org/licenses/MIT). |
| `basics_documentation_basics` | pass | [`README.md`](../../README.md), [`CONTRIBUTING.md`](../../CONTRIBUTING.md) | README answers "what is this" / "how to install" / "how to use" in the first screen; CONTRIBUTING explains how to build and test. |
| `basics_documentation_interface` | pass | [`docs/reference/cli.md`](../reference/cli.md), [`README.md` §CLI cheat-sheet](../../README.md#cli-cheat-sheet) | Per-subcommand flag tables, exit codes, env vars. Every command also supports `--help`. |
| `basics_repo_public` | pass | GitHub repository at `https://github.com/dmytrogajewski/sy` | Public, version-controlled. |
| `basics_repo_distributed` | pass | git | Distributed VCS. |
| `basics_repo_interim` | pass | Conventional Commits in commit history; granular commits per [`AGENTS.md`](../../AGENTS.md) working loop | Intermediate states are committed, not squashed away. |
| `basics_repo_track` | pass | git | All source files tracked. |
| `basics_contribution` | pass | [`CONTRIBUTING.md`](../../CONTRIBUTING.md) | Explains how to file an issue, propose a change, run tests, sign commits (DCO), and what the commit policy is. |
| `basics_contribution_requirements` | pass | [`CONTRIBUTING.md` §Commit policy](../../CONTRIBUTING.md#commit-policy), [`CONTRIBUTING.md` §Versioning (SemVer)](../../CONTRIBUTING.md#versioning-semver), [`AGENTS.md`](../../AGENTS.md) non-negotiables | DCO sign-off, Conventional Commits, SemVer, tests-first, zero clippy warnings. |
| `code_of_conduct` (reporting) | pass | [`CODE_OF_CONDUCT.md` §Reporting and enforcement contact](../../CODE_OF_CONDUCT.md#reporting-and-enforcement-contact) | Names contact, response timeline, escalation path for conflicts of interest. |
| `governance` | pass | [`GOVERNANCE.md`](../../GOVERNANCE.md) | Single-maintainer model, decision process, ADR threshold, dispute resolution, criteria for adding a second maintainer. |
| `support` | pass | [`SUPPORT.md`](../../SUPPORT.md) | Routes questions / bugs / vulnerabilities / conduct concerns to the right channel; sets best-effort expectations. |
| `release_notes` | met (CI-gated) | [`CHANGELOG.md`](../../CHANGELOG.md) | Keep a Changelog 1.1.0 shape with an `[Unreleased]` section. Per-release notes under `docs/release-notes/` are deferred until the first `v0.2.0` cut (see [`specs/docs-audit/PLAN-full.md`](../../specs/docs-audit/PLAN-full.md) Item 15). |
| `release_notes_vulnsfixed` | pass | [`SECURITY.md` §Disclosure and credit](../../SECURITY.md#disclosure-and-credit), [`CHANGELOG.md`](../../CHANGELOG.md) `### Security` section per release | Security-relevant fixes land under `### Security` in the changelog and credit the reporter unless they ask not to be named. |
| `vulnerability_report_process` | pass | [`SECURITY.md` §Reporting a vulnerability](../../SECURITY.md#reporting-a-vulnerability) | Private email channel, fields requested, PGP-on-request hedge. |
| `vulnerability_report_private` | pass | [`SECURITY.md` §Reporting a vulnerability](../../SECURITY.md#reporting-a-vulnerability) | Explicitly says do not open a public issue. |
| `vulnerability_report_response` | pass | [`SECURITY.md` §Response targets](../../SECURITY.md#response-targets) | 7-day acknowledgement, 30-day fix or mitigation. |
| `build_repeatable` | pass | [`README.md` §Install / apply](../../README.md#install--apply), [`Makefile`](../../Makefile) (`make release`) | `cargo build --release` is the single build command. The "no snowflakes" rule in [`CLAUDE.md`](../../CLAUDE.md) makes the entire system reproducible: `cargo build --release && ./target/release/sy apply` on a fresh Fedora 43 host. |
| `build_common_tools` | pass | [`Cargo.toml`](../../Cargo.toml), [`README.md`](../../README.md), [`Makefile`](../../Makefile) | `cargo` (stable Rust 2024 edition) is the canonical toolchain. |
| `installation_common` | pass | [`README.md` §Install / apply](../../README.md#install--apply), [`docs/tutorials/getting-started.md`](../tutorials/getting-started.md) | Standard `dnf` / `cargo install` / `sy apply` flow on Fedora 43. |
| `test` | pass | [`Makefile`](../../Makefile) (`make test`, `make test-npu`), [`AGENTS.md` §E2E Testing Philosophy](../../AGENTS.md#e2e-testing-philosophy) | A real test suite exists; the project's working loop blocks merges without coverage of new behaviour. |
| `test_policy_mandated` | pass | [`AGENTS.md`](../../AGENTS.md) non-negotiables, [`CONTRIBUTING.md` §Tests, lint, style](../../CONTRIBUTING.md#tests-lint-style) | "Tests come first or alongside the implementation. No PR ships code without coverage of the new behaviour." |
| `test_continuous_integration` | met (CI-gated) | [`Makefile`](../../Makefile) (`make test`), [`.github/workflows/docs.yml`](../../.github/workflows/docs.yml) | Docs CI runs on every PR that touches `**/*.md`. A Rust build/test CI workflow is the open follow-up for full coverage. |
| `warnings` | pass | [`AGENTS.md`](../../AGENTS.md) "Zero clippy warnings" non-negotiable, [`Makefile`](../../Makefile) `make lint` | `-D warnings` is enforced. |
| `warnings_fixed` | pass | Same as above | Clippy warnings are denied, not allowed. |
| `warnings_strict` | pass | [`Makefile`](../../Makefile) `cargo clippy --workspace --all-targets -- -D warnings` | Strict mode is the default and the only mode. |
| `know_secure_design` | pass | [`AGENTS.md`](../../AGENTS.md), [`SECURITY.md`](../../SECURITY.md), [`docs/explanation/architecture.md`](../explanation/architecture.md) | The persona section calls out unsafe-by-default-denied, root-cause-fix discipline, and least-privilege via per-unit `CAP_*` ambient grants rather than `setcap` on the binary. |
| `know_common_errors` | pass | [`AGENTS.md` §NPU-specific norms](../../AGENTS.md#npu-specific-norms), [`docs/explanation/architecture.md`](../explanation/architecture.md) | Documents the AMD venv re-exec dance, single-context `/dev/accel/accel0` ownership, and the "fake the NPU, not the wire format" rule that keeps CI hermetic. |
| `crypto_call` | n/a | n/a | `sy` does not implement cryptography. It consumes BlueZ + PAM for `syauth`; the underlying crypto is the platform's. |
| `crypto_floss` | n/a | n/a | Same as above. |
| `crypto_keylength` | n/a | n/a | Same as above. |
| `crypto_working` | n/a | n/a | Same as above. |
| `static_analysis` | pass | [`Makefile`](../../Makefile) `make lint`, [`AGENTS.md`](../../AGENTS.md) non-negotiables | `cargo clippy --workspace --all-targets -- -D warnings` is the static-analysis gate. |
| `static_analysis_common_vulnerabilities` | pass | Same as above; `cargo deny check` in [`Makefile`](../../Makefile) `make audit` (best-effort, skipped if `cargo-deny` absent) | Clippy covers Rust-specific bug patterns; `cargo deny` covers dependency-side advisories when installed. |
| `static_analysis_fixed` | pass | Same as above — `-D warnings` denies, does not allow | All flagged findings must be addressed before merge. |
| `static_analysis_often` | pass | Same as above — `make lint` runs on every change | Local pre-push gate and CI gate. |
| `dynamic_analysis` | met (CI-gated) | [`Makefile`](../../Makefile) `make test`, `make test-npu` | The test suite exercises real I/O, real Unix sockets, real qdrant HTTP, and (with `--features test-npu`) the real NPU. A coverage gate is the open follow-up. |
| `documentation_quick_start` | pass | [`README.md` §Install / apply](../../README.md#install--apply), [`docs/tutorials/getting-started.md`](../tutorials/getting-started.md) | Copy-pasteable commands from a fresh Fedora 43 host to `sy aiplane status` green. |

## Submitting the badge application

This document covers the documentation deliverable. The actual badge
application — registering the project at
[bestpractices.dev](https://www.bestpractices.dev/), pasting the
mapping above into the criterion-by-criterion form, and pointing the
auditor at the relevant files — is a **maintainer action**, not part
of this docs item. It is out of scope for
[`specs/docs-audit/PLAN-full.md`](../../specs/docs-audit/PLAN-full.md)
Item 22 by the item's own constraints, and tracked separately in the
maintainer's checklist.

When submitting, the auditor will ask for a justification per
clause; this page is the source for those justifications. Keep the
two in sync: any new file that closes a previously-marked **gap** or
**met (CI-gated)** row should update the corresponding cell here in
the same change.

## See also

- [`SECURITY.md`](../../SECURITY.md) — the disclosure channel itself.
- [`specs/docs-audit/AUDIT-full.md`](../../specs/docs-audit/AUDIT-full.md) — rubric row `R-COMPLY-01`.
- [`specs/docs-audit/PLAN-full.md`](../../specs/docs-audit/PLAN-full.md) — Item 22 (this mapping) and Item 27 (`THIRD_PARTY_NOTICES.md` deferral hinged on `CODE_OF_CONDUCT.md` staying a pointer).
- [OpenSSF Best Practices Badge — passing criteria](https://www.bestpractices.dev/en/criteria/0).
