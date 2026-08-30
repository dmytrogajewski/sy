<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to wire MCP into your agents

## Goal

Register `sy`'s MCP servers with Claude Code, Cursor, Codex, or
Gemini without hand-editing each client's config file.

## Prerequisites

- `sy` is installed and `sy.target` is active.
- The client you care about is installed. `sy auto` writes the
  config file that client already uses; it does not install the
  client.

## Steps

1. Print the dry-run plan. Nothing is written yet:

   ```bash
   sy auto configure --json
   ```

2. Restrict the plan to MCP detectors if you do not want the other
   auto detectors. Use the ids `sy auto list-detectors --json`
   printed on your machine:

   ```bash
   sy auto list-detectors --json
   sy auto configure --only mcp-claude,mcp-cursor,mcp-codex,mcp-gemini
   ```

3. Apply the same selection:

   ```bash
   sy auto configure --only mcp-claude,mcp-cursor,mcp-codex,mcp-gemini --apply
   ```

   Omit `--only` to apply every pending detector. `--apply` is
   required; without it the command stays a dry-run.

4. Restart the agent client so it reloads MCP server definitions.

5. Optional: enable or disable the knowledge MCP server on its own
   (also dry-run by default):

   ```bash
   sy knowledge mcp-enable --apply
   sy knowledge mcp-status --json
   ```

## Result

Each selected client has a `sy` MCP server entry. Tools such as
`knowledge_search` show up in the client after
restart. Re-running the command is safe: detectors that already
wrote their files report as present.

## See also

- [Tutorial: drive sy from an agent](../tutorials/drive-sy-from-an-agent.md)
- [CLI: `sy auto`](../reference/cli.md#sy-auto)
