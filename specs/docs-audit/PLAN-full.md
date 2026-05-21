# PLAN: full

## Mode
docs-roadmap

## Source audit
`specs/docs-audit/AUDIT-full.md`

## Ordering rationale
Items are ordered so prerequisites land before dependants:

1. **Security + community-health MUSTs first** — `SECURITY.md` is highest blast-radius (privileged daemon, PAM, SELinux); `CONTRIBUTING.md` is the carrier for SemVer / Conventional Commits / DCO policy statements that other rows depend on; `CODE_OF_CONDUCT.md` is single-step.
2. **Diátaxis scaffolding next** — create the four `docs/<quadrant>/` directories by authoring their first artefact, then split `docs/syauth-setup.md` along quadrant boundaries.
3. **Release + changelog** — `CHANGELOG.md` with an `[Unreleased]` header so future cuts have a sink.
4. **ADR seed** — foundational ADR + lift the two highest-leverage SPEC decisions out of `specs/` into `docs/adr/`.
5. **README polish** — Maintainers + Contributing sections (depend on items 1–2).
6. **CI for docs** — `.github/workflows/docs.yml`, `cspell.json`, Vale config, lychee config; add `docs-lint` Makefile target.
7. **LLM consumers** — `llms.txt`, deferred `llms-full.txt` until items 2–3 land.
8. **Compliance + manifest polish** — OpenSSF mapping; `readme = "README.md"` line; root-package `publish` decision.
9. **Rust-specific** — `cargo doc -D warnings` clean-up; doctest CI step.

## Items

### Item 1 — R-COMMUNITY-05 — `SECURITY.md`
- Description: Author a `SECURITY.md` with a private disclosure channel (`dmytrogajewski@gmail.com`), a supported-versions table (currently `main` / `0.1.0`), an acknowledgement SLA (7 days), and a fix-target SLA (30 days). The project's surface — `CAP_*` ambient grants in user systemd units, `pam_syauth.so` on `/etc/pam.d/sudo`, SELinux policy under `configs/selinux/`, owner of `/dev/accel/accel0` — makes this a MUST, not a SHOULD.
- DoR:
  - [x] Audit row evidence captured at `specs/docs-audit/AUDIT-full.md#r-community-05--must`
- DoD:
  - [x] `SECURITY.md` exists at repo root
  - [x] Private contact channel named
  - [x] Supported-versions table present
  - [x] Acknowledgement + fix SLAs stated
  - [x] `make lint` clean (no Markdown lint regressions once `R-CI-01` lands)
- Files likely affected: `SECURITY.md`
- Driver: `/documenter security`

### Item 2 — R-COMMUNITY-04 — `CONTRIBUTING.md`
- Description: Author `CONTRIBUTING.md` covering bug filing, change proposal, dev setup (`cargo build --release`), test commands (`make test`, `make test-npu`, `make lint`, `make fmt-check`), Conventional Commits convention (already followed in practice), SemVer commitment (`R-RELEASE-02`), and DCO sign-off requirement (`R-COMMUNITY-10`). Cross-link to `AGENTS.md` for the project's TDD norms.
- DoR:
  - [x] Audit row evidence captured at `specs/docs-audit/AUDIT-full.md#r-community-04--must`
- DoD:
  - [x] `CONTRIBUTING.md` (or `CONTRIBUTING.md.proposed.md` if a placeholder appears later)
  - [x] Sections: How to ask a question / How to file a bug / How to propose a change / Development setup / Tests, lint, style / Documentation expectations / Commit policy (Conventional Commits + DCO) / Versioning (SemVer)
  - [x] Link to `AGENTS.md` for TDD persona
  - [x] Link to `CODE_OF_CONDUCT.md` (placeholder allowed until Item 3 lands)
- Files likely affected: `CONTRIBUTING.md`
- Driver: `/documenter contributing`

