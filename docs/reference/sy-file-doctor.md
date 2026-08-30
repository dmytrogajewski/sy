# `sy file doctor` and `sy plugin doctor`

JSON envelopes for `sy file doctor --json` and
`sy plugin doctor --json`. Use this page when you are parsing those
documents. For the first-time window, see
[browse files with sy file](../tutorials/browse-your-files.md).

Source of truth: `src/file/doctor.rs` and `src/plugin/cli.rs`.

For `--json`, exit codes, and env vars in general, see
[the CLI reference](cli.md).

## Overview

Both commands run a fixed list of probes, then print a human summary
(on a TTY) or the JSON envelope with `--json`.

`sy file doctor` exit codes: **0** all passed, **1** any failed,
**2** warnings only. That `2` is not the same as top-level
`sy doctor`, which uses **3** for warn-only.

## `sy file doctor`

### Envelope

```json
{
  "schema": "sy.file.doctor/v1",
  "status": "ok",
  "checks": [
    {
      "name": "file.daemon.reachable",
      "status": "ok",
      "detail": "daemon socket /run/user/1000/sy-file.sock accepts connections"
    }
  ]
}
```

Field shape:

| Field            | Type   | Description                                              |
|------------------|--------|----------------------------------------------------------|
| `schema`         | string | Wire-stable marker; pinned at `sy.file.doctor/v1`.       |
| `status`         | string | Worst-of-checks: `"ok"` / `"warn"` / `"fail"`.           |
| `checks[]`       | array  | One entry per probe in stable registration order.        |
| `checks[].name`  | string | Stable dot-separated id (`file.<subsystem>.<probe>`).    |
| `checks[].status`| string | Per-probe outcome: `"ok"` / `"warn"` / `"fail"`.         |
| `checks[].detail`| string | Human-readable one-liner. Always present.                |
| `checks[].fix_hint`| string | One-paste path to green. Absent on `ok` rows.          |

### Probes

#### `file.daemon.reachable`

* **Status `ok`**: a blocking `UnixStream::connect` against the
  resolved daemon socket (`$SY_FILE_SOCK` or
  `$XDG_RUNTIME_DIR/sy-file.sock`) returns immediately.
* **Status `fail`**: connect refused or socket file missing.
* **Typical fix-hint**: `systemctl --user start sy-file.socket`.

#### `file.fonts.jetbrainsmono_nerd`

* **Status `ok`**: a file whose name contains both `JetBrainsMono` and
  `Nerd` is reachable under `$SY_FILE_FONTS_DIR` (when set) or via
  `fc-list` (host-mode).
