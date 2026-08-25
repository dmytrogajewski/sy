<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to apply a theme

## Goal

Render the `configs/` tree with a named palette and reload the
running niri session so the bar, terminal, and compositor pick up
the colours.

## Prerequisites

- `sy` is installed. You completed
  [the bring-up tutorial](../tutorials/getting-started.md).
- You are logged into a niri session, or you are willing to log out
  and pick **Niri** from the display manager after apply.

## Steps

1. List the palettes shipped under `themes/`:

   ```bash
   sy themes
   ```

2. Preview the diff for a theme without writing:

   ```bash
   sy apply --theme gruvbox-material --dry-run
   ```

   Use `--diff` for the same preview as JSON.

3. Apply:

   ```bash
   sy apply --theme gruvbox-material
   ```

   `sy apply` is idempotent. Re-running it when nothing drifted is a
   no-op.

4. Reload the running session:

   ```bash
   niri msg action load-config-file
   killall -SIGUSR2 waybar
   makoctl reload
   ```

   If you use `sy stack bar` instead of waybar, restart that user
   unit:

   ```bash
   systemctl --user restart sy-stack-bar.service
   ```

## Result

Files under `~/.config/` match the rendered templates for that
theme. The compositor, bar, and notifications show the new palette
without a reboot. To change a colour for good, edit
`themes/<name>.toml` in the repo and apply again — do not hand-edit
`~/.config/`.

## See also

- [Configuration reference](../reference/config.md)
- [Why there are no snowflakes](../explanation/no-snowflakes.md)
- [CLI: `sy apply`](../reference/cli.md#sy-apply)
