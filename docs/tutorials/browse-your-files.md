<!-- Template source: Good Docs Project tutorial template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/tutorial. Diátaxis quadrant: tutorial. -->

# Tutorial: browse files with sy file

## Introduction

In this tutorial you open the built-in file manager, land in your
home directory, and hover a markdown file so the preview pane
renders it. No extra file-manager package is involved: `sy file` is
part of the same binary you already installed.

When you finish, `Mod+E` (or `sy file ~`) opens a three-pane window
and hovering `README.md` shows a preview without spawning Chrome or
prompting the GNOME keyring.

## Prerequisites

- You completed
  [the bring-up tutorial](getting-started.md). `sy.target` is
  active:

  ```bash
  systemctl --user is-active sy.target
  ```

  The command prints `active`.

- You are in a niri session (the compositor `sy apply` configures).
  The keybinding `Mod+E` is part of that session. If you are on a
  different compositor, use the CLI in step 1 instead.

- You can read a markdown file under `$HOME`. This tutorial uses
  `$HOME/README.md` if it exists; any other `.md` file you can read
  is fine.

## Step 1 — Open the window

Press `Mod+E` (the Super/Windows key plus E). niri starts
`sy file ~`.

From a terminal, the same open is:

```bash
sy file ~
```

A three-pane window appears: parent directory, current directory,
preview.

`Mod+Shift+E` opens on the niri process working directory instead
of `$HOME`. `Mod+/` is the same as `Mod+E`.

## Step 2 — Confirm the plane is healthy

```bash
sy file doctor --json
```

The top-level `status` field is `"ok"` when the daemon, font, niri
binds, systemd unit, bookmarks directory, and plugin registry all
pass. If a probe is `fail`, the row's `fix_hint` names the repair
(most often `systemctl --user start sy-file.socket`).

## Step 3 — Hover a markdown file

In the current-directory pane, move to a `.md` file. The preview
pane paints a PNG of the rendered markdown.

There is no Chrome process and no keyring popup. Preview is a
small plugin (`sy-plugin-md`) that `sy apply` already installed.

If the preview stays empty, see
[how to troubleshoot sy file](../how-to/troubleshoot-sy-file.md).

## Verify

1. The window opened on `$HOME` (or the directory you passed).
2. `sy file doctor --json` prints `"status": "ok"`.
3. Hovering markdown fills the preview pane.

## Next steps

- For doctor details, IPC from a shell, and MCP, see
  [how to run sy file](../how-to/run-sy-file.md).
- To ship your own previewer, see
  [how to write a sy plugin](../how-to/write-a-sy-plugin.md).
- To search file *contents* (not names), see
  [search your local files](search-your-files.md).
