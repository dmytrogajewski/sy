# BUG-docs-lint-vendored-website: Exclude installed website dependencies

## Summary

The local and CI Markdown lint globs include installed website dependencies.

## Reproduction

- Method: manual documentation gate with the website dependencies installed.
- Command: `make docs-lint`, with markdownlint-cli2 0.22.1 on `PATH`.
- Evidence: `target/step15-catalog-sy-docs-lint.log` exits 2; all 22,146
  Markdown diagnostics name files below `website/node_modules/`.
- Controlling gate: rerun the same command after the scoped glob correction.

## Expected Behavior

Lint repository documentation without linting installed third-party dependencies.

## Actual Behavior

The Markdown gate scans 1,936 files and fails on vendored dependency documents
before the remaining documentation checks run.

## Root Cause Analysis

The Makefile and docs CI job exclude root `node_modules/` but not the separate
`website/node_modules/` tree. The existing spelling and link checks already
exclude that tree. The failure log contains no repository Markdown diagnostic.

## Fix

Add the exact `!website/node_modules/**` exclusion to the two Markdown lint
invocations. Keep repository documentation rules and all other checks unchanged.
Freeze both original configuration files before editing; preserve the RED log.

Stressors covered by the narrow scope are dependencies being installed or absent,
new dependency documents, nested dependency packages, local versus CI execution,
shell quoting, glob semantics, repository Markdown violations, spelling checks,
link checks, and unrelated vendor directories. Only the identified tree is excluded.

## Traceability

- Failing gate: `target/step15-catalog-sy-docs-lint.log`.
- Original inputs: `target/step15-catalog-sy-docs-preflight.sha256` and the
  corresponding `target/step15-catalog-sy-docs-preflight/` copies.
- Fixed in: `Makefile` and `.github/workflows/docs.yml`.
- Passing gate: `target/step15-catalog-sy-docs-lint-retry.log`, exit 0;
  218 Markdown files, zero Markdown, spelling, or link errors. Vale is advisory
  and unavailable; no Vale pass is claimed.
- Regression gates: `target/step15-catalog-sy-docs-fix-test.log`,
  `target/step15-catalog-sy-docs-fix-lint.log`, and
  `target/step15-catalog-sy-docs-site.log`, all exit 0.
- Scope: build-and-load-native-kernel-catalog qualification prerequisite.
