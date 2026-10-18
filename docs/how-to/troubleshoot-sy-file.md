<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to troubleshoot sy file

## Goal

Repair the `sy file` failure modes that `sy file doctor` names so the
first-session window opens, the daemon answers IPC, and markdown
preview renders.

## Prerequisites

- You followed [How to run sy file](run-sy-file.md) at least once, or
  `sy.target` is active and you expect the file plane to be up.
- The `sy file doctor --json` envelope from the failing host.

## Steps

Run doctor first. It names the failing probe:

```bash
sy file doctor --json
```

Match the probe (or the IPC exit code) to one section below. If
nothing matches, file an issue against
[`sy`](https://github.com/dmytrogajewski/sy) with that envelope
attached.

### Fix `file.daemon.reachable: fail`

The user-level socket activation unit is not running.

```bash
systemctl --user start sy-file.socket
sy file doctor --json
```

### Fix `sy file ipc open` exit `4` (refused)

The daemon rejected the path (permission, missing directory, or
symlink loop). Pick a path you can `ls` and retry:

```bash
ls "$HOME"
sy file ipc open "$HOME"
sy file ipc state
```

### Understand an empty IPC preview response

`sy file ipc preview` currently reports MIME metadata and an empty
`png_base64` value. This IPC operation does not render an image. The
GUI uses its own preview path for supported text and image files; a
plugin installation does not change the IPC response.

```bash
sy file ipc preview "$HOME/README.md"
```

For the GUI's syntax-highlighted text preview, follow the
[file-manager tutorial](../tutorials/browse-your-files.md).

### Fix `Mod+E` landing on `swaylock` (or any non-`sy file` target)

`file.niri.binds: fail` names the conflicting binding. Re-apply the
binds from the repo:

```bash
sy apply
sy file doctor --json
```

### Investigate SELinux denials

The repository does not currently ship a sy-file-specific SELinux
policy installer. To inspect a denial, collect recent AVC records:

```bash
sudo ausearch -m AVC -ts recent
```

Use the denial details to identify the executable, target path, and
SELinux domains involved. Do not disable SELinux to work around the
denial. Include the AVC record and the command that triggered it when
reporting a sy issue.

## Result

`sy file doctor --json` prints `status=ok` and six probes by name.
`sy file ipc state` returns a `cwd` you can read. The IPC preview
operation reports MIME metadata and returns an empty PNG payload.

## See also

- [How to run sy file](run-sy-file.md)
- [sy file doctor](../reference/sy-file-doctor.md)
- [CLI: `sy file`](../reference/cli.md#sy-file)
