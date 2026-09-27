# BUG-spark-headless-gui-feature-boundary: Keep GUI-only code out of Spark

## Summary

The feature-minimal Spark binary compiles unused desktop view code and fails
the warning-denying release-feature lint gate.

## Reproduction

- Command: `cargo clippy --no-default-features --features spark-agent --bin sy -- -D warnings`
- Evidence: `target/step29-feature-lint-red.log`, exit 101, 52 dead-code errors.
- Trigger: GUI consumers are disabled while their view state, preview bridge,
  thumbnail helpers, and stack-bar server remain compiled.

## Expected

The headless Spark feature build is warning-clean and preserves every CLI,
MCP, and signed qualification path. The default desktop build retains the
existing GUI behavior and tests.

## Actual

Unreachable GUI-only functions and state fail `-D warnings`; the default
lint command does not exercise this feature combination.

## Root Cause

GUI call sites are under `gui-iced`, but their exclusively consumed modules,
state fields, and helper functions are not. Source call-site searches and
the compiler diagnostics distinguish these from shared CLI/MCP code, which
must remain available. The IPC knowledge-search result also discarded its
diagnostic status; this shared path needs an observable status, not gating.

## Fix

Align producer and consumer feature boundaries in small compiler-verified
steps. Preserve shared functions and headless test helpers. Do not add lint
allowances, dead-code reference sentinels, or remove user commands. Run both
feature-minimal lint/tests and the default desktop lint/tests before release.

## Traceability

- Parent journey: `specs/journeys/JOURNEY-spark-qualification-jobs.md`
- Affected boundaries: `src/file/`, `src/plugin/`, `src/stack/`
- Reproduction and incremental diagnostics: `target/step29-feature-lint-*.log`
- Completion evidence remains open until both feature gates pass.
