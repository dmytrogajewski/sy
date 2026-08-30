<!-- Template source: Good Docs Project tutorial template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/tutorial. Diátaxis quadrant: tutorial. -->

# Tutorial: search your local files

## Introduction

In this tutorial you register a folder with the `knowledge` plane,
run one index pass, and search it from the CLI. You leave with a
working local search that the same MCP server can later expose to
your agents.

When you finish, `sy knowledge search` returns hits from that folder
and each hit carries a `chunk_id` you can fetch in full.

## Prerequisites

- You completed
  [the bring-up tutorial](getting-started.md). `sy.target` is
  active:

  ```bash
  systemctl --user is-active sy.target
  ```

  The command prints `active`.

- `sy knowledge` is running:

  ```bash
  systemctl --user is-active sy-knowledge.service
  ```

  The command prints `active`. If it prints `inactive`, run
  `systemctl --user start sy.target` and try again.

- You have a folder of text you can read — notes, a git checkout,
  or `~/Documents`. This tutorial uses `~/Documents/notes`. Create
  it and drop a file in if it does not exist:

  ```bash
  mkdir -p ~/Documents/notes
  echo "sy knowledge indexes local files through the aiplane embed workload." > ~/Documents/notes/first.txt
  ```

- An AMD NPU is **not** required. Without one, embeddings run on
  CPU. The first index pass is slower; the commands stay the same.

## Step 1 — Register the folder

```bash
sy knowledge add ~/Documents/notes
```

The command prints `+` and the absolute path on first registration.
If the folder is already registered it prints `=` and
`(already registered)`.

## Step 2 — Confirm the source is listed

```bash
sy knowledge list
```

You see the folder, an enabled flag, and the sync schedule. Add
`--json` if you want the same snapshot for a script.

## Step 3 — Index once

Scheduled sync can wait. Run a one-shot pass so the folder is
searchable now:

```bash
sy knowledge index --source ~/Documents/notes
```

The command prints a line of the form
`scanned N | indexed N | skipped N | deleted N | <ms>`. For the
sample file you created, `indexed` is at least `1`.

## Step 4 — Search

```bash
sy knowledge search "embed workload"
```

You get a short list of hits. The sample file you wrote in
prerequisites should appear. Add `--json` to print structured
results, including `chunk_id` and `confidence`.

## Step 5 — Fetch one chunk in full

Copy a `chunk_id` from the JSON search and fetch the uncapped text:

```bash
sy knowledge search "embed workload" --json
sy knowledge get-chunk <chunk_id>
```

Replace `<chunk_id>` with the id from the search output. The command
prints the full chunk, not the truncated snippet from search.

## Verify

Confirm three things:

1. `sy knowledge list` shows `~/Documents/notes` (or its resolved
   absolute path) as enabled.
2. `sy knowledge search "embed workload"` returns at least one hit.
3. `sy knowledge get-chunk` on that hit's `chunk_id` prints the
   sentence you wrote into `first.txt`.

If search returns nothing, wait a few seconds and retry the index
pass. If the daemon is down, `sy knowledge search` exits `4`
(qdrant unreachable). Start `sy.target` and run the search again.

## Next steps

- To add another folder, or to disable a source without deleting it,
  see [how to add a knowledge source](../how-to/add-a-knowledge-source.md).
- To browse files in a window (names and previews, not semantic
  search), see [browse files with sy file](browse-your-files.md).
- To expose the same search to Claude, Cursor, Codex, or Gemini, see
  [drive sy from an agent](drive-sy-from-an-agent.md).
- For flags, filters, and exit codes, see
  [the CLI reference](../reference/cli.md#sy-knowledge).
