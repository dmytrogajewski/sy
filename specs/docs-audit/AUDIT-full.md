# Full documentation audit

## Scope and method

This audit checks repository documentation against the documenter rubric and current repository evidence. It covers the root entry points, user documentation, community files, package/build scripts, CI, Rust manifests, and the prior archived audit. Generated dependencies under `website/node_modules` are excluded.

Scores: **pass**, **gap**, or **n/a**. A pass means the artefact exists and substantially meets the rubric; it does not mean every factual claim in that artefact is current. Factual drift is called out explicitly because it is the primary documentation risk.

## Findings

### Community health

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-COMMUNITY-01 | pass | MUST | `README.md` explains what, why, RPM installation, and basic usage; the feature split matches `packaging/sy.spec`. | Keep the overview synchronized with package metadata. |
| R-COMMUNITY-02 | pass | MUST | Root `LICENSE` identifies the MIT License. | None. |
| R-COMMUNITY-03 | pass | MUST | Root `CODE_OF_CONDUCT.md` is present and points to the Contributor Covenant. | Verify enforcement contact stays current during governance review. |
| R-COMMUNITY-04 | pass | MUST | `CONTRIBUTING.md` gives issue, change, tests, lint, DCO, and SemVer instructions and links to existing templates and governance. | Recheck when contribution policy changes. |
| R-COMMUNITY-05 | pass | MUST | `SECURITY.md` provides a private vulnerability disclosure channel. | None. |
| R-COMMUNITY-06 | pass | SHOULD | `SUPPORT.md` routes usage questions to public support channels. | Recheck channel availability during cleanup. |
| R-COMMUNITY-07 | pass | SHOULD | `.github/ISSUE_TEMPLATE/bug_report.md` and `feature_request.md` exist. | None. |
| R-COMMUNITY-08 | pass | SHOULD | `.github/PULL_REQUEST_TEMPLATE.md` exists. | None. |
| R-COMMUNITY-09 | pass | SUGGESTED | `GOVERNANCE.md` describes maintainer authority and decision handling. | None. |
| R-COMMUNITY-10 | pass | SUGGESTED | `CONTRIBUTING.md` asks contributors to sign commits with `-s` for DCO. | Correct surrounding stale references. |

### README

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-README-01 | pass | MUST | The lede describes a modular Fedora desktop and agent environment with a base launcher and optional feature RPMs. | Keep wording aligned with the spec and build scripts. |
| R-README-02 | pass | MUST | `README.md` documents `make rpm`, Fedora selectors, and package installation behavior. | Keep exact package/install instructions aligned with `scripts/build-rpms.sh` and `scripts/reinstall-rpms.sh`. |
| R-README-03 | pass | MUST | README gives a package installation path and examples for power and AI commands. | None. |
| R-README-04 | pass | SHOULD | README names Dmitriy Gajewski and links to governance. | Update when ownership changes. |
| R-README-05 | pass | SHOULD | README links to contributor instructions. | None. |
| R-README-06 | pass | SHOULD | README names the MIT license and links to `LICENSE`. | None. |
| R-README-07 | n/a | SUGGESTED | No badge block is present. | No vanity badges to remove. |

### Diátaxis

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-DIATAXIS-01 | pass | MUST | `docs/tutorials/getting-started.md` now follows RPM build, install, apply, and verification steps. | Continue validating specialist tutorials. |
| R-DIATAXIS-02 | pass | MUST | `docs/how-to/` has focused tasks for NPU, themes, knowledge, Spark, and troubleshooting. Feature guides name their required RPMs and document known plugin/preview limits. | Recheck prerequisites when package ownership changes. |
| R-DIATAXIS-03 | pass | MUST | `docs/reference/` contains CLI, config, glossary, Spark, file-manager, and PAM references. | Update references only where contracts changed; avoid re-documenting rustdoc output. |
| R-DIATAXIS-04 | pass | SHOULD | `docs/explanation/architecture.md` and `what-sy-is.md` explain the launcher, optional packages, and separate service processes. | Keep specialist details in relevant how-to/reference pages. |
| R-DIATAXIS-05 | pass | MUST | README is an entry point with a package overview and links to tutorials, reference, and architecture docs. | None. |

### Style

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-STYLE-01 | gap | SHOULD | Prose varies in voice and contains long, dense paragraphs; stale boilerplate appears in `CONTRIBUTING.md`. | Apply active voice and direct instructions as pages are corrected. |
| R-STYLE-02 | gap | SHOULD | Heading capitalization varies; e.g. `README.md` uses “Install / apply” while other pages use title-like headings. | Standardize sentence case as touched. |
| R-STYLE-03 | pass | SHOULD | No material gendered or ableist language was found in primary user-facing docs reviewed. | Keep inclusive-language review in docs CI/manual review. |
| R-STYLE-04 | gap | SHOULD | Several pages use project jargon and long compound explanations without a first-use definition. | Explain terms on first use; link to `docs/reference/glossary.md`. |
| R-STYLE-05 | pass | SUGGESTED | `docs/reference/glossary.md` defines project terms. | None. |

