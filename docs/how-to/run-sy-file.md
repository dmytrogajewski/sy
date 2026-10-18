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
   `fail`, follow the `fix_hint` it prints. The common socket fix is
   `systemctl --user start sy-file.socket`. For SELinux denials, see
   [troubleshooting](troubleshoot-sy-file.md#investigate-selinux-denials).

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

5. Inspect the preview IPC response for a markdown file:

   ```bash {.no-test}
   sy file ipc preview "$HOME/README.md"
   ```

   The current IPC operation reports the detected MIME and returns
   an empty `png_base64` field. It does not return the GUI's
   syntax-highlighted text preview. A successful response confirms
   MIME detection and daemon health, not rendered image output.

## Result

`sy file doctor --json` returns `status=ok` and six probes by name.
After step 3, `cwd` equals `$HOME`. The preview IPC call returns the
detected MIME and an empty PNG payload.

If a probe fails, see [How to troubleshoot sy file](troubleshoot-sy-file.md).

## See also

- [Tutorial: browse files with sy file](../tutorials/browse-your-files.md)
- [How to troubleshoot sy file](troubleshoot-sy-file.md)
- [sy file doctor](../reference/sy-file-doctor.md)
- [sy file MCP](../reference/sy-file-mcp.md)
- [CLI: `sy file`](../reference/cli.md#sy-file)
