# BUG-mount-fixture-restrictive-permissions: Make trusted mount modes explicit

## Summary

The trusted-cache group test inherits an unsuitable repository mode from the
launcher's restrictive file-creation mask.

## Reproduction

- Method: existing test under two controlled launcher masks.
- Test: `spark::executor::tests::trusted_cache_group_is_added_without_chown_capability`.
- Command: `cargo test --no-default-features --features spark-agent --bin sy
  spark::executor::tests::trusted_cache_group_is_added_without_chown_capability -- --exact`.
- Evidence: `target/march-step15-sy-umask-reproduce-red.log`, mask `077`, exit 101,
  `InvalidMode`; `target/march-step15-sy-umask-control-green.log`, mask `022`, exit 0.

## Expected Behavior

The trusted mount fixture supplies its required group-readable/searchable mode
independently of the launcher mask, while production rejects unsuitable modes.

## Actual Behavior

The fixture sets model and cache root permissions but leaves the repository child
at its inherited mode. Under mask `077`, that mode lacks required group access.

## Root Cause Analysis

Read-only mount validation requires group read/search bits on the repository
child. The test explicitly establishes root permissions but omits the child.
The same unchanged test passing under `022` isolates this fixture assumption.

## Fix

Set only the fixture repository child's permissions to `0750`, matching its
declared trusted read-only mount contract. Do not change validation or the
process-global mask. Preserve both control logs and the preceding source archive.

Stressors considered: restrictive or permissive masks, concurrent tests, inherited
permissions, nested roots, cache ownership, supplementary groups, read-only
mounts, capability removal, and production rejection of unsuitable modes.

## Traceability

- Failing test and controls: the two reproduction logs above.
- Fixed in: `src/spark/executor.rs`, test fixture setup only.
- Corrected exact test: `target/step15-catalog-sy-umask-fixture-corrected.log`,
  exit 0 under mask `077`.
- Full regression gates under `077`: `target/step15-catalog-sy-umask-headless.log`,
  `target/step15-catalog-sy-umask-test.log`, `target/step15-catalog-sy-umask-lint.log`,
  and `target/step15-catalog-sy-umask-lint-spark.log`, all exit 0.
- The preserved `step15-catalog-sy-umask-fixture-green.log` is an unsuccessful
  library-target invocation, not a test result. The first formatting check
  requested wrapping the new call; formatting changes no behavior.
- Scope: native kernel-catalog qualification prerequisite regression gate.