### Releases

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-RELEASE-01 | pass with review needed | MUST | `CHANGELOG.md` has an Unreleased section, one heading per change type, and an Unreleased comparison link. Existing entries still need verification against release history before a version is cut. | Review entries during release preparation and add only real version links. |
| R-RELEASE-02 | pass | MUST | `CONTRIBUTING.md` documents SemVer and defines compatibility handling before 1.0.0. | Revisit when the project reaches 1.0.0. |
| R-RELEASE-03 | pass | SHOULD | `CONTRIBUTING.md` documents Conventional Commit subjects and breaking-change footers. Changelog edits remain manual. | Keep the distinction between commit policy and changelog generation clear. |
| R-RELEASE-04 | gap | SHOULD | No `docs/release-notes/` directory or per-release upgrade notes found. | Add release notes when a release is cut, including Fedora/RPM and NPU compatibility notes. |

### ADRs

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-ADR-01 | pass | SHOULD | `docs/adr/` contains five decisions, including the feature-RPM boundary, workspace architecture, and ADR policy. | Add decisions when architectural trade-offs need a durable record. |
| R-ADR-02 | pass | SHOULD | The five ADRs reviewed use status/context/decision/consequences sections. | None. |
| R-ADR-03 | pass | SUGGESTED | No superseded ADR relationship was evident among current records. | Link superseding decisions if one is later replaced. |

### Docs CI

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-CI-01 | pass | SHOULD | `.github/workflows/docs.yml` runs markdownlint and verifies `llms-full.txt` freshness on documentation changes. | Keep action globs excluding generated output. |
| R-CI-02 | pass | SHOULD | Docs workflow runs Vale in advisory mode. | None. |
| R-CI-03 | pass | SHOULD | Docs workflow runs cspell with repository config. | None. |
| R-CI-04 | pass | SHOULD | Docs workflow runs lychee link checks. | None. |

### LLM consumers

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-LLMS-01 | pass | SHOULD | `llms.txt` describes the current package model and links to canonical docs. | Update the index when canonical docs move. |
| R-LLMS-02 | pass | SUGGESTED | `llms-full.txt` is generated from README and Markdown links in `llms.txt` by `scripts/generate-llms-full.py`. | Run the generator after doc changes and add a CI freshness check under DOC-10. |

### Compliance

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-COMPLY-01 | pass | SHOULD | `docs/compliance/openssf-best-practices.md` maps practices to current package/build evidence and records missing automation as a gap. | Revalidate evidence when CI or release processes change. |
| R-COMPLY-02 | pass with review needed | SUGGESTED | `THIRD_PARTY_NOTICES.md` exists and contributor guidance points to it. | Confirm notices cover all copied templates/snippets named by docs. |

### Rust documentation

| ID | Score | Severity | Evidence and finding | Fix |
|---|---|---|---|---|
| R-ECO-01 | not fully audited | SHOULD | Workspace has multiple crates; exhaustive public-item coverage requires a source/rustdoc audit, beyond Markdown review. | Use `cargo doc` warnings and targeted crate review; track separately from prose corrections. |
| R-ECO-02 | not fully audited | SHOULD | Crate-root inner docs are not exhaustively checked here. | Audit crate roots as a code-documentation task. |
| R-ECO-03 | pass gate, coverage unknown | SHOULD | Docs CI runs rustdoc/doctest checks when Rust changes; that proves compilation, not meaningful examples. | Review examples for public workflows while auditing APIs. |
| R-ECO-04 | gap | SUGGESTED | Root package manifest has no `[package.metadata.docs.rs]`; repository is a multi-binary OS layer and root package is not published. | Mark not applicable in policy or define canonical published crate docs. |
| R-ECO-05 | pass | SUGGESTED | Root `Cargo.toml` has a `readme` field. | None. |

## Cross-document factual drift

These contradictions are the highest-risk findings because readers can follow a documented command or architecture that no longer describes the project:

1. The original README and linked narrative docs mixed a multi-package RPM model with a single-binary explanation. The README, intro, architecture, and product story now describe the package split.
2. The original LLM bundle and installation tutorial repeated that obsolete architecture. `llms.txt` now describes the package model and the full-text mirror is generated from the index.
3. Contributor instructions referred to templates and governance as absent; those references now point to the existing files.
4. The OpenSSF map had no Rust test CI and relied on outdated build/install commands. It now marks the CI gap and links to the RPM path.
5. The project config says Fedora 43 in some low-level comments. Those may describe a minimum kernel/platform assumption; update them only after confirming the technical constraint, and distinguish minimum support from the RPM build matrix.

The old audit under `specs/archive/docs-audit/` is historical and was not used as evidence for current state.

## Summary

The package/install story and specialist user guides have been reconciled, and docs CI checks the generated LLM mirror. Historical changelog entries still need verification before a release; Rust public API coverage remains a separate source-level review. External link checks may time out on third-party hosts, and local Markdown linters must be installed to reproduce their CI gates.
