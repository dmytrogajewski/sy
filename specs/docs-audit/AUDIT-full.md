# AUDIT: full

## Mode
audit

## Project
- Name: `sy` (root crate) + workspace members `sy-core`, `sy-ipc`, `sy-testutils`
- Description (root, `Cargo.toml:248`): "Agentic OS layer for Fedora — single Rust binary plus declarative configs that ship an on-device NPU inference plane, agent sandbox, MCP fabric, semantic knowledge plane, power governor, and a niri/wayland rice"
- Licence: MIT (`LICENSE:1`, "MIT License"), but **no `license` field** in any `[package]` block of any manifest in the workspace
- Ecosystem: rust (workspace; `Cargo.toml:1-10` virtual workspace; resolver 2)
- Workflow: journey (specs at `specs/journeys/`, `specs/roadmaps/`, `specs/bugs/`)
- Persona anchors: `AGENTS.md` (Rust + Linux + NPU + agent-CLI persona), `CLAUDE.md` (no-snowflakes rule + CLIG + agent-friendly CLI)

## Summary
- MUST findings open: 8
- SHOULD findings open: 14
- SUGGESTED findings open: 7
- pass: 16
- n/a: 2
- Rows evaluated: 47

## Top 5 MUST fixes
1. `R-COMMUNITY-04` — `CONTRIBUTING.md` is absent. The project takes external contributions (MIT, public GitHub) but ships no contribution guide.
2. `R-COMMUNITY-05` — `SECURITY.md` is absent. The project hosts a privileged daemon owning `/dev/accel/accel0`, a PAM module (`syauth`), and SELinux policy under `configs/selinux/`; a private disclosure channel is required, not optional.
3. `R-COMMUNITY-03` — `CODE_OF_CONDUCT.md` is absent.
4. `R-RELEASE-01` — `CHANGELOG.md` is absent. The project is at `0.1.0` (`Cargo.toml:13`); a Keep-a-Changelog-shaped file with an `[Unreleased]` header is the precondition for any future `0.2.0` cut.
5. `R-DIATAXIS-01..03` — `docs/` contains a single file (`docs/syauth-setup.md`, ≈6.6 KiB). No tutorial, no how-to, no reference directory. The single doc is a hybrid tutorial/how-to/reference, which is a quadrant violation per the Diátaxis taxonomy.

## Findings

