# AUDIT: full

## Mode
audit

## Project
- Name: `sy` (root crate) + workspace members
- Licence: MIT (`LICENSE`, line 1)
- Ecosystem: rust
- Workflow: journey

## Summary
- MUST findings open: 0
- SHOULD findings open: 0
- SUGGESTED findings open: 0

## Current pass

The current pass rechecked the Sparkplane bridge surface against the pinned
`0.1.5` integration in `sy.toml`. The README, installation and serving
how-tos, CLI/reference pages, and changelog now document the per-launch
`--allow-network` opt-in and `SPARKPLANE_LAUNCH_ALLOW_NETWORK` equivalent.
The text explicitly preserves filesystem sandboxing and approval policy and
links to Sparkplane for client-specific semantics. The moved syauth page now
describes itself as a compatibility index rather than a stub.

The strict public-item documentation check now passes, and the repository has
a `THIRD_PARTY_NOTICES.md` inventory linked from `CONTRIBUTING.md`.

`make docs-lint` exits successfully. Local Markdown linters that are not
installed are skipped by the Make target; CI remains the authoritative
markdownlint, cspell, Vale, and lychee environment. The Docusaurus workflow
continues to build the `website/` tree on documentation changes.

## Top 5 MUST fixes

No MUST fixes are open. Two SHOULD/SUGGESTED findings remain:

1. `R-ECO-01` — document the five public layout constants so strict rustdoc
   passes.
2. `R-COMPLY-02` — add a `THIRD_PARTY_NOTICES.md` inventory.

## In-place edits

The following files already existed and were edited in place rather
than as `<path>.proposed.md` siblings. That matches the user's
explicit authoring instruction in this conversation ("write
comprehensive, full docs", then "do that" for the compliance pass,
including the option to keep those edits). The skill's silent-rewrite
rule is therefore not in play for these paths:

- `README.md`
- `CONTRIBUTING.md`
- `CHANGELOG.md`
- `llms.txt` / `llms-full.txt`
- `docs/reference/glossary.md`
- `docs/tutorials/getting-started.md`
- `docs/explanation/architecture.md`

## Findings

### R-COMMUNITY-01 — MUST
- Status: pass
- Evidence: `README.md` answers what / why / install / use in the first screen.
- Source: https://github.com/RichardLitt/standard-readme
- OpenSSF: `basics_documentation`, `basics_documentation_basics`

### R-COMMUNITY-02 — MUST
- Status: pass
- Evidence: `LICENSE` SPDX-recognisable MIT; `Cargo.toml` `license = "MIT"`.
- Source: https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/about-community-profiles-for-public-repositories
- OpenSSF: `basics_license`, `basics_license_location`, `basics_floss_license`

### R-COMMUNITY-03 — MUST
- Status: pass
- Evidence: `CODE_OF_CONDUCT.md` at repo root.
- Source: https://www.contributor-covenant.org/version/2/1/code_of_conduct/
- OpenSSF: `code_of_conduct`

### R-COMMUNITY-04 — MUST
- Status: pass
- Evidence: `CONTRIBUTING.md` covers issues, PRs, `make lint` / `make test`, DCO.
- Source: https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/about-community-profiles-for-public-repositories
- OpenSSF: `basics_contribution`, `basics_contribution_requirements`

### R-COMMUNITY-05 — MUST
- Status: pass
- Evidence: `SECURITY.md` names a private disclosure channel.
- Source: https://docs.github.com/en/code-security/getting-started/adding-a-security-policy-to-your-repository
- OpenSSF: `security_vulnerability_report_process`, `vulnerability_report_private`

### R-COMMUNITY-06 — SHOULD
- Status: pass
- Evidence: `SUPPORT.md` exists.
- Source: https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/creating-a-default-community-health-file
- OpenSSF: `support` (passing-tier adjacent; mapped in `docs/compliance/openssf-best-practices.md`)

### R-COMMUNITY-07 — SHOULD
- Status: pass
- Evidence: `.github/ISSUE_TEMPLATE/bug_report.md` and `feature_request.md`.
- Source: https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests
- OpenSSF: `basics_contribution`

### R-COMMUNITY-08 — SHOULD
- Status: pass
- Evidence: `.github/PULL_REQUEST_TEMPLATE.md`.
- Source: same as `R-COMMUNITY-07`
- OpenSSF: `basics_contribution`