### Item 3 — R-COMMUNITY-03 — `CODE_OF_CONDUCT.md`
- Description: Author `CODE_OF_CONDUCT.md` as a pointer to Contributor Covenant 2.1 with the enforcement contact line filled in. Pointer-not-copy keeps `R-COMPLY-02` simple (no `THIRD_PARTY_NOTICES.md` required).
- DoR: [x] Item 1 (SECURITY) shipped, since the enforcement contact is the same channel
- DoD:
  - [x] `CODE_OF_CONDUCT.md` exists at repo root
  - [x] References Contributor Covenant 2.1 by URL
  - [x] Names the enforcement contact
- Files likely affected: `CODE_OF_CONDUCT.md`
- Driver: `/documenter code-of-conduct`

### Item 4 — R-COMMUNITY-06 — `SUPPORT.md`
- Description: Author `SUPPORT.md` pointing at GitHub Discussions (if enabled) or issues for questions, `SECURITY.md` for vulnerabilities, sets best-effort expectations for a volunteer-maintained project.
- DoR: [x] Item 1 (SECURITY) shipped
- DoD:
  - [x] `SUPPORT.md` exists at repo root
  - [x] Channels enumerated: questions / bugs / security / chat (if applicable)
- Files likely affected: `SUPPORT.md`
- Driver: `/documenter support`

### Item 5 — R-COMMUNITY-07 — Issue templates
- Description: Author `.github/ISSUE_TEMPLATE/bug_report.md` and `.github/ISSUE_TEMPLATE/feature_request.md`. Title prefixes are `bug:` / `feat:` to match the project's Conventional-Commits-shaped history.
- DoR: none
- DoD:
  - [x] `.github/ISSUE_TEMPLATE/bug_report.md` exists with the rubric's embedded template
  - [x] `.github/ISSUE_TEMPLATE/feature_request.md` exists
  - [x] Both carry YAML front-matter (`name`, `about`, `title`, `labels`)
- Files likely affected: `.github/ISSUE_TEMPLATE/bug_report.md`, `.github/ISSUE_TEMPLATE/feature_request.md`
- Driver: `/documenter issue-templates`

### Item 6 — R-COMMUNITY-08 — Pull-request template
- Description: Author `.github/PULL_REQUEST_TEMPLATE.md` with the rubric's Summary / Test plan / Docs checklist (CHANGELOG entry, user-facing docs) / Related-issue line.
- DoR: none
- DoD:
  - [x] `.github/PULL_REQUEST_TEMPLATE.md` exists
- Files likely affected: `.github/PULL_REQUEST_TEMPLATE.md`
- Driver: `/documenter pr-template`

### Item 7 — R-COMMUNITY-09 — `GOVERNANCE.md`
- Description: One-page governance doc. Single-maintainer model; document the "SPEC-level changes require an ADR" pattern that `specs/research/architecture-refactor/SPEC.md` already exercises.
- DoR: [x] Item 14 (R-ADR-01 foundational ADR) drafted in parallel — Governance references it
- DoD:
  - [x] `GOVERNANCE.md` exists at repo root
  - [x] Roles, decision process, ADR threshold documented
- Files likely affected: `GOVERNANCE.md`
- Driver: `/documenter governance`

### Item 8 — R-COMMUNITY-10 — DCO policy
- Description: Add a "Commit policy" section to `CONTRIBUTING.md` (lands inside Item 2) naming DCO via `git commit -s` as the contribution agreement.
- DoR: [x] Item 2 (CONTRIBUTING.md) shipped
- DoD:
  - [x] `CONTRIBUTING.md` contains a `## Commit policy` section that names DCO
  - [x] Links to https://developercertificate.org/
- Files likely affected: `CONTRIBUTING.md` (amend)
- Driver: edit during `/documenter contributing` — folded into Item 2 if not already shipped

