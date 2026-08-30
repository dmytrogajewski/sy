<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to run sy file from a shell

## Goal

Confirm the file-manager daemon is healthy, open a directory over
IPC, and preview a markdown file — the same operations the window
uses, without needing niri keybindings.

If you have never opened the window, start with
[browse files with sy file](../tutorials/browse-your-files.md).

## Prerequisites

- `sy` is installed and `sy.target` is active
  (`systemctl --user is-active sy.target` prints `active`). If not,
  see [the bring-up tutorial](../tutorials/getting-started.md).
- `SY_FILE_SOCK`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`,
  `XDG_DATA_HOME`, and `SY_PLUGIN_DIR` are unset (production
  defaults) or point at writable directories.
- You can read a markdown file under `$HOME`.

## Steps

1. Confirm the file plane is healthy. Doctor exits `0` and prints a
   JSON envelope whose top-level `status` is `"ok"` when every probe
   passes:

   ```bash
   sy file doctor --json
   ```

   The envelope is documented at
   [sy file doctor](../reference/sy-file-doctor.md). If any probe is
   `fail`, run the `fix_hint` it prints (`systemctl --user start
   sy-file.socket` is the most common; for SELinux-gated plugin
   spawn, `make install-system-sy-plugin-selinux`).

2. Confirm the same envelope reports six checks (`file.daemon.reachable`,
   `file.fonts.jetbrainsmono_nerd`, `file.niri.binds`,
   `file.systemd.unit_installed`, `file.bookmarks.writable`, and
   `file.plugins.registry`):

   ```bash
   sy file doctor --json | grep -o '"name":"[^"]*"'
   ```

3. Open a known-good directory through the daemon. On niri,
   `Mod+E` does the same for `$HOME`. From a shell:

   ```bash {.no-test}
   sy file ipc open "$HOME"
   ```

4. Confirm the current pane points at your home directory:

   ```bash {.no-test}
   sy file ipc state
   ```

   The `cwd` field must equal `$HOME`. If it does not, the daemon
   rejected the path (permission error on stderr) or another client
   retargeted the pane between steps 3 and 4.

5. Preview a markdown file. The built-in `sy-plugin-md` previewer
   renders to PNG in-process (no Chrome, no keyring):

   ```bash {.no-test}
   sy file ipc preview "$HOME/README.md"
   ```

   Substitute any other markdown file you can read. A full preview
   carries `mime = "image/png"`. Until the plugin resolves, you may
   see the sniffed MIME without a PNG body — that is still a
   successful call if the daemon did not crash.

## Result

`sy file doctor --json` returns `status=ok` and six probes by name.
After step 3, `cwd` equals `$HOME`. Preview returns a PNG MIME or a
safe MIME sniff.

If a probe fails, see [How to troubleshoot sy file](troubleshoot-sy-file.md).

## See also

- [Tutorial: browse files with sy file](../tutorials/browse-your-files.md)
- [How to troubleshoot sy file](troubleshoot-sy-file.md)
- [sy file doctor](../reference/sy-file-doctor.md)
- [sy file MCP](../reference/sy-file-mcp.md)
- [CLI: `sy file`](../reference/cli.md#sy-file)
