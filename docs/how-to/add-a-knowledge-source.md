<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to add a knowledge source

## Goal

Register a folder of local files with `sy knowledge` so its contents
become searchable from the CLI, from the `sy-knowledge` MCP server,
and from the waybar tile.

## Prerequisites

- `sy` is installed and `sy.target` is running under your user
  systemd manager (`systemctl --user is-active sy.target` prints
  `active`). If not, see
  [the getting-started tutorial](../tutorials/getting-started.md).
- The `sy-knowledge` daemon and its embedded `qdrant` child are up
  (`systemctl --user is-active sy-knowledge.service` prints
  `active`).
- You have read access to the folder you intend to register.
  `sy knowledge` respects `.gitignore` by default, so files ignored
  by git are skipped automatically.

## Steps

1. Register the folder as a source. The path is recorded in
   `sy.toml`, so it survives reboots:

   ```bash
   sy knowledge add ~/Documents/notes
   ```

   The command prints `+ <absolute-path>` on first registration and
   `= <absolute-path> (already registered)` if the source is already
   on file. To add the folder in the disabled state (recorded but
   skipped by the indexer until you re-enable it by editing
   `sy.toml`), pass `--disabled`. To treat the path as a *discovery
   root* — meaning the indexer walks it for per-folder `qdr.toml`
   manifests instead of indexing the whole tree wholesale — pass
   `--discover`.

2. Confirm the registration:

   ```bash
   sy knowledge list
   ```

   The output names every registered source, its enabled flag, its
   mode (`explicit` or `discover`), and the schedule the daemon
   uses for incremental syncs. Pass `--json` for a machine-readable
   snapshot that also reports the live qdrant point count.

3. Run a one-shot incremental index pass so the new source is
   searchable immediately rather than at the next scheduled sync:

   ```bash
   sy knowledge index --source ~/Documents/notes
   ```

   The pass walks the folder, extracts text, embeds each chunk
   through the `aiplane` embed workload, and upserts the resulting
   vectors into qdrant. The command prints
   `scanned N | indexed N | skipped N | deleted N | <ms>` on
   completion. Omit `--source` to run an incremental pass over
   every registered source.

4. Search to confirm the content is indexed:

   ```bash
   sy knowledge search "<phrase from your notes>"
   ```

   By default the search runs the two-stage embed-plus-rerank path
   and returns the top eight hits. Add `--source ~/Documents/notes`
   to scope the search to the folder you just added, or `--json`
   for structured output.

## Result

`sy knowledge list` shows your folder as an enabled source, and
`sy knowledge search` returns hits drawn from it. The daemon picks
the source up on every scheduled incremental sync from now on, and
the `sy-knowledge` MCP server exposes the same hits to any agent
registered through `sy auto`.
