<!-- Template source: Good Docs Project tutorial template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/tutorial. Diátaxis quadrant: tutorial. -->

# Tutorial: bring up sy on a fresh Fedora 43 laptop

## Introduction

In this tutorial you turn a stock Fedora 43 install into a `sy`
workstation: clone the repo, install the Fedora packages the niri
session needs, build the binary, apply every config, and start the
user-level supervisor.

When you finish, `sy.target` is active, `sy doctor` is green (or
warn-only on optional hardware), and `sy --help` lists the planes.
You have a desktop that matches this git repo, search and a file
manager ready to use, and a CLI an agent can call. You have not
been required to own an NPU, a Spark, or a phone.

What you are installing is described in
[What sy is](../explanation/what-sy-is.md). This page is only the
bring-up.

A *plane* is one long-running service the `sy` binary hosts — search
(`knowledge`), power, the file manager, and so on. `sy apply` is the
command that renders `configs/` onto disk and reloads systemd so the
machine matches the repo.

You do **not** need an AMD NPU or a DGX Spark for this tutorial.

## Prerequisites

- Fedora 43 Workstation, fully updated (`sudo dnf upgrade --refresh`).
- An x86_64 CPU.
- A working network connection.
- `git`, `curl`, `unzip`, `make`, `gcc`, and the Rust toolchain
  (`rustup`, stable channel):

  ```bash
  sudo dnf install -y git curl unzip make gcc
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source "$HOME/.cargo/env"
  ```

- `sudo` on the host.

## Step 1 — Clone the repository

The rest of this tutorial (and several how-tos) assume
`~/sources/sy`:

```bash
mkdir -p ~/sources
git clone https://github.com/dmytrogajewski/sy.git ~/sources/sy
cd ~/sources/sy
```

## Step 2 — Enable the Fedora COPR repositories sy depends on

The session uses `niri` (Wayland compositor) and `i3status-rust`
(status backend) from two COPRs:

```bash
sudo dnf copr enable -y avengemedia/dms
sudo dnf copr enable -y atim/i3status-rust
```

## Step 3 — Install the Fedora package prerequisites

```bash
sudo dnf install -y \
  niri waybar mako fuzzel foot swaylock swayidle wlsunset \
  wl-clipboard brightnessctl playerctl pavucontrol \
  network-manager-applet lxpolkit gnome-themes-extra \
  xdg-desktop-portal-gnome xdg-desktop-portal-gtk \
  i3status-rust
```

Install JetBrainsMono Nerd Font so bar glyphs render:

```bash
mkdir -p ~/.local/share/fonts/JetBrainsMono
curl -fL -o /tmp/JBM.zip \
  https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip
unzip -q -o /tmp/JBM.zip -d ~/.local/share/fonts/JetBrainsMono '*.ttf'
rm /tmp/JBM.zip
fc-cache -f
```

Install the clipboard helper. The file manager is `sy file` (built
with the rest of the binary); do not install yazi.

```bash
sudo dnf install -y golang
GOBIN=~/.local/bin go install go.senan.xyz/cliphist@latest
```

## Step 4 — Build the sy binary

`make install` builds a release binary, copies it to `~/.local/bin`,
and runs `restorecon` when SELinux is enforcing:

```bash
make install
```

If `sy` is not found, add `~/.local/bin` for this shell:

```bash
export PATH="$HOME/.local/bin:$PATH"
sy --help
```

You see the top-level command list (`apply`, `aiplane`, `knowledge`,
`power`, `file`, `doctor`, and the others). Persist PATH the way
your distro already does (a login-shell profile), or log out and
back in if `~/.local/bin` is already on PATH via systemd user
environment. Do not treat a one-off `~/.bashrc` edit as the source
of truth for the rest of `sy` — that is what `sy apply` is for.

## Step 5 — Apply the sy configs

`sy apply` renders every template under `configs/` to its
destination, symlinks user units into `~/.config/systemd/user/`, and
runs `systemctl --user daemon-reload`:

```bash
cd ~/sources/sy
sy apply --dry-run
sy apply
```

## Step 6 — Bring up sy.target

`sy.target` is the user-level systemd target that starts the planes
at login:

```bash
systemctl --user enable --now sy.target
```

That pulls in units such as `sy-agentd.service`,
`sy-knowledge.service`, `sy-qdrant.service`, and `sy-stack-bar.service`.
The NPU plane (`aiplane`) joins the same
group; it binds `/dev/accel/accel0` only if that device exists.

## Step 7 — Run sy doctor

```bash
sy doctor
echo $?
```

`0` means every check passed. `3` means warn-only (optional probes,
often missing NPU hardware). `1` means a real failure — read the
failing row and `journalctl --user -u sy.target`.

Machine-readable:

```bash
sy doctor --json
```

## Verify

1. `sy.target` is active:

   ```bash
   systemctl --user is-active sy.target
   ```

   The command prints `active`.

2. `sy doctor` exits `0` or `3`:

   ```bash
   sy doctor
   echo $?
   ```

   Treat `3` as success for this tutorial if the warnings are about
   hardware you do not have (NPU, Spark, phone). Treat `1` as a
   failure to fix before you continue.

3. The binary answers:

   ```bash
   sy --help
   ```

When those three hold, the planes are up and you can use `sy`.

## Next steps

- [Start here](../intro.md) — map of the rest of the docs.
- [What sy is](../explanation/what-sy-is.md) — the product story.
- [Search your local files](search-your-files.md)
- [Browse files with sy file](browse-your-files.md)
- [Drive sy from an agent](drive-sy-from-an-agent.md)
- [Set up the NPU](../how-to/set-up-npu.md) — only if you have Ryzen AI
  hardware.
- [Install the Spark agent](../how-to/install-spark.md) — only if you
  have a DGX Spark.
- [Unlock sudo with your phone](syauth-setup.md) — only if you have
  an Android phone.
- [CLI reference](../reference/cli.md)
- [How the planes fit together](../explanation/architecture.md)
