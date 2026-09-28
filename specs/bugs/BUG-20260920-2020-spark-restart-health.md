# Docker-owned restart retains a stale healthy route

## Summary

Spark advertised a healthy model while Docker was reloading its crashed engine.

## Reproduction

- At 2026-09-20 17:00 UTC the Qwen EngineCore crashed; Docker's restart counter
  became one and the container's host process changed, without a new sy generation.
- `sy spark dgx-spark ps --json` still returned `healthy: true` while Responses
  requests returned `healthy generation became unavailable`.
- Regression: `spark::agent::tests::published_route_requires_the_same_live_process`.

## Expected

A new process must pass native health and semantic readiness before its route
is published. Status must not reuse an earlier process's readiness result.

## Actual

The reconciler skipped readiness whenever an exact generation was already
published and Docker reported a running container with `unless-stopped`.

## Root Cause

Container generation is not process identity. Docker-owned restarts preserve
the container and sy generation while replacing the process. The executor
already supplies PID and `/proc` start ticks, but routes did not retain them.

## Fix

Bind published routes to the executor-observed PID/start-tick pair. Reuse
readiness only for the same generation and process; otherwise mark the route
warming before re-entering readiness. PID reuse alone cannot bypass this gate.
Reconciliation is event-triggered with a 30-second periodic fallback, not an
instantaneous liveness guarantee. No wire schema or client command changes.

Live confirmation: generation 4 crashed again at 17:41:48 UTC during the
sampling qualification. With this change deployed, `ps` reported
`healthy: false` and degraded state while Docker restarted the engine.

## Traceability

- `src/spark/upstream.rs`: observed route process identity.
- `src/spark/agent.rs`: startup binding, restart gating, regression test.
- Related production incident: `BUG-20260920-2015-qwen-sampling-startup.md`.
