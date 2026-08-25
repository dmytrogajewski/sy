<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to read power status

## Goal

Print the live power-governor document — current profile, source,
and rationale — from the CLI or from an MCP session you already
have.

## Prerequisites

- `sy-powerd` is running under your user manager:

  ```bash
  systemctl --user is-active sy-powerd.service
  ```

- If you want the MCP path, you already speak MCP to `sy` (see
  [how to wire MCP into your agents](wire-mcp-into-agents.md)).

## Steps

1. Print the human-readable status:

   ```bash
   sy power status
   ```

2. Print the stable `sy.power.status/v1` document:

   ```bash
   sy power status --json
   ```

3. If you already have an MCP session, call the `power_status` tool
   instead of shelling the CLI. A daemon-down dial surfaces as a
   JSON-RPC error (`code -32000`), not a transport crash.

4. Read the exit code before you retry. `0` means healthy. Any other
   code is listed under [CLI: `sy power`](../reference/cli.md#sy-power).
   A persistent `3` is a signal to inspect `sy power log` or
   `journalctl --user -u sy-powerd`, not an error to hammer.

## Result

You know which profile is active, why the bandit picked it, and
whether the daemon is reachable. For the offline PDF report over the
decision journal, use `sy power show` (add `--json` for
`sy.power.report/v1`).

## See also

- [CLI: `sy power`](../reference/cli.md#sy-power)
- [How to run sy doctor](run-doctor.md)
