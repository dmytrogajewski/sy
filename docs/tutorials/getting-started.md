<!-- Template source: Good Docs Project tutorial template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/tutorial. Diátaxis quadrant: tutorial. -->

# Tutorial: bring up sy on a fresh Fedora 43 laptop

## Introduction

In this tutorial you turn a stock Fedora 43 install into an
agent-first workstation by building and applying `sy`. You clone
the repo, install the Fedora prerequisites the rice depends on,
build the single Rust binary, render every config under `configs/`
into your home directory with `sy apply`, hand control of the
planes to your user systemd manager via `sy.target`, and finally
confirm the bring-up with `sy doctor` and `sy aiplane status`.

When you finish, your user systemd manager supervises every `sy`
plane (aiplane, knowledge, qdrant, agentd, powerd, stack-bar),
`sy doctor` returns exit code `0`, and `sy aiplane status` reports
the NPU plane as up.

A *plane* is one of the long-running services `sy` ships — for
example the NPU inference daemon (`aiplane`), the semantic
search index (`knowledge`), or the adaptive power governor
(`power`). Every plane is a child unit of `sy.target` and speaks
the same JSON-over-stdio surface, so an agent can drive any plane
the same way you do.

`sy apply` is the single command that renders the templates under
`configs/` into the right paths on disk (`~/.config/`,
`~/.local/share/`, `~/.config/systemd/user/`, plus the few
system-level units), reloads systemd, and converges your machine
on the contents of the repo.

## Prerequisites

- Fedora 43 Workstation, freshly installed, fully updated
  (`sudo dnf upgrade --refresh`).
- An x86_64 CPU. AMD Ryzen AI hardware is optional for this
  tutorial — the NPU one-time setup is covered in a separate
  how-to and is not required for the planes to come up.
- A working network connection.
- `git`, `curl`, `unzip`, `make`, `gcc`, and the Rust toolchain
  available through `rustup` (stable channel). Install with:

  ```bash
  sudo dnf install -y git curl unzip make gcc
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source "$HOME/.cargo/env"
  ```

- Your shell has `~/.local/bin` and `~/.cargo/bin` on `$PATH`. Add
  the following to `~/.bashrc` if it is not already there:

  ```bash
  export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
  ```

- `sudo` privileges on the host.

## Step 1 — Clone the repository

Clone `sy` into `~/sources/sy`. The rest of the tutorial assumes
that path because the repo's own scripts reference it.

```bash
mkdir -p ~/sources
git clone https://github.com/dmytrogajewski/sy.git ~/sources/sy
cd ~/sources/sy
```

## Step 2 — Enable the Fedora COPR repositories sy depends on

The rice pulls `niri` (the Wayland compositor `sy` targets) and
`i3status-rust` (the waybar status backend) from two COPR
repositories. Enable both:

```bash
sudo dnf copr enable -y avengemedia/dms
sudo dnf copr enable -y atim/i3status-rust
```

## Step 3 — Install the Fedora package prerequisites

Install `niri`, the waybar stack, and every package the rendered
configs under `configs/` reference:

```bash
sudo dnf install -y \
  niri waybar mako fuzzel foot swaylock swayidle wlsunset \
  wl-clipboard brightnessctl playerctl pavucontrol \
  network-manager-applet lxpolkit gnome-themes-extra \
  xdg-desktop-portal-gnome xdg-desktop-portal-gtk \
  i3status-rust
```

Install JetBrainsMono Nerd Font so the bar glyphs render:

```bash
mkdir -p ~/.local/share/fonts/JetBrainsMono
curl -fL -o /tmp/JBM.zip \
  https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip
unzip -q -o /tmp/JBM.zip -d ~/.local/share/fonts/JetBrainsMono '*.ttf'
rm /tmp/JBM.zip
fc-cache -f
```

Install the two helpers that `sy` shells out to:

```bash
cargo install --locked --force yazi-build
rm -f ~/.cargo/bin/yazi-build
sudo dnf install -y golang
GOBIN=~/.local/bin go install go.senan.xyz/cliphist@latest
```

## Step 4 — Build the sy binary

Build a release binary in-tree and copy it onto your `$PATH`. The
project's `make install` target does both, plus a SELinux
`restorecon` pass when the system is in enforcing mode:

```bash
make install
```

Confirm the binary runs:

```bash
sy --help
```

You see the top-level command list (`apply`, `aiplane`,
`knowledge`, `power`, `doctor`, and the others).

## Step 5 — Apply the sy configs

Run `sy apply`. The command renders every template under
`configs/` to its destination on disk (the directory layout
mirrors `~/.config/`), symlinks user-level systemd units into
`~/.config/systemd/user/`, and runs `systemctl --user
daemon-reload`:

```bash
cd ~/sources/sy
sy apply
```

If you want to preview the changes before they land, prepend a
dry run on a separate invocation:

```bash
sy apply --dry-run
```

## Step 6 — Bring up sy.target

`sy.target` is the user-level systemd target that supervises every
`sy` plane. Enable it so it starts at every login, and start it
now:

```bash
systemctl --user enable --now sy.target
```

Behind the scenes systemd starts the user-level units `sy.target`
pulls in: `sy-agentd.service`, `sy-knowledge.service`,
`sy-qdrant.service`, `sy-powerd.service`, and
`sy-stack-bar.service`. The NPU inference plane (`aiplane`) joins
the same group; the daemon binds `/dev/accel/accel0` on first use.

## Step 7 — Run sy doctor

`sy doctor` runs the cross-plane health probes. On a successful
bring-up it exits `0`:

```bash
sy doctor
```

To inspect the structured output, ask for JSON:

```bash
sy doctor --json
```

## Verify

Confirm three things, in order:

1. `sy.target` is active:

   ```bash
   systemctl --user is-active sy.target
   ```

   The command prints `active` on success.

2. `sy doctor` exits `0`:

   ```bash
   sy doctor
   echo $?
   ```

   The exit code is `0` (all checks pass). Exit code `3` means
   warn-only drift and is acceptable for the optional probes, but
   the canonical success state is `0`.

3. `sy aiplane status` reports the NPU plane as up:

   ```bash
   sy aiplane status
   ```

   The human-readable output names the registered workloads and
   the active hardware backend. `sy aiplane status --json` gives
   you the same data on stdout for scripting.

When all three checks pass, the planes are up under your user
systemd manager and `sy` is ready to drive.

## Next steps

- To run on-device NPU inference on AMD Ryzen AI hardware, see
  [how to set up the NPU](../how-to/set-up-npu.md).
- To wire up phone-as-key sudo with `syauth`, see
  [the syauth tutorial](syauth-setup.md).
- For the full CLI surface, see
  [the CLI reference](../reference/cli.md).
- For the mental model of how the planes fit together, see
  [the architecture explanation](../explanation/architecture.md).
