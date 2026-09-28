# BUG-spark-upgrade-qualification-authority-confinement: Spark upgrade rejects the qualification authority

## Summary

A signed Spark control-plane upgrade fails its first executor activation because
the confined executor cannot read the release authority required by the
qualification subsystem.

## Reproduction

- Method: real DGX Spark activation plus focused regression test.
- Test:
  `src/spark/install.rs::spark::install::tests::confined_executor_can_read_the_active_qualification_authority`.
- Command:
  `cargo test --no-default-features --features spark-agent confined_executor_can_read_the_active_qualification_authority -- --nocapture`.
- Evidence: the real executor journal reported
  `read qualification release authority: Permission denied (os error 13)`;
  the regression test fails because the executor profile lacks
  `/opt/sy-spark/releases/*/minisign.pub r,`.

## Expected

The signed release is activated once, the executor reads the authority from
the active content-addressed release, and semantic health succeeds without a
rollback.

## Actual

The executor's first start exits with `EACCES`. The installer treats that exit
as an activation failure and rolls the `current` link back. A subsequent
systemd retry starts the preceding binary, which does not exercise the new
authority read and therefore masks the missing confinement rule.

## Root Cause

`qualification::RELEASE_PUBLIC_KEY_PATH` resolves through
`/opt/sy-spark/current` to a content-addressed release file. The executor
AppArmor profile permits the release executable but does not permit reading
the adjacent `minisign.pub`, so Linux rejects the read even though the process
runs as root.

## Fix

The agent and executor confinement policies grant read-only access to exactly
the release-scoped `minisign.pub`. Focused agent/executor regressions, AppArmor
parser coverage, `make test`, and `make lint` pass without adding broader write
authority. The rebuilt signed aarch64 release activated on the real DGX Spark,
and a signed qualification completed while both services remained healthy.

## Traceability

- Failing test: `src/spark/install.rs`.
- Fixed in: `configs/apparmor.d/sy-spark-agent`,
  `configs/apparmor.d/sy-spark-executor`, and the focused regressions in
  `src/spark/install.rs`.
