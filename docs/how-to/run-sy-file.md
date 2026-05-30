<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to run `sy file` for the first time

## Goal

Open a fresh `sy file` window on a host that already runs `sy`, browse
to a repo, hover a markdown file, and confirm the built-in previewer
renders without spawning chrome or touching the gnome-keyring. This
walks the first three beats (J1, J2, J3) of the
[first-session journey](../../specs/journeys/JOURNEY-20260527-0215-sy-file-first-session.md)
end to end — once the doctor returns `status=ok`, the same window
unlocks beats J4 through J8 (knowledge search, multi-select, copy,
tile-shrink reflow, agent IPC mirror) without further setup.

`sy file` is the in-tree replacement for the previous yazi-based
file manager. The pure-Rust previewer pipeline removes the chrome
dependency that the [`md-rich.yazi`](../../specs/research/sy-file-manager/SPEC.md)
episode tripped on. See [`sy file doctor`](../reference/sy-file-doctor.md)
for the wire-stable schema this how-to assumes, and
[`sy file mcp`](../reference/sy-file-mcp.md) for the agent surface
that mirrors every step below over JSON-RPC.

## Prerequisites

- `sy` is installed and `sy.target` is running under your user systemd
  manager (`systemctl --user is-active sy.target` prints `active`).
  If not, see [the getting-started tutorial](../tutorials/getting-started.md).
- The `SY_FILE_SOCK`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`,
  `XDG_DATA_HOME`, and `SY_PLUGIN_DIR` env vars are either unset
  (production defaults) or pointing at writable directories. The
  blocks below assume the standard `XDG` layout; substitute your
  override paths if you run `sy` from a non-default prefix.
- You have read access to a markdown file under your `$HOME` —
  `README.md` in a checked-out repo works.

## Steps

1. Confirm the `sy file` plane is reachable on the productivised
   socket. The doctor sub-command exits `0` and prints a JSON envelope
   whose top-level `status` field is `"ok"` when every probe passes:

   ```bash
   sy file doctor --json
   ```

   The envelope is documented at
   [`docs/reference/sy-file-doctor.md`](../reference/sy-file-doctor.md);
   the journey-J1 acceptance is `status=ok`. If any probe is `fail`,
   run the fix-hint it prints (`systemctl --user start sy-file.socket`
   is the most common; for SELinux-gated plugin spawn,
   `make install-system-sy-plugin-selinux`).

2. Confirm the same envelope reports six checks (one per SPEC §3.3
   item 19 probe — `file.daemon.reachable`,
   `file.fonts.jetbrainsmono_nerd`, `file.niri.binds`,
   `file.systemd.unit_installed`, `file.bookmarks.writable`, and
   `file.plugins.registry`). Pipe doctor output through `jq` so the
   per-probe rows print one per line:

   ```bash
   sy file doctor --json | grep -o '"name":"[^"]*"'
   ```

3. Open a known-good directory through the daemon. On a niri host
   with the productivised binds (Step 34 of the roadmap), `Mod+E`
   spawns the same call: it opens `$HOME`, `Mod+Shift+E` opens the
   niri process cwd, and `Mod+Slash` is a duplicate of `Mod+E` kept
   for the slash-then-letter keymap muscle memory. Running the IPC
   command from a shell is how non-niri environments reach the same
   surface:

   ```bash {.no-test}
   sy file --ipc open "$HOME"
   ```

4. Confirm the daemon's current pane points at your home directory.
   The `state` op is read-only and machine-parseable; agents and the
   waybar pill consume the same envelope:

   ```bash {.no-test}
   sy file --ipc state
   ```

   The `cwd` field of the response must equal `$HOME`. If it does not,
   the daemon either rejected the path (permission error — surfaced
   on stderr) or another `sy file` instance retargeted the pane
   between Step 3 and Step 4.

5. Hover a markdown file to trigger the built-in previewer. The
   in-tree `sy-plugin-md` (SPEC §3.3 item 18) renders markdown to
   PNG via the pure-Rust `pulldown-cmark` → `cosmic-text` →
   `tiny-skia` pipeline. There is no chrome process, no terminal
   image protocol, no keyring popup. The `preview` op returns the
   PNG body as base64 inside the response envelope:

   ```bash {.no-test}
   sy file --ipc preview "$HOME/README.md"
   ```

   If `$HOME/README.md` does not exist on your host, substitute any
   other markdown file you have read access to. The preview response
   carries `mime = "image/png"` once the previewer plugin resolves
   the markdown previewer; until then the response carries the MIME
   the daemon sniffed from the path without a PNG body.

## Verify

`sy file doctor --json` returns `status=ok` (Step 1) and surfaces all
six probes by name (Step 2). After Step 3, the daemon's `cwd` equals
`$HOME` (Step 4). The preview envelope from Step 5 either carries a
`mime` field set to `image/png` (full preview path) or an empty
`png_base64` body with `mime` set to `text/markdown` (plugin
dispatcher not yet wired — the daemon still rendered the MIME sniff
without crashing). Either outcome is a green journey-J3 beat: the
first session reached "hover a markdown file" without hitting the
chrome / keyring failure mode the yazi-based previewer suffered from.

## Troubleshooting

- **`sy file doctor` prints `file.daemon.reachable: fail`** — the
  user-level socket activation unit isn't running. Start it with
  `systemctl --user start sy-file.socket` and re-run doctor.
- **`sy file --ipc open` exits `4` (refused)** — the daemon rejected
  the path (permission, non-existent directory, or symlink loop).
  Pick a path you can `ls` and retry.
- **Preview body is empty for every file** — the markdown plugin is
  not installed. Install the in-tree canary with
  `sy plugin install ./crates/sy-plugin-md` and re-run Step 5.
- **niri's `Mod+E` lands on `swaylock` (or any non-`sy file`
  target)** — `file.niri.binds: fail` will name the conflicting
  binding. Re-run `sy apply` to re-render the productivised binds.
- **Anything else** — file an issue against
  [`sy`](https://github.com/dmytrogajewski/sy) with the
  `sy file doctor --json` envelope attached.
