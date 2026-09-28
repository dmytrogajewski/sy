# BUG-plugin-crash-fixture-ownership: Crash injection crosses fixture boundaries

## Summary

The conformance crash test selects other live fake-plugin fixtures and can kill their handshakes.

## Reproduction

- Method: actual default release gate, `make test`.
- Failing scenario: `tests/sy_plugin_conformance.rs::cap_violation_returns_32099`.
- Evidence: `target/march-step20-session-operation.a8CD12/test.log`; command exit 2, capture exit 0.
- Observed error: `Transport("send initialize: framed send: Broken pipe (os error 32)")`.

## Expected

Crash injection affects only the child owned by the crash test. Concurrent capability tests complete independently.

## Actual

A concurrent capability fixture lost its pipe during initialization while the crash scenario passed.

## Root Cause

`find_children_by_cmdline` scanned all processes for the fake binary name; the crash test sent SIGKILL to every match. It did not check the per-fixture private working directory already supplied through `SpawnOpts`. This explains the cross-fixture failure; no signal trace was captured.

## Fix

Require the actual child's cwd to equal the requesting fixture's private directory and assert exactly one selected child. Keep another live fixture in the same crash scenario, assert it is excluded, and verify its original PID survives through restart and graceful shutdown. No production API or process policy changes.

## Traceability

- Fixed in: `tests/sy_plugin_conformance.rs`.
- Focused replay: `target/march-step20-session-operation.a8CD12/conformance-scoped.log`, 67 passed, 0 failed.
- Repaired `make lint`, `make test`, `make lint-spark`, `make test-spark`, and `make fmt-check` each exited 0 with capture exit 0; exact raw logs and exits use the `*-repaired` names in that directory.
