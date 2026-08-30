# `sy file mcp` — MCP tools

The eleven `file_*` tools that `sy file mcp` advertises on stdio
JSON-RPC. Each tool is a thin wrapper around a `file.*` IPC call to
the running daemon (`$XDG_RUNTIME_DIR/sy-file.sock`, or
`$SY_FILE_SOCK`).

Source of truth: `src/file/mcp.rs`. For flags and exit codes, see
[the CLI reference](cli.md#sy-file). To open the window as a human,
see [browse files with sy file](../tutorials/browse-your-files.md).

`protocolVersion` is `2024-11-05`. The transport is one JSON object
per line — the same shape as `sy knowledge mcp` and `sy mon mcp`.

Every tool's response travels inside the MCP `content` / `isError`
envelope:

```json
{
  "content": [{ "type": "text", "text": "<json-stringified payload>" }],
  "isError": false,
  "structuredContent": <payload>
}
```

When the daemon is unreachable or a `file.*` op returns an error, the
envelope flips to `isError: true` and the `content[0].text` carries
the human-readable failure reason. JSON-RPC `error:` frames are
reserved for protocol-level faults (malformed `params`, unknown tool
name).

## `file_list`

### Description

List entries in a directory. Returns `{ entries: [{name, mime, size,
mtime}, …] }`.

### Arguments

| Name             | Type    | Optional | Description                                              |
|------------------|---------|----------|----------------------------------------------------------|
| `path`           | string  | no       | Directory to list.                                       |
| `include_hidden` | boolean | yes      | Include dotfiles; defaults to `false`.                   |
| `limit`          | integer | yes      | Maximum number of entries; defaults to `1024`.           |
| `offset`         | integer | yes      | Skip the first `offset` entries; defaults to `0`.        |

### Returns

```json
{
  "entries": [
    { "name": "Cargo.toml", "mime": "text/x-toml", "size": 1024, "mtime": 1700000000 }
  ]
}
```

### IPC mapping

`file.list { path }` (primary) → falls back to `file.open` +
`file.cd` + `file.state` if the daemon does not yet advertise
`file.list`.

### Errors

`isError: true` on daemon-unreachable.

## `file_open`

### Description

Set the daemon's current pane cwd to `path`.

### Arguments

| Name   | Type   | Optional | Description       |
|--------|--------|----------|-------------------|
| `path` | string | no       | Path to open at. |

### Returns

```json
{ "ok": true }
```

### IPC mapping

`file.open { path }`.

### Errors

`isError: true` on daemon-unreachable or `path` rejection.

## `file_copy`

### Description

Queue a copy op. Returns an `op_id`; poll `file_ops_list` for
progress and call `file_op_cancel` to abort.

### Arguments

| Name       | Type     | Optional | Description                                       |
|------------|----------|----------|---------------------------------------------------|
| `sources`  | string[] | no       | Source paths.                                     |
| `dest`     | string   | no       | Destination directory.                            |
| `conflict` | string   | yes      | `"skip"` (default), `"replace"`, or `"rename"`.   |

### Returns

```json
{ "op_id": 42 }
```

### IPC mapping

`file.copy { sources, dest, conflict }`.

### Errors

`isError: true` on daemon-unreachable. SPEC §4.3 exit-code 4
(refused) surfaces as the structured error message.

## `file_move`

### Description

Queue a move op. Same-fs moves rename in-place; cross-fs returns a
daemon error per SPEC §4.3 ("op cancelled / refused").

### Arguments

| Name       | Type     | Optional | Description                                       |
|------------|----------|----------|---------------------------------------------------|
| `sources`  | string[] | no       | Source paths.                                     |
| `dest`     | string   | no       | Destination directory.                            |
| `conflict` | string   | yes      | `"skip"` (default), `"replace"`, or `"rename"`.   |

### Returns

```json
{ "op_id": 43 }
```

### IPC mapping

`file.move { sources, dest, conflict }`.

### Errors

`isError: true` on daemon-unreachable. Cross-fs move without `--yes`
flips `isError: true` with a `cross-fs move … requires --yes`
message.

## `file_trash`

### Description

Send paths to the freedesktop trash.

### Arguments

| Name    | Type     | Optional | Description           |
|---------|----------|----------|-----------------------|
| `paths` | string[] | no       | Paths to trash.       |

### Returns

```json
{ "trashed": ["/home/user/old.txt"] }
```

### IPC mapping

`file.trash { paths }`.

### Errors

`isError: true` on daemon-unreachable or freedesktop trash failure.

## `file_restore`

### Description

Restore a previously-trashed entry by its original absolute path.

### Arguments

| Name           | Type   | Optional | Description                                 |
|----------------|--------|----------|---------------------------------------------|
| `trashed_path` | string | no       | Original path the trashed entry came from.  |

### Returns

```json
{ "ok": true }
```

### IPC mapping

`file.restore { trashed_path }`.

### Errors

`isError: true` if no trash entry matches `trashed_path`.

## `file_search`

### Description

Filename match against `walk(root)`. When `knowledge=true` and the
knowledge plane is up, results are re-ranked semantically. If the
knowledge plane is down, the daemon falls back to filename match and
the response carries `knowledge_status: "down"` so the agent knows
the result set is filename-only.

### Arguments

| Name        | Type    | Optional | Description                                       |
|-------------|---------|----------|---------------------------------------------------|
| `query`     | string  | no       | Search query.                                     |
| `root`      | string  | no       | Directory to search under.                        |
| `knowledge` | boolean | yes      | Enable knowledge-backed semantic re-ranking.      |

### Returns

```json
{
  "results": ["/home/user/notes/OOM-tuning.md"],
  "knowledge_status": "down"
}
```

The `knowledge_status` field is omitted when the knowledge plane is
up; agents should treat its absence as "up".

### IPC mapping

`file.search { query, root, knowledge }`.

### Errors

`isError: true` on daemon-unreachable or walk failure.

## `file_preview`

### Description

Render a preview for `path` as a PNG. Returns
`{ mime, png_base64 }`; the body is empty until the Step 27 plugin
dispatcher fills it. `max_width` / `max_height` are forward-
compatible sizing hints.

### Arguments

| Name         | Type    | Optional | Description                                   |
|--------------|---------|----------|-----------------------------------------------|
| `path`       | string  | no       | File to preview.                              |
| `max_width`  | integer | yes      | Sizing hint (pixels).                         |
| `max_height` | integer | yes      | Sizing hint (pixels).                         |

### Returns

```json
{
  "mime": "image/png",
  "png_base64": "iVBORw0KGgo…"
}
```

### IPC mapping

`file.preview { path, max_width, max_height }`.

### Errors

`isError: true` on daemon-unreachable or MIME-sniff failure.
`isError: true` with a `plugin …` message surfaces a Step 27 plugin
crash once the dispatcher is wired.

## `file_select`

### Description

Mutate the daemon's selection set against the current pane.

### Arguments

| Name    | Type     | Optional | Description                                     |
|---------|----------|----------|-------------------------------------------------|
| `paths` | string[] | no       | Paths to select (matched by basename).          |
| `mode`  | string   | no       | `"add"`, `"replace"`, or `"toggle"`.            |

### Returns

```json
{ "selection": ["/home/user/Cargo.toml", "/home/user/README.md"] }
```

### IPC mapping

`file.select { paths, mode }`.

### Errors

`isError: true` on daemon-unreachable or unknown `mode`.

## `file_ops_list`

### Description

Enumerate every in-flight or recently-completed op.

### Arguments

None.

### Returns

```json
{
  "ops": [
    { "op_id": 42, "kind": "copy", "state": "running", "done": 4096, "total": 8192 }
  ]
}
```

### IPC mapping

`file.ops_list {}`.

### Errors

`isError: true` on daemon-unreachable.

## `file_op_cancel`

### Description

Cancel an op by id. Best-effort: a running copy executor unlinks
the partial destination on observing the cancel signal.

### Arguments

| Name    | Type    | Optional | Description           |
|---------|---------|----------|-----------------------|
| `op_id` | integer | no       | Op id to cancel.      |

### Returns

```json
{ "ok": true }
```

### IPC mapping

`file.op_cancel { op_id }`.

### Errors

`isError: true` on daemon-unreachable.

## Stability

The JSON-Schema arg/return shapes documented above are **wire-
stable**: additive evolution only. New optional fields may appear in
returns without notice; new required arguments are a breaking change
and require a new tool name. Removing a field or repurposing one is
a breaking change. Agents that pin against this document must treat
unknown fields as transparent and `isError: true` envelopes as
unconditional aborts.