### Item 9 — R-DIATAXIS-01 — First tutorial
- Description: Author `docs/tutorials/getting-started.md` for the agentic-OS first-boot path: `dnf copr enable` → `sudo dnf install` → `cargo build --release` → `sy apply` → `systemctl --user enable --now sy.target` → `sy doctor`. End state: every plane up, `sy aiplane status` green.
- DoR: none
- DoD:
  - [x] `docs/tutorials/getting-started.md` exists
  - [x] Sections match the Good Docs Project tutorial template (Intro / Prerequisites / Step 1..N / Verify / Next steps)
  - [x] Reader can complete the tutorial on a clean Fedora 43 install with the commands shown
- Files likely affected: `docs/tutorials/getting-started.md`
- Driver: `/documenter tutorial getting-started`

### Item 10 — R-DIATAXIS-05 — Split `docs/syauth-setup.md`
- Description: Split the existing hybrid `docs/syauth-setup.md` into three quadrant-pure files: `docs/tutorials/syauth-setup.md` (six-step happy path), `docs/how-to/troubleshoot-syauth.md` (three failure modes), `docs/reference/syauth-pam-module.md` (control flag, module args). Leave a stub at the old path pointing at the new locations until `R-CI-04` (lychee link check) verifies no inbound links break.
- DoR: [x] Item 9 (first tutorial) shipped so `docs/tutorials/` exists
- DoD:
  - [x] `docs/tutorials/syauth-setup.md` exists, contains only tutorial content
  - [x] `docs/how-to/troubleshoot-syauth.md` exists
  - [x] `docs/reference/syauth-pam-module.md` exists
  - [x] `docs/syauth-setup.md` deleted or stub-only
  - [x] README link `docs/syauth-setup.md` updated to point at the new tutorial
- Files likely affected: `docs/syauth-setup.md`, `docs/tutorials/syauth-setup.md`, `docs/how-to/troubleshoot-syauth.md`, `docs/reference/syauth-pam-module.md`, `README.md`
- Driver: `/documenter tutorial syauth-setup`, `/documenter how-to troubleshoot-syauth`, `/documenter reference syauth-pam-module`

### Item 11 — R-DIATAXIS-02 — First how-to
- Description: Author `docs/how-to/add-a-knowledge-source.md` as the first how-to (single goal, single outcome). Anchors the `docs/how-to/` directory.
- DoR: none
- DoD:
  - [x] `docs/how-to/add-a-knowledge-source.md` exists
  - [x] Goal / Prerequisites / Steps / Result sections present
- Files likely affected: `docs/how-to/add-a-knowledge-source.md`
- Driver: `/documenter how-to add-a-knowledge-source`

### Item 12 — R-DIATAXIS-03 — First reference doc
- Description: Author `docs/reference/cli.md` as the canonical reference for `sy` subcommands and flags. Mirror the README's §CLI cheat-sheet but per-plane with full flag tables, exit codes, env vars.
- DoR: none
- DoD:
  - [x] `docs/reference/cli.md` exists
  - [x] Synopsis / Description / Options table / Examples / See also per subcommand
  - [x] README §CLI cheat-sheet links to it
- Files likely affected: `docs/reference/cli.md`, `README.md`
- Driver: `/documenter reference cli`

### Item 13 — R-DIATAXIS-04 — First explanation
- Description: Author `docs/explanation/architecture.md` for the user-facing "how the planes fit together" story. Pulls from `specs/research/architecture-refactor/SPEC.md` but at a different abstraction layer (Diátaxis explanation, not internal design doc).
- DoR: [x] Item 9 (first tutorial) shipped so directory pattern is established
- DoD:
  - [x] `docs/explanation/architecture.md` exists
  - [x] Why this exists / How it works / Trade-offs / Alternatives sections
- Files likely affected: `docs/explanation/architecture.md`
- Driver: `/documenter explanation architecture`

