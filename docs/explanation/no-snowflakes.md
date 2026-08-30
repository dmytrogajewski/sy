<!-- Template source: Good Docs Project explanation template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/explanation. Diátaxis quadrant: explanation. -->

# Why there are no snowflakes

## Why this exists

A snowflake is a one-off change to the host that lives outside the
repository: a hand-edited `~/.bashrc`, an ad-hoc `systemctl enable`,
a colour tweaked in `~/.config/waybar/style.css` and never committed.
Snowflakes are easy the hour you make them and expensive the month
you reinstall.

`sy` exists so a fresh Fedora 43 laptop plus
`cargo build --release && sy apply` reproduces the machine. That
promise is false the moment any load-bearing state lives only on
disk.

![sy apply renders this git repo onto the laptop](../img/sy-apply.svg)

## How it works

You change the system by changing the repo. Templates under
`configs/` are rendered with the active theme from `themes/` and
written to the paths they already mirror. Units under
`configs/systemd/user/` are symlinked into the user manager. The
binary itself hosts the planes. There is no third place.

`sy apply --dry-run` shows the diff first. `sy diff` is the same
idea: pending changes are visible, then you apply them. Destructive
overwrites need `--yes`.

The rule is not "never use the shell". It is "never leave the shell
as the source of truth". If a package must be installed, it is
listed in a how-to that is itself in the repo, or it is encoded in
a unit, a COPR enable, or a script under `scripts/`. If a keybinding
must change, it changes in `configs/niri/config.kdl` and you re-apply.

## Trade-offs

- **Slower first tweak, faster tenth reinstall.** Changing one
  colour is an edit plus apply, not a one-line patch in `~/.config`.
  Recreating the laptop is a clone plus build plus apply.
- **The repo is the backup.** What is not in `configs/` or in `sy`
  is not part of the system. That is strict on purpose.
- **Fedora-shaped, not distro-neutral.** The no-snowflakes rule is
  cheaper when the target OS is one version of one distribution.

## Alternatives we considered

- **A dotfile manager that copies files but does not own units,
  PAM, or the NPU daemon.** It covers the desktop look and leaves the
  privileged planes as snowflakes. Rejected: the planes *are* the
  product.
- **Document the manual steps and ask people to be careful.** That
  is how snowflakes accumulate. Rejected: the rule has to be
  mechanical (`sy apply`) or it is a wish.

## See also

- [What sy is](what-sy-is.md)
- [Configuration reference](../reference/config.md)
- [How to apply a theme](../how-to/apply-a-theme.md)
- [How the planes fit together](architecture.md)
