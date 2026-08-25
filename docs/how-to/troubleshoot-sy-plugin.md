<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to troubleshoot a sy file plugin

## Goal

Repair a previewer plugin that `sy plugin doctor` reports as invalid,
unreachable, shadowed, or crashing mid-preview.

## Prerequisites

- You followed [How to write a sy plugin](write-a-sy-plugin.md) and
  ran `sy plugin install`.
- The `sy plugin doctor --json` envelope from the failing host.

## Steps

Run doctor first. Each installed plugin contributes three rows:
`manifest.valid`, `binary.reachable`, and `capability.routes`.

```bash
sy plugin doctor --json
```

Match the false row to one section below.

### Fix `manifest.valid: false`

Re-read the manifest schema (required `api`, `[plugin]` table, known
`kind` values). Common failures: missing `api`, missing `[plugin]`,
or a `[[capability]]` row with an unknown `kind`. Fix the manifest
next to the binary and reinstall:

```bash
sy plugin install ./
sy plugin doctor --json
```

### Fix `binary.reachable: false`

The `exec` path in the manifest does not resolve relative to the
manifest directory, or the binary lacks the execute bit:

```bash
chmod +x ./echo_previewer
sy plugin install ./
sy plugin doctor --json
```

### Fix `capability.routes: false`

Another plugin claims the same `(kind, mime)` tuple. List installed
ids and either uninstall the conflict or differentiate your MIME
(or raise `priority` on your `[[capability]]` row):

```bash
sy plugin list --json
```

### Fix a plugin process that crashes mid-preview

The host supervisor retries with exponential backoff (100 ms →
200 ms → 400 ms) and then marks the plugin `Unhealthy`. Read the
crash reason:

```bash
journalctl --user -t sy-file
```

## Result

All three doctor rows for your plugin id are `"ok": true`. Preview
requests for that MIME return a body instead of an empty envelope.

## See also

- [How to write a sy plugin](write-a-sy-plugin.md)
- [sy file doctor](../reference/sy-file-doctor.md)
- [plugin SPEC](../../specs/research/sy-file-manager-plugins/SPEC.md)
