<!-- Template source: Good Docs Project reference template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/reference. Diátaxis quadrant: reference. -->

# Configuration reference

Where `sy` reads configuration, in what order, and which files
`sy apply` writes.

This page lists shapes the source toolchain does not extract.
Per-flag env vars live with each subcommand in
[the CLI reference](cli.md).

## Synopsis

```text
flags  >  environment  >  config file  >  built-in defaults
```

Every flag that accepts an env var in clap is also settable via
that variable. Names start with `SY_` unless they are standard
XDG variables.

## Files

| Path | Role |
|------|------|
| `sy.toml` (repo root, copied into the target) | Active theme, registered knowledge sources, knowledge schedule, and other host knobs `sy` itself owns. |
| `themes/<name>.toml` | Colour palette injected into every `configs/**` template at render time. |
| `configs/` | Minijinja templates. Directory layout mirrors `~/.config/`. |
| `configs/sy/power.toml` | Power-governor profiles, bandit arms, EPP / governor rules. |
| `configs/sy/intent_whitelist.toml` | Agent-runner intent whitelist; also the `[call].who` substrings for "in a call" detection. |
| `configs/systemd/user/` | User units grouped by `sy.target`. `sy apply` symlinks them into `~/.config/systemd/user/`. |
| `configs/systemd/system/` | Rare system-level units (NPU, Spark agent/executor, syauth PAM helpers). |

Override the repo root with `--root` or `SY_ROOT`. Override the
write target with `--target` or `XDG_CONFIG_HOME`.

## `sy apply` destinations

`sy apply` renders `configs/` into:

- `~/.config/` (or `$XDG_CONFIG_HOME`) for user session files
- `~/.local/share/` where a template says so
- `~/.config/systemd/user/` for unit symlinks, then
  `systemctl --user daemon-reload`

It does not hand-edit files that already exist as untracked
snowflakes outside those templates. If a destination is a regular
file that is not the rendered output, `--yes` is required to
overwrite.

## Environment variables

Load-bearing globals (full per-command list is in
[the CLI reference](cli.md)):

| Name | Meaning |
|------|---------|
| `SY_ROOT` | Repo root for template rendering. Same as `--root`. |
| `XDG_CONFIG_HOME` | Default `--target` for `sy apply`. |
| `XDG_STATE_HOME` | Per-plane state (`crash`, `knowledge`, `power`). |
| `XDG_RUNTIME_DIR` | Unix sockets (`agt`, `ipc`, `power`, `mon`, `file`). |
| `NO_COLOR` | Disable ANSI when set (CLIG). |
| `SY_POWER_REPORT_TIMESTAMP` | Pin the PDF report clock for byte-identical output. |
| `SY_POWER_REPORT_MODEL_SHA` | Pin the PDF report model id for byte-identical output. |
| `SY_KB_*` | Knowledge-search filters; flags override these. |

## Knowledge sources in `sy.toml`

`sy knowledge add <path>` records the path in `sy.toml` so it
survives reboot. `--disabled` records the source but skips it
until you enable it. `--discover` treats the path as a root of
per-folder `qdr.toml` manifests instead of indexing the whole
tree.

Do not register sources by editing `~/.config/` copies. Edit the
repo (or use the CLI) and apply.

## Intent whitelist

`~/.config/sy/intent_whitelist.toml` is materialised by the
installer from `configs/sy/intent_whitelist.toml`. New "in a call"
triggers go in `[call].who` (case-insensitive substring match on
the logind inhibitor `Who` field). A missing or malformed file
falls back to an empty whitelist: call detection goes quiet rather
than crashing the daemon.

## Examples

```bash
sy apply --dry-run
sy apply --theme gruvbox-material
sy render waybar/style.css          # one template to stdout
export SY_ROOT=~/sources/sy
sy knowledge add ~/Documents/notes
```

## See also

- [How to apply a theme](../how-to/apply-a-theme.md)
- [Why there are no snowflakes](../explanation/no-snowflakes.md)
- [CLI: `sy apply`](cli.md#sy-apply)