* **Status `fail`**: no such font on disk / not reported by `fc-list`.
* **Status `warn`**: `fc-list` is absent and no fonts-dir override is
  set (fontconfig isn't installed).
* **Typical fix-hint**: `dnf install jetbrainsmono-nerd-fonts`.

#### `file.niri.binds`

* **Status `ok`**: `Mod+E`, `Mod+Shift+E`, and `Mod+/` all start
  `sy file`. Niri config is read from `opts.niri_config` or
  `$XDG_CONFIG_HOME/niri/config.kdl`.
* **Status `fail`**:
  * any required bind is absent, **or**
  * any required bind starts something else (for example `Mod+E`
    rebound to `swaylock`). The `detail` field names the target.
* **Typical fix-hint**: `sy apply` (rewrites the niri binds from
  `configs/`).

#### `file.systemd.unit_installed`

* **Status `ok`**: both `sy-file.service` and `sy-file.socket` exist
  under `$XDG_CONFIG_HOME/systemd/user/`.
* **Status `fail`**: one or both unit files missing.
* **Typical fix-hint**: `sy apply`.

#### `file.bookmarks.writable`

* **Status `ok`**: `$XDG_STATE_HOME/sy/file/` exists (or was created)
  and a sentinel file round-tripped successfully.
* **Status `fail`**: `create_dir_all` or `write` failed (typically a
  read-only state dir).
* **Typical fix-hint**: verify `$XDG_STATE_HOME` is writable for the
  current user.

#### `file.plugins.registry`

* **Status `ok`**: `crate::plugin::registry::discover()` returns at
  least one plugin AND the canary `sy-plugin-md` is in the list.
* **Status `warn`**: registry discovers plugins but the canary is
  absent (the markdown previewer is not installed).
* **Status `fail`**: registry discover returned an error OR no
  plugins were discovered at all.
* **Typical fix-hint**: `sy plugin install ./crates/sy-plugin-md`.

## `sy plugin doctor`

### Envelope

```json
{
  "schema": "sy.plugin.doctor/v1",
  "checks": [
    {
      "plugin": "sy-plugin-md",
      "name": "manifest.valid",
      "ok": true,
      "detail": "manifest passes SPEC §4.1 validation"
    },
    {
      "plugin": "sy-plugin-md",
      "name": "binary.reachable",
      "ok": true,
      "detail": "/usr/local/bin/sy-plugin-md is executable"
    },
    {
      "plugin": "sy-plugin-md",
      "name": "capability.routes",
      "ok": true,
      "detail": "(previewer, mime=text/markdown, url=__doctor.probe) routes to sy-plugin-md"
    }
  ]
}
```

Field shape:

| Field             | Type    | Description                                                 |
|-------------------|---------|-------------------------------------------------------------|
| `schema`          | string  | Wire-stable marker; pinned at `sy.plugin.doctor/v1`.        |
| `checks[]`        | array   | One entry per `(plugin, check)` pair.                       |
| `checks[].plugin` | string  | Plugin id (matches `[plugin] id` from the manifest).        |
| `checks[].name`   | string  | Stable check id (`manifest.valid`, `binary.reachable`, …).  |
| `checks[].ok`     | boolean | `true` on success, `false` on failure (flips exit code 8).  |
| `checks[].detail` | string  | Human-readable detail. Always present.                      |

Exit code 8 (SPEC §4.5 row 8) is emitted when any `ok` is `false`.
The top-level envelope intentionally does not carry a `status`
roll-up — each row is independent and the exit code is the operator
chokepoint.

### Probes

#### `manifest.valid`

* **`ok = true`**: the manifest passes the SPEC §4.1 validator (same
  code path `sy plugin validate` runs).
* **`ok = false`**: malformed TOML, bad glob, missing required field.

#### `binary.reachable`

* **`ok = true`**: `[plugin.binary] exec` resolves to a file (absolute
  or relative-to-manifest-dir) AND the file's mode bits include at
  least one executable bit.
* **`ok = false`**: missing file, non-file path, or no execute bit.

#### `capability.routes`

* **`ok = true`**: every `[[capability]]` row in the manifest routes
  back to its own plugin. Catches a predicate that compiles but
  matches nothing.
* **`ok = false`**: the registry's `select_for(kind, mime, url)`
  resolved to a different plugin (or `None`) for the probe.

## Schema stability

Both schemas are wire-stable additive-only. The contract is:

* New check rows MAY be appended at the end of the list without
  bumping the major.
* New fields MAY be added to existing rows (e.g. a future
  `severity` enum) provided the existing fields keep their semantics.
* Renaming an existing field, re-casing a status enum, or removing a
  check counts as a breaking change and bumps the major
  (`sy.file.doctor/v2`).

The integration tests at
[`tests/sy_file_doctor.rs::json_schema_stable`](../../tests/sy_file_doctor.rs)
pin the wire shape; a doc revision that breaks the schema must update
that test in lockstep.

## See also

* [sy file mcp](sy-file-mcp.md) — MCP tool list.
* [CLI reference](cli.md) — `--json` and exit codes.
* [How to troubleshoot sy file](../how-to/troubleshoot-sy-file.md)
