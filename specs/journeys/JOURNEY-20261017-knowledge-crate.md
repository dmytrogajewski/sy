# Journey: knowledge runtime as a workspace crate

**Actor:** desktop user invoking `sy knowledge` or `sy auto` from a checkout.
**Goal:** retain the command and IPC contract while knowledge builds in its own crate.

1. `sy knowledge --help` exposes the existing subcommands and flags.
2. `sy auto --help` exposes the existing detector commands.
3. `sy knowledge daemon` owns the knowledge socket and delegates workloads to the AI plane as before.
4. Knowledge CLI and MCP requests use the same IPC envelopes and exit codes.

## Acceptance

- `cargo check -p sy-knowledge --lib` and focused CLI and IPC tests pass.
- Root `sy` still parses and dispatches both command trees.
- `make lint`, `make test`, `make lint-spark`, and `make test-spark` pass.

## Source paths

Knowledge runtime moved from `src/knowledge/` to
`crates/sy-knowledge/src/knowledge/`. Its detector and MCP configuration
helpers moved from `src/auto.rs` and `src/auto_mcp.rs` to
`crates/sy-knowledge/src/`. The root `src/main.rs` imports those modules
through `sy-knowledge`; its command names and dispatch are unchanged.
