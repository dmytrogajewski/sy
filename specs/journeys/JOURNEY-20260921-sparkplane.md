# JOURNEY: independent Sparkplane with a sy bridge

## Actor & Goal

- Actor: DGX Spark operator and contributor, using either standalone CLI or sy.
- Goal: build, release and operate Sparkplane without any sy checkout or library dependency.
- Hardest constraint: preserve the optimized vLLM deployment, credentials and inference URLs during a signed, resumable namespace transition with at most one planned model reload.

## Happy Path

1. Build standalone Sparkplane from the audited current Spark implementation (`src/spark/mod.rs`), owning its IPC and platform assets.
2. Install a verified, pinned client with `sy apply`; `sy spark HOST ...` delegates unchanged before NPU initialization (`src/main.rs:399`).
   Use `sy apply --only sparkplane` to install that integration without applying
   desktop configurations, services, or unrelated coding-agent package updates.
3. Inspect and dry-run migration; receive a conflict-checked, signed transition plan without changing the appliance.
4. Confirm migration; drain, snapshot, rename state and identities, install policies and reload the same optimized model once.
5. Use existing inference URLs and client credentials through either CLI; compare performance against a fresh matched baseline.
6. Build, sign and deploy the next Sparkplane release independently; sy needs no engine/model source changes.

## Edge Cases

- Missing or incompatible client: actionable stderr/exit status; ordinary commands never install software.
- Tampered release or transition authority: reject before execution or state changes.
- Destination conflicts, insufficient disk, cross-filesystem data moves: preflight failure before stopping work.
- Interrupted migration: durable journal and consistent SQLite backup; resumable or reversible before accepting new writes.
- User-modified generated client files: preserve them and report conflict, never overwrite blindly.
- GPU readiness/performance regression: do not commit cutover; preserve diagnostics and previous release.

## Acceptance Criteria

- [x] Independent client and appliance builds/tests without sy dependencies.
- [x] Bridge process/TTY/JSON/exit/env and signed installer tests.
- [x] Migration failure injection and rollback coverage.
- [x] Optimized vLLM and maximum-context/concurrency qualification preserved.
- [x] Matched performance within 5% or explained and accepted.
- [x] Public repository publication audit, independent CI and signed release authority.
- [x] Both projects' lint/test gates green and documentation current.

Live acceptance completed with v0.1.4 on 2026-09-21; see the roadmap's final
evidence. Failed activation and rollback required additional recovery reloads,
so the one-reload happy path was not achieved. The final qualified cutover
preserved the optimized image/settings and measured −0.0127% median throughput
change. No Docker restart or host reboot was used.

## Out of Scope

Generic GPU support, merging Sparky, inference redesign, Docker restart and host reboot.

## Decisions

Approved proposal in conversation: public MIT `Sumatoshi-tech/sparkplane`, full namespace migration, sy-managed pinned client installation, one planned model reload. Live changes follow successful migration checks, never precede them.