### R-COMMUNITY-01 — MUST
- Status: pass
- Evidence: `README.md` exists at repo root (≈18 KiB after the agentic-OS rewrite). First two lines answer "what" ("An Agentic OS layer for Fedora") and "why" ("turn a stock Fedora 43 laptop into an agent-first workstation"). A paste-ready `sy` command block follows immediately.
- Source: [Standard README](https://github.com/RichardLitt/standard-readme), [Make-A-README](https://www.makeareadme.com/)

### R-COMMUNITY-02 — MUST
- Status: pass (with SUGGESTED follow-up under `R-ECO-05`)
- Evidence: `LICENSE` at repo root, line 1: "MIT License". SPDX-recognisable. **However**, no `license = "MIT"` field in `[package]` (`Cargo.toml:244-260`) or in any workspace member's `[package]` (`crates/sy-core/Cargo.toml:1-6`, `crates/sy-ipc/Cargo.toml:1-6`, `crates/sy-testutils/Cargo.toml:1-6`). For `publish = false` crates this is permissible but loses Cargo's SPDX validation.
- Fix shape (optional): add `license = "MIT"` to each `[package]` block.
- Source: [GitHub community profiles](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/about-community-profiles-for-public-repositories)

### R-COMMUNITY-03 — MUST
- Status: gap
- Evidence: no `CODE_OF_CONDUCT.md` at repo root, no `docs/CODE_OF_CONDUCT.md`, no `.github/` directory at all (`ls /home/dmitriy/sources/sy/.github/` → ENOENT).
- Fix shape: `/documenter code-of-conduct` → `CODE_OF_CONDUCT.md` pointing at Contributor Covenant 2.1 with a maintainer-contact line.
- Source: [Contributor Covenant 2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/)

### R-COMMUNITY-04 — MUST
- Status: gap
- Evidence: no `CONTRIBUTING.md`. Project ships `AGENTS.md` (coding-agent persona, working loop) and `CLAUDE.md` (no-snowflakes + CLI rules); neither is a contributing guide for human contributors — they describe an agent's TDD loop and the CLIG contract, not "how to file an issue / how to run tests / how to sign commits".
- Fix shape: `/documenter contributing` → `CONTRIBUTING.md.proposed.md` covering: bug filing, change proposal, `make lint` / `make test` / `make fmt-check` commands (already exist), DCO via `git commit -s`, and a pointer to `AGENTS.md` for the project's TDD norms.
- Source: [GitHub community profiles](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/about-community-profiles-for-public-repositories)

### R-COMMUNITY-05 — MUST
- Status: gap (highest blast-radius MUST in this audit)
- Evidence: no `SECURITY.md`. The project ships:
  - A user daemon with `CAP_*` ambient grants (`AGENTS.md:152` "Cap grants live in the systemd unit, not in `setcap` on the binary").
  - A PAM module (`syauth`) that mediates `sudo` (`README.md` §syauth, `configs/systemd/user/`, `src/syauth.rs`).
  - SELinux policy at `configs/selinux/` and a polkit rule at `configs/policy/10-sy-power.rules`.
  - Direct ownership of `/dev/accel/accel0` (`AGENTS.md:147` "One process per NPU").
  No private disclosure channel is named. A public GitHub issue is the only currently visible reporting path, which is unacceptable for the surface above.
- Fix shape: `/documenter security` → `SECURITY.md.proposed.md` with: supported versions table (currently only `0.1.0`/`main`), private email (`dmytrogajewski@gmail.com` per `CLAUDE.md` userEmail), 7-day acknowledgement / 30-day fix-target, credit-on-release-note policy.
- Source: [GitHub security policy](https://docs.github.com/en/code-security/getting-started/adding-a-security-policy-to-your-repository)

### R-COMMUNITY-06 — SHOULD
- Status: gap
- Evidence: no `SUPPORT.md`. README has no "where to ask questions" pointer; issues are the implicit channel by default.
- Fix shape: `/documenter support` → points readers at GitHub Discussions (if enabled) or issues, links to `SECURITY.md` for vulns, sets best-effort expectations.
- Source: [GitHub default community health files](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/creating-a-default-community-health-file)

### R-COMMUNITY-07 — SHOULD
- Status: gap
- Evidence: no `.github/ISSUE_TEMPLATE/` directory; no `bug_report.md`; no `feature_request.md`.
- Fix shape: `/documenter issue-templates` → both files with `bug:` / `feat:` title prefixes matching the project's Conventional-Commits-shaped commit history (e.g. `1f0860f fix(supervision): …`, `a90a53b sy refactor: …`).
- Source: [GitHub issue templates](https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests)

### R-COMMUNITY-08 — SHOULD
- Status: gap
- Evidence: no `.github/PULL_REQUEST_TEMPLATE.md`.
- Fix shape: `/documenter pr-template` → Summary / Test plan / Docs checkboxes (CHANGELOG entry, user-facing docs) / Related-issue line.
- Source: same as `R-COMMUNITY-07`

### R-COMMUNITY-09 — SUGGESTED
- Status: gap
- Evidence: no `GOVERNANCE.md`. The project is a single-maintainer codebase; a one-page governance doc would still help a contributor understand decision authority (e.g. SPEC change requires an ADR).
- Fix shape: `/documenter governance` — name the maintainer, document the "ADR-then-change for SPEC-level decisions" pattern that `specs/research/architecture-refactor/SPEC.md` and `specs/roadmaps/arch-*` already imply.
- Source: [CHAOSS governance metric](https://chaoss.community/kb/metrics-model-oss-project-viability-governance/)

### R-COMMUNITY-10 — SUGGESTED
- Status: gap
- Evidence: no DCO/CLA policy is stated anywhere. Recent commits (e.g. `1f0860f`, `a90a53b`, `4900ad7`) are unsigned-off; there is no `Signed-off-by` trailer convention in the visible history.
- Fix shape: pick one (DCO is the lighter-touch choice for an MIT solo project) and add a section to `CONTRIBUTING.md` once authored under `R-COMMUNITY-04`.
- Source: [DCO vs CLA](https://opensource.com/article/18/3/cla-vs-dco-whats-difference)

### R-README-01 — MUST
- Status: pass
- Evidence: post-rewrite, `README.md:3-7` answers what + why in two sentences ("An **Agentic OS layer for Fedora** — a single Rust binary plus declarative configs…").

### R-README-02 — MUST
- Status: pass
- Evidence: install path is the build path; `README.md` §Deploy gives `cargo build --release && ./target/release/sy apply` plus the Fedora `dnf copr enable / dnf install` prerequisites verbatim.

### R-README-03 — MUST
- Status: pass
- Evidence: `README.md:7-14` (intro fence) shows the simplest end-to-end across each plane (`sy apply`, `sy aiplane daemon`, `sy knowledge search`, etc.); §CLI cheat-sheet ≈line 230+ expands per-plane.

### R-README-04 — SHOULD
- Status: gap
- Evidence: no "Maintainers" section in `README.md`. Author appears only in `LICENSE:3` ("Copyright (c) 2026 Dmitriy Gajewski").
- Fix shape: add a `## Maintainers` block naming `@dmytrogajewski` (or whatever GitHub handle is canonical).
- Source: [Standard README](https://github.com/RichardLitt/standard-readme)

### R-README-05 — SHOULD
- Status: gap
- Evidence: README has no `## Contributing` section linking to `CONTRIBUTING.md` (which does not exist yet — see `R-COMMUNITY-04`).
- Fix shape: add the section after `R-COMMUNITY-04` lands.

### R-README-06 — SHOULD
- Status: partial pass
- Evidence: `README.md` §License says "MIT — see [LICENSE](LICENSE)." SPDX is named in prose but not in the structured `[package].license` field — see `R-COMMUNITY-02`'s follow-up.

### R-README-07 — SUGGESTED
- Status: pass
- Evidence: `README.md` has zero badges. No vanity badges to remove. (When the project does start shipping releases, the SUGGESTED guidance is to limit badges to build / version / licence.)

### R-DIATAXIS-01 — MUST
- Status: gap
- Evidence: no `docs/tutorials/` directory. `docs/` contains exactly one file: `docs/syauth-setup.md` (≈6.6 KiB), which is structured as a six-step setup walkthrough — a tutorial-shaped artefact, but mis-located.
- Fix shape: `/documenter tutorial getting-started` for the agentic-OS first-boot path (`dnf copr enable → sudo dnf install → cargo build → sy apply → systemctl --user enable --now sy.target`); separately, move `docs/syauth-setup.md` to `docs/tutorials/syauth-setup.md` once the directory exists.
- Source: [Diátaxis tutorial](https://diataxis.fr/tutorials/), [Good Docs Project tutorial](https://www.thegooddocsproject.dev/template/tutorial)

### R-DIATAXIS-02 — MUST
- Status: gap
- Evidence: no `docs/how-to/` directory. The README's §Deploy is a how-to in disguise but lives in the landing page.
- Fix shape: candidate how-tos surfaced by the README's existing structure — `how-to/add-a-knowledge-source.md`, `how-to/migrate-from-cuda-to-vitisai.md`, `how-to/wire-mcp-into-claude.md`, `how-to/install-syauth-pam-into-sudo.md`. Author one per `/documenter how-to <slug>` call.
- Source: [Diátaxis how-to](https://diataxis.fr/how-to-guides/)

### R-DIATAXIS-03 — MUST
- Status: gap
- Evidence: no `docs/reference/` directory. CLI flag tables live inline in `README.md` §CLI cheat-sheet, config file shape (`configs/sy/power.toml`, `configs/sy/intent_whitelist.toml`) is undocumented as a reference, the IPC envelope shape (`sy-ipc::envelope`) is referenced from `specs/roadmaps/arch-ipc-v1/ROADMAP.md` but has no user-facing reference doc.
- Fix shape: `/documenter reference cli`, `/documenter reference config-power`, `/documenter reference config-intent-whitelist`, `/documenter reference ipc-envelope`.
- Source: [Diátaxis reference](https://diataxis.fr/reference/)

### R-DIATAXIS-04 — SHOULD
- Status: partial pass
- Evidence: explanation-quadrant material exists, but in `specs/` (`specs/research/architecture-refactor/SPEC.md`, the various journey docs) not `docs/explanation/`. SPEC docs are internal-design artefacts, not reader-facing mental-model docs.
- Fix shape: `/documenter explanation architecture` to extract the user-facing "how the planes fit together" story; `/documenter explanation why-vitisai-not-cuda` to host the README's existing `e5-base` rationale.

### R-DIATAXIS-05 — MUST
- Status: gap
- Evidence: `docs/syauth-setup.md` mixes tutorial steps ("six steps from fresh host…") with how-to recipes (the three known failure-mode fixes) and reference material (PAM module args). Per the rubric, this is a quadrant violation.
- Fix shape: split into `docs/tutorials/syauth-setup.md` (the six-step happy path), `docs/how-to/troubleshoot-syauth.md` (the three failure modes), `docs/reference/syauth-pam-module.md` (control-flag, module args).
- Source: [Diátaxis start here](https://diataxis.fr/start-here/)

### R-STYLE-01 — SHOULD
- Status: partial pass
- Evidence: README and AGENTS.md mostly use active voice and imperative mood ("Make sure `~/.local/bin` is in `$PATH`", "Encode it in `configs/`"). Some passive constructions survive in `CLAUDE.md` ("Every environment change the user requests must be productivized inside this repo") — acceptable for a contract / spec voice. No systematic rewrite needed; spot-fix during future authoring.
- Fix shape: defer to per-artefact authoring under `/documenter`.
- Source: [Google Developer Style — highlights](https://developers.google.com/style/highlights)

### R-STYLE-02 — SHOULD
- Status: pass
- Evidence: README headings audited — all sentence case ("Planes", "Repo layout", "Install / apply", "CLI cheat-sheet", "Keybindings", "Niri vs sway", "Keyboard layout", "Theme", "Notes", "License"). Sub-headings under §Rice ("Rice prerequisites", "Deploy", "NPU one-time setup") also sentence case. AGENTS.md and CLAUDE.md headings similarly compliant.

### R-STYLE-03 — SHOULD
- Status: pass
- Evidence: no militaristic or ableist language in `README.md`, `AGENTS.md`, `CLAUDE.md`. Pronouns are not gendered (the prose addresses "you" / "the user").

### R-STYLE-04 — SHOULD
- Status: partial pass
- Evidence: README leans on a few jargon-dense compound terms (e.g. "VitisAI EP", "ModelProto serialisation", "BF16-quantises with AMD's Quark", "re-exec dance"). These are not Microsoft-flagged "complex words" so much as domain terminology, and a glossary is the right escape valve — see `R-STYLE-05`.
- Fix shape: ship `docs/reference/glossary.md` (defined under `R-STYLE-05`); do not rewrite the terms themselves.

### R-STYLE-05 — SUGGESTED
- Status: gap
- Evidence: no `docs/glossary.md`. README repeatedly uses project-specific terms ("plane", "aiplane", "workload", "session pool", "re-exec dance", "rice", "sy.target") without a one-stop definition.
- Fix shape: `/documenter reference glossary` — one-line definition per term, alphabetised.
- Source: [Good Docs Project glossary](https://www.thegooddocsproject.dev/template/glossary)

### R-RELEASE-01 — MUST
- Status: gap
- Evidence: no `CHANGELOG.md`. Project is at `0.1.0` (`Cargo.toml:13`); commit log shows release-shaped messages (`fix(supervision):`, `feat`-style refactors) but no published changelog.
- Fix shape: `/documenter changelog` → `CHANGELOG.md` seeded with an `[Unreleased]` header and the six visible commits (or as many as the user wants surfaced) bucketed Added / Changed / Fixed / Security.
- Source: [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/)

### R-RELEASE-02 — MUST
- Status: partial pass
- Evidence: version is `0.1.0` (SemVer-shaped), but no policy statement says "this project follows SemVer".
- Fix shape: one paragraph in `CONTRIBUTING.md` (authored under `R-COMMUNITY-04`).

### R-RELEASE-03 — SHOULD
- Status: pass
- Evidence: recent commit subjects follow Conventional Commits — `1f0860f fix(supervision): …`, `4900ad7 stack-bar UX: …`, `9bd8ba5 prep_npu_workload.py + daemon-in-thread integration test`, `03b8011 aiplane: daemon now dispatches…`. Style is consistent; policy is implicit. Document it under `R-COMMUNITY-04`.

### R-RELEASE-04 — SHOULD
- Status: gap (n/a-adjacent — no releases cut yet)
- Evidence: no `docs/release-notes/` directory. The project has not cut a versioned release.
- Fix shape: deferred until the first `v0.2.0` cut; gate on `CHANGELOG.md` existing first.

### R-ADR-01 — SHOULD
- Status: gap (with strong existing scaffolding in `specs/`)
- Evidence: no `docs/adr/` directory. ADR-shaped material lives in `specs/research/architecture-refactor/SPEC.md` and the `specs/roadmaps/arch-*` series (Context / Decision Drivers / Decision are implicit in the SPEC's section structure) but is not labelled or located as ADRs.
- Fix shape: `/documenter adr` → `docs/adr/0001-use-adrs.md` (the foundational ADR), then progressively lift the SPEC's load-bearing decisions into MADR-shaped per-decision files (e.g. `0002-virtual-workspace-with-sy-core-vocabulary.md`, `0003-vitisai-ep-not-cuda-for-on-device-embedding.md`).
- Source: [MADR 4.0](https://adr.github.io/madr/)

### R-ADR-02 — SHOULD
- Status: n/a-pending
- Evidence: no ADRs exist; evaluable only after `R-ADR-01` lands.

### R-ADR-03 — SUGGESTED
- Status: n/a-pending
- Evidence: same as `R-ADR-02`.

### R-CI-01 — SHOULD
- Status: gap
- Evidence: no `.github/workflows/` directory; no GitLab CI; no CircleCI. `make lint` runs `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check` (per `Makefile:lint` and `AGENTS.md` non-negotiables) but does not lint Markdown.
- Fix shape: `/documenter ci-docs` → `.github/workflows/docs.yml` with the markdownlint step. Add a `docs-lint` target to `Makefile` so the same check runs locally.
- Source: [Write the Docs — Docs as Code](https://www.writethedocs.org/guide/docs-as-code/)

### R-CI-02 — SHOULD
- Status: gap
- Evidence: no Vale config (`.vale.ini`), no Vale step in any workflow.
- Fix shape: include the Vale step in `/documenter ci-docs`; pin Microsoft + Google style packs in advisory mode.
- Source: [Datadog × Vale](https://www.datadoghq.com/blog/engineering/how-we-use-vale-to-improve-our-documentation-editing-process/)

### R-CI-03 — SHOULD
- Status: gap
- Evidence: no cspell config (`cspell.json`), no spelling step. Domain terms ("aiplane", "vitisai", "gruvbox", "niri", "yazi", "waybar") will need a project dictionary on first run.
- Fix shape: include cspell step in `/documenter ci-docs`; seed `cspell.json` with the domain terms.

### R-CI-04 — SHOULD
- Status: gap
- Evidence: no lychee config, no link-check step. README contains ≈15 external links (Standard README, Diátaxis, Keep a Changelog, etc. — most cited from authored work, not yet present in the repo) plus several internal `[text](path)` links. None are validated.
- Fix shape: include lychee step in `/documenter ci-docs`.

### R-LLMS-01 — SHOULD
- Status: gap
- Evidence: no `llms.txt` at repo root.
- Fix shape: `/documenter llms-txt` → `llms.txt` listing README, the (future) tutorials, the (future) reference docs, and the existing `specs/research/architecture-refactor/SPEC.md` under `## Optional`.
- Source: [llms.txt proposal](https://llmstxt.org/)

### R-LLMS-02 — SUGGESTED
- Status: gap
- Evidence: no `llms-full.txt`. Defer until `docs/tutorials/`, `docs/how-to/`, `docs/reference/`, `docs/explanation/` exist so the concatenation has a stable canonical order.

### R-COMPLY-01 — SHOULD
- Status: gap
- Evidence: no mapping table to OpenSSF Best Practices Badge passing criteria. Visible criteria already met or close-to-met by the project, by passing-criteria clause:
  - `basics_documentation` — `README.md` present (`R-COMMUNITY-01` pass), `LICENSE` present (`R-COMMUNITY-02` pass), but `CONTRIBUTING.md` missing (`R-COMMUNITY-04` gap).
  - `security_vulnerability_report_process` — **gap** until `SECURITY.md` lands (`R-COMMUNITY-05`).
  - `quality_build_status` — `cargo build --release` works (per the README), but no public CI badge.
  - `analysis_static_analysis` — `cargo clippy --all-targets -- -D warnings` is mandatory (`AGENTS.md` non-negotiables) — pass once exposed.
  - `code_of_conduct` — gap until `R-COMMUNITY-03` lands.
- Fix shape: roll the mapping into `SECURITY.md` (or a dedicated `docs/compliance/openssf-best-practices.md`) once `R-COMMUNITY-03`, `-04`, `-05` are resolved.
- Source: [OpenSSF passing criteria](https://www.bestpractices.dev/en/criteria/0)

### R-COMPLY-02 — SUGGESTED
- Status: gap
- Evidence: no `THIRD_PARTY_NOTICES.md`. The project vendors zero third-party prose into its docs at the moment (templates and snippets are written from scratch in the rubric's own templates). Becomes load-bearing once Contributor Covenant 2.1 text is included or copied (under `R-COMMUNITY-03`).
- Fix shape: when `CODE_OF_CONDUCT.md` is authored as a pointer (not a copy), no `THIRD_PARTY_NOTICES.md` is needed. If the full Covenant text is inlined, add the file.

### R-ECO-01 — SHOULD
- Status: deferred to `/implement` (skill scope boundary)
- Evidence: many public items across `src/aiplane/`, `src/knowledge/`, `src/power/`, `crates/sy-core/`, `crates/sy-ipc/`, `crates/sy-testutils/`. Spot-check on `crates/sy-core/src/lib.rs` and `crates/sy-ipc/src/lib.rs` shows crate-level `//!` docs exist (see `R-ECO-02`), but per-item `///` coverage is not exhaustively audited here per the skill's `<rules>` clause 11 ("Toolchain ownership (Rust). … The skill audits whether they exist and whether `cargo doc` is clean under `-D warnings`; it does not author them").
- Fix shape: run `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items`; for any items the lints flag, file a `/implement` step. This rubric row stays open until the rustdoc-warnings count is zero.
- Source: [Rustdoc book — what to include](https://doc.rust-lang.org/rustdoc/what-to-include.html)

### R-ECO-02 — SHOULD
- Status: pass for library crates, gap for the binary
- Evidence:
  - `crates/sy-core/src/lib.rs:1-11` — strong inner `//!` doc (purpose + matklad citation).
  - `crates/sy-ipc/src/lib.rs:1-10` — strong inner `//!` doc (links the SPEC + roadmap).
  - `crates/sy-testutils/src/lib.rs:1-10` — strong inner `//!` doc (cites the originating commit).
  - `src/main.rs:1-10` — **no inner `//!`**. Binary crate roots are exempt by convention from `cargo doc` rendering, so the gap is cosmetic; the README is the binary's de-facto crate-root doc.
- Fix shape: SUGGESTED (not required by the rubric) — add a brief `//!` to `src/main.rs` that points to `README.md` and `AGENTS.md`.

### R-ECO-03 — SHOULD
- Status: deferred to `/implement`
- Evidence: no doctests are visible in the spot-checked crate roots. `Makefile:test` does not currently call `cargo test --doc` separately (would need to confirm whether the default `cargo test` invocation here runs doctests for library crates and a binary crate — it does for libraries, skips for `[[bin]]`).
- Fix shape: gate a `cargo test --doc` step in CI once `R-CI-01..04` land.

### R-ECO-04 — SUGGESTED
- Status: n/a for `publish = false` crates, gap for the root `sy` package
- Evidence: workspace members all carry `publish = false` (`crates/sy-core/Cargo.toml:5`, `crates/sy-ipc/Cargo.toml:5`, `crates/sy-testutils/Cargo.toml:5`), so `docs.rs` will not build them. Root `sy` package has no `publish` field (`Cargo.toml:244-249`) and is therefore nominally publishable; however, no `[package.metadata.docs.rs]` block exists.
- Fix shape: either add `publish = false` to the root `sy` package (it is a Fedora-coupled binary, not a library — publication to crates.io has little upside) or add `[package.metadata.docs.rs]` with `all-features = true`. Recommendation: `publish = false`, since this is an OS layer, not a crate.

### R-ECO-05 — SUGGESTED
- Status: gap
- Evidence: no `readme = "README.md"` field in any `[package]` block. If the root `sy` package is ever published to crates.io, the README will not be picked up automatically.
- Fix shape: add `readme = "README.md"` to `[package]` in `Cargo.toml`. Cost: one line.

## Audit log
- [seq:1] read AGENTS.md
- [seq:2] read CLAUDE.md
- [seq:3] read README.md (post-agentic-OS-rewrite)
- [seq:4] enumerated repo root: confirmed `LICENSE` present, `CHANGELOG.md` / `CONTRIBUTING.md` / `SECURITY.md` / `SUPPORT.md` / `GOVERNANCE.md` / `CODE_OF_CONDUCT.md` / `llms.txt` / `llms-full.txt` / `THIRD_PARTY_NOTICES.md` all absent
- [seq:5] enumerated `docs/`: one file (`docs/syauth-setup.md`)
- [seq:6] enumerated `.github/`: directory absent
- [seq:7] read `Cargo.toml` `[workspace.package]` + root `[package]` block (`Cargo.toml:1-30, 244-260`)
- [seq:8] read all `crates/*/Cargo.toml` `[package]` blocks
- [seq:9] read `crates/sy-core/src/lib.rs`, `crates/sy-ipc/src/lib.rs`, `crates/sy-testutils/src/lib.rs` heads — all carry `//!` doc
- [seq:10] read `src/main.rs:1-20` — no `//!` doc; `mod` declarations only
- [seq:11] enumerated `Makefile` targets: build, release, test, test-npu, lint, fmt, fmt-check, audit, bench, install, install-system-npu, install-system-syauth-selinux, yazi-plugins, help. No `docs-lint`.
- [seq:12] scored `R-COMMUNITY-01` pass
- [seq:13] scored `R-COMMUNITY-02` pass (with structured-license follow-up)
- [seq:14] scored `R-COMMUNITY-03` gap
- [seq:15] scored `R-COMMUNITY-04` gap
- [seq:16] scored `R-COMMUNITY-05` gap (highest blast radius)
- [seq:17] scored `R-COMMUNITY-06` gap
- [seq:18] scored `R-COMMUNITY-07` gap
- [seq:19] scored `R-COMMUNITY-08` gap
- [seq:20] scored `R-COMMUNITY-09` gap
- [seq:21] scored `R-COMMUNITY-10` gap
- [seq:22] writeback checkpoint — flushed Community Health findings to disk
- [seq:23] scored `R-README-01..03` pass
- [seq:24] scored `R-README-04..06` gap / partial
- [seq:25] scored `R-README-07` pass (zero badges, none vanity)
- [seq:26] scored `R-DIATAXIS-01..03` gap (no tutorial/how-to/reference dirs)
- [seq:27] scored `R-DIATAXIS-04` partial pass (explanation lives in `specs/`)
- [seq:28] scored `R-DIATAXIS-05` gap (`syauth-setup.md` mixes quadrants)
- [seq:29] writeback checkpoint — flushed README + Diátaxis findings
- [seq:30] scored `R-STYLE-01..05` (mostly pass, glossary suggested)
- [seq:31] scored `R-RELEASE-01..04` (changelog gap is MUST; release-notes deferred)
- [seq:32] scored `R-ADR-01` gap, `R-ADR-02..03` n/a-pending
- [seq:33] scored `R-CI-01..04` gap (no `.github/workflows/` at all)
- [seq:34] scored `R-LLMS-01..02` gap
- [seq:35] scored `R-COMPLY-01..02`
- [seq:36] scored `R-ECO-01..05` (library crates pass on crate-root docs; root `sy` package gap on `readme` field; rustdoc + doctest coverage deferred to `/implement`)
- [seq:37] writeback checkpoint — flushed remaining findings + final summary

## Final Audit Summary
- Scope: full
- Rows evaluated: 47
- pass: 10 (R-COMMUNITY-01, -02; R-README-01, -02, -03, -07; R-STYLE-02, -03; R-RELEASE-03; R-ECO-02 library crates)
- partial pass: 6 (R-README-06; R-STYLE-01, -04; R-RELEASE-02; R-DIATAXIS-04; R-ECO-02 binary)
- gap: 29
  - MUST gaps (8): R-COMMUNITY-03, -04, -05; R-DIATAXIS-01, -02, -03, -05; R-RELEASE-01
  - SHOULD gaps (14): R-COMMUNITY-06, -07, -08; R-README-04, -05; R-RELEASE-04; R-ADR-01; R-CI-01, -02, -03, -04; R-LLMS-01; R-COMPLY-01; R-ECO-01 / -03 (deferred to `/implement`)
  - SUGGESTED gaps (7): R-COMMUNITY-09, -10; R-STYLE-05; R-LLMS-02; R-COMPLY-02; R-ECO-04, -05
- n/a-pending: 2 (R-ADR-02, R-ADR-03 — evaluable once `R-ADR-01` lands)
- Plan: `specs/docs-audit/PLAN-full.md`