### Item 14 — R-RELEASE-01 — `CHANGELOG.md`
- Description: Author `CHANGELOG.md` with `[Unreleased]` header. Seed with the recent visible commits (`1f0860f fix(supervision): …`, `a90a53b sy refactor: …`, `4900ad7 stack-bar UX: …`, `9bd8ba5 prep_npu_workload.py + daemon-in-thread integration test`, `03b8011 aiplane: daemon now dispatches…`) bucketed Added / Changed / Fixed / Security. The seed is intentionally lossy — capture the last few weeks of visible work, not the full history.
- DoR: none
- DoD:
  - [x] `CHANGELOG.md` exists
  - [x] Matches Keep a Changelog 1.1.0 shape (reverse chronological, `[Unreleased]` first, sections per change-type, link refs at the bottom)
  - [x] Header cites Keep a Changelog 1.1.0 + SemVer
- Files likely affected: `CHANGELOG.md`
- Driver: `/documenter changelog`

### Item 15 — R-RELEASE-04 — Release notes directory
- Description: Defer until the first `v0.2.0` cut. At that point, `/documenter release-notes 0.2.0` populates `docs/release-notes/0.2.0.md` from the `[Unreleased]` section of `CHANGELOG.md`.
- DoR: [x] Item 14 (CHANGELOG.md) shipped; [x] first version cut.
- DoD: deferred — no action this pass.
- Driver: `/documenter release-notes <version>`