### R-COMMUNITY-09 — SUGGESTED
- Status: pass
- Evidence: `GOVERNANCE.md`.
- Source: https://chaoss.community/kb/metrics-model-oss-project-viability-governance/

### R-COMMUNITY-10 — SUGGESTED
- Status: pass
- Evidence: `CONTRIBUTING.md` documents DCO via `git commit -s`.
- Source: https://developercertificate.org/

### R-README-01 — MUST
- Status: pass
- Evidence: `README.md` first two lines: Agentic OS layer for Fedora; one repo, zero snowflakes.
- Source: https://github.com/RichardLitt/standard-readme
- OpenSSF: `basics_documentation_basics`

### R-README-02 — MUST
- Status: pass
- Evidence: paste-ready `dnf` + `cargo build --release` + `sy apply`.
- Source: same
- OpenSSF: `installation_common`

### R-README-03 — MUST
- Status: pass
- Evidence: intro fence plus CLI cheat-sheet.
- Source: same
- OpenSSF: `basics_documentation_interface`

### R-README-04 — SHOULD
- Status: pass
- Evidence: Maintainers section names `@dmytrogajewski`.
- Source: https://github.com/RichardLitt/standard-readme
- OpenSSF: `basics_documentation`

### R-README-05 — SHOULD
- Status: pass
- Evidence: Contributing section links `CONTRIBUTING.md`.
- Source: same
- OpenSSF: `basics_contribution`

### R-README-06 — SHOULD
- Status: pass
- Evidence: Licence section names MIT and links `LICENSE`.
- Source: same
- OpenSSF: `basics_license`

### R-README-07 — SUGGESTED
- Status: pass
- Evidence: no vanity badges.

### R-DIATAXIS-01 — MUST
- Status: pass
- Evidence: `docs/tutorials/getting-started.md`, `docs/tutorials/search-your-files.md`, `docs/tutorials/drive-sy-from-an-agent.md`, `docs/tutorials/syauth-setup.md`. Each follows Prerequisites → Steps → Verify → Next steps. `docs/intro.md` is a Diátaxis start-here map (links only).
- Source: https://diataxis.fr/tutorials/
- OpenSSF: `basics_documentation`, `installation_common`
- Fix shape: closed

### R-DIATAXIS-02 — MUST
- Status: pass
- Evidence: `docs/how-to/` includes `set-up-npu.md` (resolves the getting-started link), `add-a-knowledge-source.md`, `wire-mcp-into-agents.md`, `serve-a-model-on-spark.md`, `apply-a-theme.md`, `run-doctor.md`, `read-power-status.md`, plus the pre-existing syauth / file / plugin how-tos. Each has one Goal and one Result. Exit-code lookup tables live in `docs/reference/cli.md`, not in the how-tos.
- Source: https://diataxis.fr/how-to-guides/
- OpenSSF: `basics_documentation`
- Fix shape: closed

### R-DIATAXIS-03 — MUST
- Status: pass
- Evidence: `docs/reference/cli.md`, `docs/reference/config.md`, `docs/reference/spark.md`, `docs/reference/glossary.md`, plus syauth PAM and sy-file reference pages.
- Source: https://diataxis.fr/reference/
- OpenSSF: `basics_documentation_interface`
- Fix shape: closed

### R-DIATAXIS-04 — SHOULD
- Status: pass
- Evidence: `docs/explanation/architecture.md`, `docs/explanation/no-snowflakes.md`, `docs/explanation/agent-first-cli.md`, `docs/explanation/why-npu-not-gpu.md`.
- Source: https://diataxis.fr/explanation/
- OpenSSF: `basics_documentation`
- Fix shape: closed

### R-DIATAXIS-05 — MUST
- Status: pass
- Evidence: `docs/intro.md` is a start-here map (no steps, no field tables, no mental-model essay). How-tos `run-doctor.md` and `read-power-status.md` point at the CLI reference for exit codes instead of embedding lookup tables. `docs/syauth-setup.md` remains a stub that points at split tutorial / how-to / reference.
- Source: https://diataxis.fr/start-here/
- OpenSSF: `basics_documentation`

### R-STYLE-01 — SHOULD
- Status: pass
- Evidence: user-facing `docs/` prose uses second person and active voice.
- Source: https://developers.google.com/style/highlights
- OpenSSF: `basics_documentation`

