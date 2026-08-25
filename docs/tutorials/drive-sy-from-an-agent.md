<!-- Template source: Good Docs Project tutorial template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/tutorial. Diátaxis quadrant: tutorial. -->

# Tutorial: drive sy from an agent

## Introduction

In this tutorial you let `sy auto` find the MCP servers `sy` ships
(search, power, file, and the others your install enables) and write
them into the agent configs on this machine. You then confirm an
agent can call `knowledge_search` the same way you called
`sy knowledge search` in the previous tutorial.

When you finish, `sy auto configure --apply` has updated the agent
config files, `sy auto` no longer reports those detectors as pending,
and a search from the agent returns hits from a registered source.

## Prerequisites

- You completed
  [Search your local files](search-your-files.md). At least one
  knowledge source is registered and indexed.
- At least one of Claude Code, Cursor, Codex, or Gemini CLI is
  installed so `sy auto` has a config file to write to. If none of
  those are installed, this tutorial still works as a dry-run: you
  will stop after the plan prints and skip the apply step.
- `sy.target` is active.

## Step 1 — See what auto would change

`sy auto configure` is a dry-run unless you pass `--apply`. Run it
once and read the plan:

```bash
sy auto configure
```

The output lists detectors (for example `mcp-claude`, `mcp-cursor`)
and the files they would write. Nothing on disk changes yet.

To see the same plan as JSON:

```bash
sy auto configure --json
```

To list every detector and whether it is on by default:

```bash
sy auto list-detectors --json
```

## Step 2 — Limit the plan to MCP, if you want

If you only want MCP plumbing and not other auto detectors, pass
`--only` with the detector ids from step 1. The names below are
examples; use the ids `list-detectors` printed on your machine:

```bash
sy auto configure --only mcp-claude,mcp-cursor
```

Read the plan. If a detector would overwrite a file you care about,
stop here and inspect that file first.

## Step 3 — Apply the plan

```bash
sy auto configure --apply
```

If you used `--only` in step 2, pass the same `--only` here.

`--apply` is required. The command refuses to rewrite agent configs
on a dry-run, so an agent that shells `sy auto` cannot change disk
by accident.

## Step 4 — Restart the agent

Restart Claude Code, Cursor, Codex, or Gemini so it reloads MCP
server definitions. A running session does not pick up a new
`mcp.json` (or equivalent) until it restarts.

## Step 5 — Call knowledge_search from the agent

In the agent, ask it to search for a phrase you know is in the
folder you indexed — the sample sentence from the search tutorial
works:

> Search my notes for "embed workload" using the sy knowledge tool.

The agent should call `knowledge_search` (stdio MCP from
`sy knowledge mcp`) and return the same hit you saw from
`sy knowledge search`.

If the agent has no MCP tools listed after the restart, re-run
`sy auto configure` without `--apply` and check that the detector
for that agent reported a write.

## Verify

1. `sy auto configure` (dry-run) no longer proposes the MCP writes
   from step 3, or it reports them as already present.
2. The agent lists a `knowledge_search` tool (names vary slightly by
   client; the MCP server is `sy knowledge mcp`).
3. A search from the agent returns a hit from your registered
   source.

If the knowledge daemon is down, the MCP tool returns a JSON-RPC
error rather than crashing the agent. Start `sy.target` and retry.

## Next steps

- To add or remove sources the agent can see, see
  [how to add a knowledge source](../how-to/add-a-knowledge-source.md).
- To wire MCP without the rest of `sy auto`, see
  [how to wire MCP into your agents](../how-to/wire-mcp-into-agents.md).
- For the CLIG contract (`--json`, `--dry-run`, `SY_*`, exit codes)
  see [why the CLI is agent-first](../explanation/agent-first-cli.md).