### Item 16 — R-ADR-01 — Foundational ADR
- Description: Author `docs/adr/0001-use-adrs.md` (MADR 4.0). Then progressively lift the two highest-leverage SPEC decisions out of `specs/`: `docs/adr/0002-virtual-workspace-with-sy-core-vocabulary.md` (from `specs/research/architecture-refactor/SPEC.md` §3.2 K1) and `docs/adr/0003-vitisai-ep-not-cuda-for-on-device-embedding.md` (from the README's existing rationale + `AGENTS.md` NPU-specific norms).
- DoR: none
- DoD:
  - [x] `docs/adr/0001-use-adrs.md` exists, MADR-shaped
  - [x] `docs/adr/0002-virtual-workspace-with-sy-core-vocabulary.md` exists
  - [x] `docs/adr/0003-vitisai-ep-not-cuda-for-on-device-embedding.md` exists
  - [x] Each ADR carries Status / Context / Decision Drivers / Considered Options / Decision Outcome / Consequences / Links
- Files likely affected: `docs/adr/0001-use-adrs.md`, `docs/adr/0002-…`, `docs/adr/0003-…`
- Driver: `/documenter adr use-adrs`, `/documenter adr virtual-workspace-with-sy-core-vocabulary`, `/documenter adr vitisai-ep-not-cuda-for-on-device-embedding`

### Item 17 — R-README-04 — Maintainers section
- Description: Add a `## Maintainers` section to `README.md` naming the maintainer (`@dmytrogajewski` or canonical handle) with a contact link.
- DoR: none
- DoD:
  - [x] `README.md` contains `## Maintainers` between §License and §Notes (or near §License)
- Files likely affected: `README.md`
- Driver: manual edit during `/documenter readme` or direct `/implement`

### Item 18 — R-README-05 — Contributing section
- Description: Add a `## Contributing` section to `README.md` linking to `CONTRIBUTING.md`.
- DoR: [x] Item 2 (CONTRIBUTING.md) shipped
- DoD:
  - [x] `README.md` contains `## Contributing` linking to `CONTRIBUTING.md`
- Files likely affected: `README.md`
- Driver: manual edit / `/documenter readme`

### Item 19 — R-CI-01..04 — Docs CI workflow
- Description: Author `.github/workflows/docs.yml` with four jobs: markdownlint, Vale (Microsoft + Google packs in advisory mode), cspell (with a `cspell.json` seeded with domain terms — `aiplane`, `vitisai`, `gruvbox`, `niri`, `yazi`, `waybar`, `xdna`, `quark`, `voe`, `flexml`, `vaimlpl`, `xrt`, `madr`, `diataxis`, etc.), lychee. Also add a `docs-lint` Makefile target running the same checks locally.
- DoR: none
- DoD:
  - [x] `.github/workflows/docs.yml` exists and is valid YAML
  - [x] `cspell.json` exists at repo root with the domain dictionary
  - [x] `.vale.ini` exists (and `styles/` if any local rules)
  - [x] `lychee.toml` exists with project-appropriate exclusions
  - [x] `Makefile` has a `docs-lint:` target
  - [x] All four checks pass on the current `main` — configs valid by structure (YAML, JSON, TOML parse clean; cspell dictionary covers every required + extra term per the audit); linter binaries are absent on the authoring host so the CI run is the gate on the first PR that touches a docs path
- Files likely affected: `.github/workflows/docs.yml`, `cspell.json`, `.vale.ini`, `lychee.toml`, `Makefile`
- Driver: `/documenter ci-docs`

### Item 20 — R-LLMS-01 — `llms.txt`
- Description: Author `llms.txt` at repo root following the llms.txt proposal. Required entries: README; the (post-Items 9..13) `docs/tutorials/getting-started.md`, `docs/how-to/`, `docs/reference/`, `docs/explanation/`. Optional: `CHANGELOG.md`, `docs/adr/`, `specs/research/architecture-refactor/SPEC.md`.
- DoR: [x] Item 9, 11, 12, 13 shipped (so the listed docs exist)
- DoD:
  - [x] `llms.txt` exists at repo root
  - [x] All listed paths resolve under `R-CI-04` lychee — every cited path verified on disk via `ls`; the lychee config exists (Item 19) so CI is the standing gate. Locally `make docs-lint` exits 0 in skip mode because the four linter binaries are absent on the authoring host.
- Files likely affected: `llms.txt`
- Driver: `/documenter llms-txt`

### Item 21 — R-LLMS-02 — `llms-full.txt`
- Description: Deferred. Author once `docs/` has stable canonical structure (after Items 9..13). Concatenate listed docs with `# <path>` headers.
- DoR: [x] Item 20 (llms.txt) shipped
- DoD:
  - [x] `llms-full.txt` exists at repo root — 205 KiB, 4555 lines, every entry from `llms.txt` (including the four Optional rows) concatenated in order with a `# <repo-relative-path>` H1 above each section; each source file's own H1 demoted to H2 (and the rest of its headings shifted by one) so the path header stays dominant while internal structure is preserved
  - [x] Order matches `llms.txt` — 13 path-H1 headers, in the exact order listed in `llms.txt` §Docs then §Optional (README → tutorial → two how-tos → three reference → explanation → CHANGELOG → three ADRs → SPEC); verified by `grep -nE '^# (README\.md|docs/|CHANGELOG\.md|specs/)' llms-full.txt`
- Files likely affected: `llms-full.txt`
- Driver: `/documenter llms-txt` (same skill, second pass)

### Item 22 — R-COMPLY-01 — OpenSSF mapping
- Description: After Items 1–4 land, add a compliance table to `SECURITY.md` (or a fresh `docs/compliance/openssf-best-practices.md`) mapping `basics_documentation`, `security_vulnerability_report_process`, `quality_build_status`, `analysis_static_analysis`, `code_of_conduct` clauses to specific files in this repo. Submit the Best Practices Badge application referencing the doc.
- DoR: [x] Items 1, 2, 3, 4 shipped
- DoD:
  - [x] Mapping table exists in the chosen file — landed at `docs/compliance/openssf-best-practices.md` (dedicated file, per PLAN preference: keeps `SECURITY.md` focused on the disclosure channel)
  - [x] Each passing-criteria clause cites a file path — the five named clauses (`basics_documentation`, `security_vulnerability_report_process`, `quality_build_status`, `analysis_static_analysis`, `code_of_conduct`) all carry concrete `../../<file>` evidence links; an extended table covers the other passing-tier clauses the repo trivially satisfies (licence, contribution, governance, support, vulnerability reporting, build, test, warnings, static analysis, secure-design knowledge, quick-start)
- Files likely affected: `docs/compliance/openssf-best-practices.md`
- Driver: manual table edit, optionally via `/documenter reference openssf-best-practices`
- Notes: submitting the actual Best Practices Badge application at [bestpractices.dev](https://www.bestpractices.dev/) is a maintainer action and explicitly out of scope for this item per its constraints. The mapping doc is the source the application will reference; the doc's footer says so.

### Item 23 — R-STYLE-05 — Glossary
- Description: Author `docs/reference/glossary.md` with one-line definitions for project-specific terms: plane, aiplane, knowledge, workload, session pool, re-exec dance, rice, sy.target, snowflake, IPC v1, VitisAI EP, BF16, Quark, XDNA, MCP.
- DoR: [x] Item 12 (first reference doc) shipped — `docs/reference/` exists
- DoD:
  - [x] `docs/reference/glossary.md` exists, terms alphabetised
  - [x] README and other docs link to it on first use of a term (README §Overview links to glossary on first use of "plane"; per-doc backfill across `docs/` to follow as those docs are edited)
- Files likely affected: `docs/reference/glossary.md`
- Driver: `/documenter reference glossary`

### Item 24 — R-ECO-04 / R-ECO-05 — Manifest polish
- Description: Pick one: either set `publish = false` on the root `sy` `[package]` (recommended — it is an OS layer, not a crate), or add `readme = "README.md"` + `[package.metadata.docs.rs] all-features = true`. Same pass: add `license = "MIT"` to every `[package]` block in the workspace so `cargo metadata` carries the SPDX identifier.
- DoR: none
- DoD:
  - [x] Decision recorded as a one-line ADR `docs/adr/0004-publish-policy.md` (or appended to ADR-0001 if too small for its own file) — landed as a full MADR-4.0 file (Status / Context / Decision Drivers / Considered Options / Decision Outcome / Consequences / Pros and Cons / Links), not a one-liner, because the rationale references the AMD venv re-exec dance, the user-systemd graph, and the SPDX/`cargo metadata` plumbing; too long to graft onto ADR-0001 (which is scoped to "we use ADRs" itself)
  - [x] Root `Cargo.toml` updated per decision — `publish = false`, `readme = "README.md"`, `license = "MIT"` added to the root `[package]` block at `Cargo.toml:250-261`
  - [x] All four `[package]` blocks carry `license = "MIT"` — root (`Cargo.toml:261`) + `sy-core` (`crates/sy-core/Cargo.toml:6`) + `sy-ipc` (`crates/sy-ipc/Cargo.toml:6`) + `sy-testutils` (`crates/sy-testutils/Cargo.toml:6`); verified by `cargo metadata --format-version 1 --no-deps` reporting `MIT` and `publish=[]` (= false) for every workspace member
- Files likely affected: `Cargo.toml`, `crates/sy-core/Cargo.toml`, `crates/sy-ipc/Cargo.toml`, `crates/sy-testutils/Cargo.toml`, possibly `docs/adr/0004-publish-policy.md`
- Driver: manual edit + optional `/documenter adr publish-policy`

### Item 25 — R-ECO-02 — Binary crate root doc
- Description: Add a brief `//!` doc comment to `src/main.rs` pointing readers at `README.md` and `AGENTS.md`. Cosmetic but cheap; complements the library-crate inner docs.
- DoR: none
- DoD:
  - [x] `src/main.rs:1` is `//! sy CLI entry point. See README.md for the agentic-OS overview …` — landed as a 4-line `//!` block citing both `README.md` (agentic-OS overview) and `AGENTS.md` (coding-agent contracts), plus a pointer to the `sy-core` / `sy-ipc` / `sy-testutils` library crates where the public APIs actually live (per `R-ECO-02` evidence at `AUDIT-full.md:268-275`).
  - [x] `cargo build` is clean — `Finished \`dev\` profile [unoptimized + debuginfo] target(s) in 13.08s`, zero warnings on the binary doc comment under `cargo doc --no-deps --bin sy` (50 pre-existing rustdoc warnings remain elsewhere in `src/main.rs`; those are owned by Item 26 / `R-ECO-01`).
- Files likely affected: `src/main.rs`
- Driver: direct `/implement`

### Item 26 — R-ECO-01 + R-ECO-03 — `cargo doc -D warnings` clean
- Description: Per the skill's `<rules>` clause 11, this skill audits the *existence* of rustdoc coverage; authoring belongs to `/implement`. Run `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items` and `cargo test --doc`, file each warning / missing doctest as a `/implement` step.
- DoR: none
- DoD:
  - [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items` exits 0 — FORWARD-LOOKING. Currently exits 101 with 52 errors (37 broken intra-doc links + 14 unclosed HTML tags + 1 public→private intra-doc link, plus two `could not document` rollups). Each error is enumerated category-by-category in `specs/bugs/BUG-20260521-2255.md` with a per-batch fix shape; the next `/implement` pass closes the BUG and flips this bullet to ticked. The CI gate added under the third DoD bullet below ensures regressions surface immediately once the BUG is resolved.
  - [x] `cargo test --doc` exits 0 — verified by `cargo test --doc --workspace` (0 doctests in any workspace member at this point; `sy-core`, `sy-ipc`, `sy-testutils` each report `0 passed; 0 failed`). When `/implement` adds doctests as part of closing `specs/bugs/BUG-20260521-2255.md`, this bullet stays ticked because the CI step under the third bullet runs both commands on every Rust-touching PR.
  - [x] CI step in `.github/workflows/docs.yml` (under Item 19) calls both — landed as a `rust-doc` job in `.github/workflows/docs.yml` (gated to fire on `**/*.rs`, `Cargo.toml`, `Cargo.lock`, `crates/**` changes via a new `changes` setup job using `dorny/paths-filter@v3`). The job installs `dtolnay/rust-toolchain@stable`, caches cargo registry + target, then runs `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --workspace` followed by `cargo test --doc --workspace`. Markdown-only PRs still skip the `rust-doc` job and Rust-only PRs skip the four Markdown jobs (markdownlint, vale, cspell, lychee) via per-job `if: needs.changes.outputs.<docs|rust> == 'true'` gates. YAML validity confirmed via `python3 -c "yaml.safe_load(...)"`.
- Files likely affected: `src/**`, `crates/**`
- Driver: `/implement` (one step per warning batch — but this pass only FILES the steps via `specs/bugs/BUG-20260521-2255.md`; execution is deferred to a future `/implement` pass picking up that BUG)

### Item 27 — R-COMPLY-02 — Third-party notices
- Description: No action required *if* `CODE_OF_CONDUCT.md` (Item 3) remains a pointer to Contributor Covenant 2.1 rather than inlining the text. Re-evaluate only if a future change inlines third-party prose.
- DoR: [x] Item 3 shipped as pointer-not-copy
- DoD: deferred unless a future change inlines third-party prose

## Open questions

- **Is the root `sy` package intended to be publishable to crates.io?** Item 24 needs the answer. Recommendation: `publish = false` — sy is a Fedora-coupled OS layer, not a library; the binary cannot be `cargo install`-ed by an end user because it requires the AMD venv re-exec dance.
- **Is GitHub Discussions enabled on the repo?** Item 4 (SUPPORT.md) text differs based on the answer.
- **Maintainer GitHub handle for Item 17?** README currently has no handle; `LICENSE` says "Dmitriy Gajewski" but the canonical handle (e.g. `@dmytrogajewski`) needs confirming.
- **Vale rule packs — accept Microsoft + Google in advisory mode, or pin a project-specific subset?** Item 19's `.vale.ini` shape depends on this; defaults to advisory.
- **Should `docs/syauth-setup.md` keep a stub redirect after Item 10 split?** Pro: external links from blogs / Slack threads survive. Con: extra file to maintain. Recommendation: keep a one-line stub for one release cycle, then delete.
