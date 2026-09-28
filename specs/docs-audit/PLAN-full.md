# PLAN: full

Status: complete

## Mode
docs-roadmap

## Source audit
`specs/docs-audit/AUDIT-full.md`

## Items

### Item 1 — R-DIATAXIS-01 start-here
- Description: Author `docs/intro.md` as the new-reader map: what `sy` is, who it is for, what you need, and which quadrant to open next.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] `docs/intro.md` exists as a Diátaxis start-here map (links only; no mixed quadrants)
- Files likely affected: `docs/intro.md`
- Driver: `/documenter` authoring (start-here page)

### Item 2 — R-DIATAXIS-01 search tutorial
- Description: Author `docs/tutorials/search-your-files.md` so a reader who finished bring-up can register a folder and run a first search.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] Tutorial follows Good Docs Project (Prerequisites → Steps → Verify → Next steps)
- Files likely affected: `docs/tutorials/search-your-files.md`
- Driver: `/documenter tutorial search-your-files`

### Item 3 — R-DIATAXIS-01 agent tutorial
- Description: Author `docs/tutorials/drive-sy-from-an-agent.md` covering `sy auto` MCP plumbing into Claude / Cursor / Codex / Gemini.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] Tutorial follows Good Docs Project
- Files likely affected: `docs/tutorials/drive-sy-from-an-agent.md`
- Driver: `/documenter tutorial drive-sy-from-an-agent`

### Item 4 — R-DIATAXIS-02 NPU how-to
- Description: Author `docs/how-to/set-up-npu.md` to close the broken link from the getting-started tutorial.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] How-to exists; getting-started link resolves
- Files likely affected: `docs/how-to/set-up-npu.md`
- Driver: `/documenter how-to set-up-npu`

### Item 5 — R-DIATAXIS-02 remaining how-tos
- Description: Author goal-oriented how-tos for MCP wiring, Spark serve, theme apply, doctor, and power status.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] Each how-to has one Goal and one Result
- Files likely affected: `docs/how-to/wire-mcp-into-agents.md`, `docs/how-to/serve-a-model-on-spark.md`, `docs/how-to/apply-a-theme.md`, `docs/how-to/run-doctor.md`, `docs/how-to/read-power-status.md`
- Driver: `/documenter how-to`

### Item 6 — R-DIATAXIS-03 spark and config reference
- Description: Author `docs/reference/spark.md` and `docs/reference/config.md`.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] Reference pages use synopsis / fields / examples; no tutorial steps
- Files likely affected: `docs/reference/spark.md`, `docs/reference/config.md`
- Driver: `/documenter reference spark`; `/documenter reference config`

### Item 7 — R-DIATAXIS-04 explanations
- Description: Author explanations for no-snowflakes, agent-first CLI, and NPU-not-GPU.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] Each page is mental-model only (no command recipes)
- Files likely affected: `docs/explanation/no-snowflakes.md`, `docs/explanation/agent-first-cli.md`, `docs/explanation/why-npu-not-gpu.md`
- Driver: `/documenter explanation`

### Item 8 — R-STYLE-05 glossary
- Description: Add `spark`, `sy file`, `sy mon`, and `sy doctor` to `docs/reference/glossary.md`.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] Four terms present, alphabetised
- Files likely affected: `docs/reference/glossary.md`
- Driver: `/documenter reference glossary`

### Item 9 — R-LLMS-01 catalogue
- Description: Point `llms.txt` at the new pages.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] `llms.txt` lists the new tutorial / how-to / reference / explanation paths
  - [x] `llms-full.txt` concatenates every path in `llms.txt`
- Files likely affected: `llms.txt`, `llms-full.txt`
- Driver: `/documenter llms-txt`

### Item 10 — R-DIATAXIS-05 start-here and how-to tables
- Description: Slim `docs/intro.md` to a Diátaxis start-here map. Move exit-code lookup tables out of `docs/how-to/run-doctor.md` and `docs/how-to/read-power-status.md` into links to `docs/reference/cli.md`.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] `docs/intro.md` has no steps, field tables, or explanation essay
  - [x] doctor and power how-tos have no exit-code tables
- Files likely affected: `docs/intro.md`, `docs/how-to/run-doctor.md`, `docs/how-to/read-power-status.md`
- Driver: `/documenter` authoring

### Item 11 — R-COMPLY-01 and R-ECO
- Description: Rescore rustdoc rows against `cargo doc -D warnings` and `cargo test --doc`. Annotate every MUST/SHOULD audit row with an OpenSSF passing-criteria clause.
- DoR:
  - [x] Audit row evidence captured in AUDIT-full.md
- DoD:
  - [x] `R-ECO-01`..`03` scored from a live cargo invocation
  - [x] MUST/SHOULD rows carry an `OpenSSF:` line
- Files likely affected: `specs/docs-audit/AUDIT-full.md`
- Driver: `/documenter` audit

## Open questions
- None for the current documentation scope. The existing Docusaurus site and
  GitHub Actions are maintained beside this plan and pass their production
  build/link checks in the current pass.

### Item 12 — Sparkplane bridge/network documentation refresh
- Description: Align sy's Sparkplane documentation with the pinned `0.1.5`
  client and its explicit per-launch network opt-in.
- DoR:
  - [x] Current `sy.toml` release pin inspected
  - [x] Canonical Sparkplane reference checked for flag and environment names
- DoD:
  - [x] README, installation, serving, CLI, and bridge reference show a
    copy-pasteable `--allow-network` example
  - [x] The docs state the opt-in is per launch and does not remove filesystem
    sandboxing or alter approval policy
  - [x] Changelog records the documentation change
  - [x] Legacy syauth page identifies itself as a compatibility index
- Files affected: `README.md`, `CHANGELOG.md`, `docs/how-to/install-spark.md`,
  `docs/how-to/serve-a-model-on-spark.md`, `docs/reference/cli.md`,
  `docs/reference/spark.md`, `docs/syauth-setup.md`
- Driver: `/documenter` current-pass refresh

### Item 13 — Rust public-item documentation
- Description: Add outer documentation comments for every public item that
  fails strict rustdoc, including the constants, fields, variants, methods,
  and constructors discovered across the workspace.
- DoR:
  - [x] Strict `RUSTDOCFLAGS="-D warnings -D missing_docs" cargo doc --no-deps --workspace` reproduced the finding
- DoD:
  - [x] Strict workspace rustdoc exits successfully
- Files likely affected: `crates/sy-plugin-md/src/render.rs`
- Driver: `/implement` (implementation, not documentation-only authoring)

### Item 14 — Third-party notices inventory
- Description: Add a repository-level inventory for Good Docs Project
  templates and other third-party material referenced in documentation.
- DoR:
  - [x] Attribution comments and external template sources identified
- DoD:
  - [x] `THIRD_PARTY_NOTICES.md` lists source, licence, and affected files
  - [x] A community/documentation entry links to the inventory
- Files likely affected: `THIRD_PARTY_NOTICES.md`, `CONTRIBUTING.md`
- Driver: `/documenter` authoring