### R-STYLE-02 — SHOULD
- Status: pass
- Evidence: headings in `docs/tutorials/` and `docs/how-to/` use sentence case.
- Source: https://developers.google.com/style/headings
- OpenSSF: `basics_documentation`

### R-STYLE-03 — SHOULD
- Status: pass
- Evidence: no militaristic or ableist language in `docs/`.
- Source: https://learn.microsoft.com/en-us/style-guide/bias-free-communication
- OpenSSF: `code_of_conduct` (inclusive language adjacent)

### R-STYLE-04 — SHOULD
- Status: pass
- Evidence: common words in user-facing `docs/` (`use`, `fix`, `run`).
- Source: https://learn.microsoft.com/en-us/style-guide/top-10-tips-style-voice
- OpenSSF: `basics_documentation`

### R-STYLE-05 — SUGGESTED
- Status: pass
- Evidence: `docs/reference/glossary.md` includes `spark`, `file (plane)`, `mon`, and `doctor`, alphabetised.

### R-RELEASE-01 — MUST
- Status: pass
- Evidence: `CHANGELOG.md` Keep a Changelog 1.1.0 with `[Unreleased]`.
- Source: https://keepachangelog.com/en/1.1.0/
- OpenSSF: `release_notes`

### R-RELEASE-02 — MUST
- Status: pass
- Evidence: SemVer policy in `CONTRIBUTING.md`.
- Source: https://semver.org/
- OpenSSF: `release_notes`

### R-RELEASE-03 — SHOULD
- Status: pass
- Evidence: Conventional Commits policy in `CONTRIBUTING.md`.
- Source: https://www.conventionalcommits.org/
- OpenSSF: `basics_repo_interim`

### R-RELEASE-04 — SHOULD
- Status: n/a
- Evidence: no tagged release yet; `docs/release-notes/` stays empty until a version is cut.
- OpenSSF: `release_notes` (deferred until a tag exists)

### R-ADR-01 — SHOULD
- Status: pass
- Evidence: `docs/adr/0001-use-adrs.md` through `0004-publish-policy.md`.
- Source: https://adr.github.io/madr/
- OpenSSF: `basics_documentation`

### R-ADR-02 — SHOULD
- Status: pass
- Evidence: ADRs carry Status, Context, Decision, Consequences (MADR).
- Source: same
- OpenSSF: `basics_documentation`

### R-ADR-03 — SUGGESTED
- Status: pass
- Evidence: none superseded yet.

### R-CI-01 — SHOULD
- Status: pass
- Evidence: `.github/workflows/docs.yml` markdownlint job.
- Source: https://www.writethedocs.org/guide/docs-as-code/
- OpenSSF: `quality_build_status`

### R-CI-02 — SHOULD
- Status: pass
- Evidence: Vale job, advisory.
- Source: https://vale.sh/
- OpenSSF: `quality_build_status`

### R-CI-03 — SHOULD
- Status: pass
- Evidence: cspell job.
- Source: https://cspell.org/
- OpenSSF: `quality_build_status`

### R-CI-04 — SHOULD
- Status: pass
- Evidence: lychee job.
- Source: https://github.com/lycheeverse/lychee
- OpenSSF: `quality_build_status`

### R-LLMS-01 — SHOULD
- Status: pass
- Evidence: `llms.txt` at repo root lists the Diátaxis tree.
- Source: https://llmstxt.org/
- OpenSSF: `basics_documentation`

### R-LLMS-02 — SUGGESTED
- Status: pass
- Evidence: `llms-full.txt` concatenates every path in `llms.txt` (Docs + Optional) with `# <path>` headers and shifted heading levels.

### R-COMPLY-01 — SHOULD
- Status: pass
- Evidence: this file maps every MUST/SHOULD row to an OpenSSF passing-criteria clause; `docs/compliance/openssf-best-practices.md` is the maintainer-facing table.
- Source: https://www.bestpractices.dev/en/criteria/0
- OpenSSF: this row

### R-COMPLY-02 — SUGGESTED
- Status: pass
- Evidence: `THIRD_PARTY_NOTICES.md` inventories Good Docs Project templates,
  Contributor Covenant, and linked external standards; `CONTRIBUTING.md`
  points contributors to it.

### R-ECO-01 — SHOULD
- Status: pass
- Evidence: `RUSTDOCFLAGS="-D warnings -D missing_docs" cargo doc --no-deps --workspace` exits 0. Public items across the workspace, including the previously missing constants, fields, variants, and methods, now carry rustdoc comments.
- Source: https://doc.rust-lang.org/rustdoc/what-to-include.html
- OpenSSF: `analysis_static_analysis` (docs as a compile-time gate)

### R-ECO-02 — SHOULD
- Status: pass
- Evidence: every workspace library crate root and `src/main.rs` carry an inner `//!` purpose comment (paths listed under `R-ECO-01`).
- Source: https://doc.rust-lang.org/reference/comments.html
- OpenSSF: `basics_documentation`

### R-ECO-03 — SHOULD
- Status: pass
- Evidence: `cargo test --doc --workspace` exits 0 (two ignored examples in `sy-plugin-pdk`, zero failures).
- Source: https://doc.rust-lang.org/rustdoc/documentation-tests.html
- OpenSSF: `test`

### R-ECO-04 — SUGGESTED
- Status: n/a
- Evidence: every workspace package sets `publish = false` (root `Cargo.toml` and ADR 0004). `docs.rs` will not build these crates, so `[package.metadata.docs.rs]` would have no effect.

### R-ECO-05 — SUGGESTED
- Status: pass
- Evidence: root `Cargo.toml` `[package] readme = "README.md"` (declared even though `publish = false`, per ADR 0004).

## Audit log
- [seq:1] read AGENTS.md
- [seq:2] read README.md
- [seq:3] read docs/ tree
- [seq:4] read .github/ (docs.yml, docs-site.yml, issue and PR templates)
- [seq:5] read CHANGELOG.md, LICENSE, Cargo.toml, llms.txt
- [seq:6] previous pass of this file scored five Diátaxis/style gaps
- [seq:17] `docs/intro.md` slimmed to a start-here map
- [seq:18] exit-code tables removed from `docs/how-to/run-doctor.md` and `docs/how-to/read-power-status.md`
- [seq:19] `llms-full.txt` regenerated from `llms.txt` (27 files)
- [seq:20] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --workspace` scored pass
- [seq:21] `cargo test --doc --workspace` scored pass
- [seq:22] MUST/SHOULD rows annotated with OpenSSF clauses
- [seq:23] in-place edits recorded as user-authorized
- [seq:24] R-DIATAXIS-01..05, R-STYLE-05, R-LLMS-02, R-ECO-01..03, R-ECO-05 rescored pass; R-ECO-04 n/a
- [seq:25] re-read Sparkplane bridge, integration, reference, community, and CI docs
- [seq:26] verified `sy.toml` pins Sparkplane `0.1.5` and checked the local Markdown link inventory
- [seq:27] ran `make docs-lint` successfully; optional local linters were unavailable and skipped by the target
- [seq:28] documented per-launch Sparkplane network opt-in in README, how-to, CLI/reference, and changelog surfaces
- [seq:29] renamed the syauth compatibility page from a stub to a documentation index
- [seq:30] replaced the OpenSSF mapping's intermittently forbidden opensource.org URL with the reachable SPDX MIT record
- [seq:31] reran `make docs-lint`: 419 links OK, 0 errors; markdownlint, cspell, and Vale remain optional local skips
- [seq:32] ran the Docusaurus production build successfully with `npm run build` in `website/`
- [seq:33] ran strict rustdoc with `-D missing_docs`; five public constants failed documentation coverage
- [seq:34] corrected `R-COMPLY-02` and `R-ECO-01` from pass to gap
- [seq:35] added public-item rustdoc comments across the workspace and reran strict rustdoc successfully
- [seq:36] added `THIRD_PARTY_NOTICES.md` and linked it from `CONTRIBUTING.md`
- [seq:37] regenerated README preview goldens after the documented README update
- [seq:38] `make lint` passed
- [seq:39] `make test` passed
- [seq:40] reran docs link checks successfully
- [seq:41] strict rustdoc passed after formatting; `cargo fmt --all -- --check` and `make lint` passed

## Final Audit Summary
- Scope: full
- Rows evaluated: 47
- pass: 45
- gap: 0
- n/a: 2 (`R-RELEASE-04`, `R-ECO-04`)
- Plan: `specs/docs-audit/PLAN-full.md`
