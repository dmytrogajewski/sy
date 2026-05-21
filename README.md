<img src="assets/sy.png" alt="sy mascot — a tiny kitten" align="right" width="220" />

# sy

An **Agentic OS layer for Fedora** — a single Rust binary plus
declarative configs that turn a stock Fedora 43 laptop into an
agent-first workstation. One repo, one source of truth, zero
snowflakes: `cargo build --release && ./target/release/sy apply` on a
fresh machine reproduces the entire system.

```
sy apply              # render configs/* → ~/.config/, ~/.local/share/, /etc/* (via diff)
sy aiplane daemon     # on-device NPU inference plane (ORT + VitisAI EP)
sy agt …              # sandboxed agent runner
sy knowledge daemon   # semantic search over local files (NPU-accelerated)
sy power status       # adaptive power governor (ppd shim + bandit + MCP)
sy auto               # auto-configure MCP servers across agents (Claude, …)
sy stack bar          # layer-shell waybar replacement
sy syauth doctor      # phone-as-key sudo (PAM + BlueZ + Android)
…
```

`sy` is the orchestrator. Everything below is a
[*plane*](docs/reference/glossary.md#plane) exposed by the same
binary, supervised by the same user-level `sy.target`, and reachable
over the same CLIG + JSON-over-stdio surface (so an agent can drive
any plane the same way a human can). See [`CLAUDE.md`](CLAUDE.md)
for the "no snowflakes" rule that drives the single-binary choice
and [`AGENTS.md`](AGENTS.md) for the coding-agent persona. For
one-line definitions of `sy`-specific terms (plane, aiplane,
re-exec dance, snowflake, VitisAI EP, …) see the
[glossary](docs/reference/glossary.md).

## Planes

### `aiplane` — on-device NPU inference

The privileged process that owns `/dev/accel/accel0` and hosts every
on-device ML workload behind a JSON-over-Unix-socket IPC. Workloads
declare their EP preference (`Vitisai | Cpu`); the session pool picks
what to load at start-up.

- One process per NPU: `/dev/accel/accel0` is single-context. CLI and
  MCP consumers delegate over IPC; no second ORT session is ever
  spun up "just to be safe."
- Re-exec dance: `aiplane::reexec` sets `LD_LIBRARY_PATH`,
  `ORT_DYLIB_PATH` and the `RYZEN_AI_*` env before any thread spawns
  so the AMD venv loads correctly.
- Workloads ship under `src/aiplane/workloads/` — currently `embed`
  (multilingual-e5-base, 768-dim), with `rerank | vad | stt | ocr`
  scaffolds.
- A `workloads::fake` impl returns deterministic vectors so daemon
  tests run on CI without `/dev/accel/accel0`.

### `agt` — sandboxed agent runner

`sy agt` runs coding/inference agents under an intent-whitelisted
sandbox. The whitelist lives in [`configs/sy/intent_whitelist.toml`](configs/sy/intent_whitelist.toml);
the runner enforces it before dispatching tool calls. Used by `sy auto`
to plumb MCP servers into Claude / Cursor / Codex / Gemini configs
without each tool re-implementing tool-permission policy.

### `knowledge` — semantic search

In-process vector index over your local files, served by an embedded
**qdrant** + the `aiplane` embed workload. Sources are registered
once, then incrementally synced on a schedule. Exposes the search via
CLI and via an MCP server (`sy knowledge mcp`) auto-registered with
your agents by `sy auto`.

```
sy knowledge add ~/Documents/notes
sy knowledge daemon
sy knowledge search "rust async cancellation"
sy knowledge status --json
```

### `power` — adaptive power governor

The `sy-powerd` user daemon (under `src/power/`) is a power-profiles-daemon
shim with an adaptive layer on top: cpufreq governor selection, EPP,
turbo, and `net.hadess.PowerProfiles` D-Bus name ownership are all
managed declaratively from [`configs/sy/power.toml`](configs/sy/power.toml).
A contextual bandit picks profile transitions; every decision is
journaled and reachable over MCP (`sy power mcp`).

```
sy power status            # current profile, source, rationale
sy power apply             # apply config rules
sy power show --json       # full snapshot (governor, EPP, bandit weights)
```

### Rice — niri + waybar + …

Gruvbox Material Dark Medium. Configs live as
[minijinja](https://github.com/mitsuhiko/minijinja) templates under
`configs/`, driven by the active theme under `themes/`. `sy apply`
renders them into `~/.config/`. Directory layout mirrors `~/.config/`,
so each subfolder maps 1:1 to its destination.

```
configs/
├── niri/config.kdl
├── waybar/{config.jsonc,style.css,modules/}
├── i3status-rust/config.toml
├── mako/config
├── fuzzel/fuzzel.ini
├── foot/foot.ini
├── swaylock/config
└── yazi/{package.toml,theme.toml,yazi.toml,keymap.toml,init.lua}
```

Stack:

| Role             | Tool            | Source                          |
|------------------|-----------------|---------------------------------|
| Compositor       | niri            | COPR `avengemedia/dms` (or `dnf`) |
| Bar frontend     | waybar          | `dnf`                           |
| Bar content      | i3status-rs     | COPR `atim/i3status-rust`       |
| Launcher         | fuzzel          | `dnf`                           |
| Terminal         | foot            | `dnf`                           |
| Notifications    | mako            | `dnf`                           |
| File manager     | yazi (+ `ya`)   | `cargo install yazi-build`      |
| Lock             | swaylock        | `dnf`                           |
| Idle             | swayidle        | `dnf` (DPMS via `niri msg`)     |
| Night light      | wlsunset        | `dnf`                           |
| Clipboard hist.  | cliphist        | `go install`                    |
| Polkit agent     | lxpolkit        | `dnf`                           |
| Screenshots      | niri built-in (`screenshot*` actions) | —          |
| Media/brightness | playerctl, brightnessctl | `dnf`                  |
| Wallpaper        | niri `layout.background-color` solid fill |          |

### `syauth` — phone-as-key sudo

`sy syauth` wraps [syauth](https://github.com/dmytrogajewski/syauth)
(PAM module + user daemon + Android app). The waybar pill renders the
bond / adapter / unlock state; `sy syauth install-pam --service sudo`
wires `pam_syauth.so` into `/etc/pam.d/sudo` with the reality-corrected
defaults (`sufficient`, `timeout=8000`); `sy syauth doctor` one-shot-probes
the whole chain (daemon liveness, bonds, key file modes, BlueZ,
systemd unit, audit log, plus the two sy-only fs probes) and returns
0 / 2 / 1 (all-ok / warn-only / any-fail).

Full setup walkthrough — six steps from a fresh host to
`grantors=pam_syauth` — in
[`docs/tutorials/syauth-setup.md`](docs/tutorials/syauth-setup.md).
Failure-mode fixes are in
[`docs/how-to/troubleshoot-syauth.md`](docs/how-to/troubleshoot-syauth.md);
the PAM module's control flags and arguments are in
[`docs/reference/syauth-pam-module.md`](docs/reference/syauth-pam-module.md).

## Repo layout

```
.
├── Cargo.toml
├── sy.toml                   # active theme + sy config
├── src/                      # sy CLI + planes
│   ├── aiplane/              # NPU plane: registry, session pool, IPC, daemon
│   ├── agt/                  # sandboxed agent runner
│   ├── knowledge/            # qdrant-backed semantic search
│   ├── power/                # adaptive power governor
│   ├── supervision/          # sy.target supervisor + unit linker
│   ├── stack/                # layer-shell waybar replacement
│   ├── doctor/               # cross-plane health probes
│   └── …                     # bat, bright, bt, cal, gpu, npu, net, … (bar tiles)
├── configs/                  # declarative config (rendered by `sy apply`)
│   ├── systemd/{system,user}/*.service|*.target
│   ├── niri/ waybar/ mako/ fuzzel/ foot/ swaylock/ yazi/
│   ├── sy/{power,intent_whitelist}.toml
│   ├── dbus-1/ policy/ selinux/ udev/ modprobe.d/ grub/ dracut/
│   └── …
├── scripts/                  # one-shot helpers (prep_npu_workload.py …)
├── themes/<name>.toml        # palettes
└── specs/                    # journeys, bugs, roadmaps (long-form docs)
```

## Install / apply

### Rice prerequisites

```bash
sudo dnf copr enable -y avengemedia/dms
sudo dnf install -y niri

sudo dnf install -y \
  waybar mako fuzzel foot swaylock swayidle wlsunset \
  wl-clipboard brightnessctl playerctl pavucontrol \
  network-manager-applet lxpolkit gnome-themes-extra \
  xdg-desktop-portal-gnome xdg-desktop-portal-gtk

sudo dnf copr enable -y atim/i3status-rust
sudo dnf install -y i3status-rust

# JetBrainsMono Nerd Font (icons in waybar / mako)
mkdir -p ~/.local/share/fonts/JetBrainsMono
curl -fL -o /tmp/JBM.zip \
  https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip
unzip -q -o /tmp/JBM.zip -d ~/.local/share/fonts/JetBrainsMono '*.ttf'
rm /tmp/JBM.zip
fc-cache -f

cargo install --locked --force yazi-build
rm -f ~/.cargo/bin/yazi-build
GOBIN=~/.local/bin go install go.senan.xyz/cliphist@latest
```

Make sure `~/.local/bin` and `~/.cargo/bin` are in `$PATH`.

### Deploy

```bash
cargo build --release

./target/release/sy apply --dry-run     # preview
./target/release/sy apply               # render and write
./target/release/sy apply --theme gruvbox-material
./target/release/sy themes              # list themes
./target/release/sy render waybar/style.css   # render single file to stdout

# Reload running session
niri msg action load-config-file
killall -SIGUSR2 waybar
makoctl reload

./scripts/yazi-plugins.sh               # yazi plugins + flavor (idempotent)
```

Override target dir with `--target` or `$XDG_CONFIG_HOME`. Override
repo root with `--root` or `$SY_ROOT`. The active theme lives in
`sy.toml`. Log out and pick **Niri** from your display manager, or
start a TTY session with `niri --session`.

`sy apply` symlinks user-level systemd units under
`~/.config/systemd/user/` and runs `systemctl --user daemon-reload`.
`systemctl --user enable --now sy.target` brings every plane (aiplane,
knowledge, power, stack-bar, agentd) up under the user manager.

### NPU one-time setup

1. Install AMD Ryzen AI 1.7.1 system packages via the companion repo
   [`ryzenai-rpm`](https://github.com/dmytrogajewski/ryzenai-rpm)
   (RPM-ified XRT runtime, XDNA DKMS module, memlock config, AMD's
   Python wheel set).

2. Build the model + the NPU compile cache from this repo:

   ```bash
   source /opt/AMD/ryzenai/venv/bin/activate
   python ~/sources/sy/scripts/prep_npu_workload.py
   ```

   Downloads `intfloat/multilingual-e5-base` from HF, exports to
   static-shape ONNX, BF16-quantises with AMD's Quark, and runs a
   one-shot VitisAI compile to produce the `.rai` NPU artifact
   (≈75 s, cached forever). All outputs land in
   `~/.cache/sy/npu-embed/`.

3. Start the daemon:

   ```bash
   sy aiplane daemon            # foreground
   # or
   systemctl --user start sy.target   # supervises every plane
   ```

`sy` auto-detects the AMD venv at startup, re-execs itself with the
right `LD_LIBRARY_PATH` + `ORT_DYLIB_PATH` + `RYZEN_AI_*` env baked
in, and routes embeddings through the NPU.

#### Knowledge hardware tier

| Backend    | Trigger                                | Throughput | VRAM | Notes |
|------------|----------------------------------------|------------|------|-------|
| `vitisai`  | `/opt/AMD/ryzenai/venv` is present     | ~7 chunks/s | 0 GB | Pre-compile cache under `~/.cache/sy/npu-embed/`. Best fit for laptops where you also want to run an LLM on the dGPU. |
| `cuda`     | pip `onnxruntime-gpu==1.24.*` installed, NVIDIA GPU | depends | ≈1 GB | Legacy fastembed path. Sees CUDA libs via `~/.local/lib/python*/site-packages/nvidia/*/lib`. |
| `cpu`      | Always-available fallback              | slow       | 0 GB | Last resort. |

Surface the active backend with `sy knowledge status --json` (look at
`embed_backend`).

#### Why `multilingual-e5-base`, not `-large`?

We originally used `-large` (1024-dim) via fastembed on CUDA. The
NPU port required dropping to `-base` (768-dim) because **VitisAI EP
1.7.1 caps internal ModelProto serialisation at 2 GiB**. `e5-large`
is 2.2 GB FP32 and even after BF16 / INT8 quantization the runtime
upcasts the weights past the cap before partitioning. The MTEB
quality cost is roughly 6 % (64.2 → 60.5 avg) — acceptable for the
"free GPU" trade.

Migration: switching to `-base` is **schema-breaking** (vector dim
1024 → 768). On first start after upgrading run:

```bash
sy knowledge cancel       # stop any in-flight scheduled sync
sy knowledge drop         # drop the old 1024-dim qdrant collection
sy knowledge resync       # rebuild with the new 768-dim embeddings
```

## CLI cheat-sheet

The snippets below are the everyday-driver subset. For the full
per-subcommand reference — every flag, default, env var, exit code,
and example — see [`docs/reference/cli.md`](docs/reference/cli.md).

```bash
# apply / preview the whole system
sy apply [--dry-run] [--theme <name>]
sy diff                            # pending changes
sy doctor                          # cross-plane health probes
sy themes                          # list themes
sy render <relpath>                # render single template to stdout

# aiplane (NPU)
sy aiplane daemon                  # supervises /dev/accel/accel0
sy aiplane run --workload <k>      # one-shot via IPC
sy aiplane status [--json]

# knowledge (semantic search consumer of aiplane)
sy knowledge add <path>            # register a tree (respects .gitignore)
sy knowledge rm <path>             # unregister
sy knowledge schedule 30m          # rewrite [knowledge].schedule
sy knowledge sources               # list registered roots
sy knowledge manifests --json      # active per-folder manifests
sy knowledge daemon                # supervises qdrant + scheduled embed
sy knowledge status [--json]
sy knowledge pause / resume / toggle-pause / cancel
sy knowledge bench --n 1024
sy knowledge search <query>
sy knowledge mcp                   # MCP server (stdio) for AI agents

# power
sy power status [--json]
sy power apply
sy power show --json
sy power mcp

# agent runner
sy agt run <prompt> [--profile <name>]

# auto-configure MCP across agents
sy auto                            # plumbs sy-knowledge / sy-power into Claude, Cursor, Codex, Gemini

# syauth
sy syauth install-pam --service sudo
sy syauth doctor

# stack bar (layer-shell waybar replacement)
sy stack bar
```

Every command supports `--help`, every command that produces output
supports `--json`, every state-changing command supports `--dry-run`,
and every flag is also settable via `SY_*` env var. See [`CLAUDE.md`](CLAUDE.md).

## Keybindings

Mod key is **Super** (Mod4). Full list below.

| Keys                              | Action                                   |
|-----------------------------------|------------------------------------------|
| `Super+Return`                    | Terminal (foot)                          |
| `Super+n`                         | File manager (yazi in foot)              |
| `Super+d` / `Super+Shift+d`       | Launcher / dmenu mode                    |
| `Super+c`                         | Clipboard history (cliphist + fuzzel)    |
| `Super+Escape`                    | Lock screen                              |
| `Super+Shift+q`                   | Close window                             |
| `Super+Shift+c` / `Super+Shift+e` | Reload config / quit niri                |
| `Super+Shift+/`                   | Show hotkey overlay                      |
| `Super+[1-0]`                     | Focus workspace N                        |
| `Super+Shift+[1-0]`               | Move column to workspace N               |
| `Super+Tab`                       | Focus previous workspace                 |
| `Super+[` / `Super+]`             | Workspace up / down (niri is vertical)   |
| `Super+h` / `Super+l`             | Focus column left / right                |
| `Super+j` / `Super+k`             | Focus window within column / next ws     |
| `Super+Shift+<dir>`               | Move column / window                     |
| `Super+b` / `Super+v`             | Consume / expel window right / left      |
| `Super+w`                         | Toggle tabbed display for column         |
| `Super+e`                         | Center focused column                    |
| `Super+f` / `Super+Shift+f`       | Maximize column / true fullscreen        |
| `Super+Shift+space` / `Super+space` | Toggle floating / focus floating↔tiled |
| `Super+r` / `Super+Shift+r`       | Column width +10% / −10%                 |
| `Super+-` / `Super+=`             | Column width −10% / +10%                 |
| `Print` / `Shift+Print`           | Interactive / whole-screen screenshot → clipboard |
| `Super+Print` / `Super+Shift+Print` | Whole-screen / active-window → `~/Pictures/` |
| Volume / brightness / media keys  | Work out of the box                      |

### Niri vs sway

- **Scrollable columns, not binary splits.** `Super+b`/`v` consume/expel
  windows into the current column. Columns scroll horizontally across the output.
- **No stacking layout.** `Super+w` maps to niri's `toggle-column-tabbed-display`.
- **No scratchpad.** `Super+-` / `Super+Shift+-` are repurposed for column-width tweaks.
- **No `focus parent`.** `Super+a` is unbound.
- **Workspaces are vertical.** `Super+[` / `Super+]` scroll up / down.

## Keyboard layout

`niri/config.kdl` defaults to dual `us,ua` with **Alt+Shift** to switch
and Caps Lock remapped to Escape. Change in the
`input { keyboard { xkb { ... } } }` block.

## Theme

The palette lives in `themes/<name>.toml` and is injected into every
`configs/**` file at render time. Edit the `[colors]` table and re-run
`sy apply` to recolor the whole rice.

### Palette (gruvbox-material)

| Name     | Hex       |
|----------|-----------|
| bg       | `#282828` |
| bg_soft  | `#32302f` |
| bg1      | `#3c3836` |
| bg2      | `#504945` |
| fg       | `#ebdbb2` |
| fg_dim   | `#a89984` |
| red      | `#ea6962` |
| orange   | `#e78a4e` |
| yellow   | `#d8a657` |
| green    | `#a9b665` |
| aqua     | `#89b482` (primary accent) |
| blue     | `#7daea3` |
| purple   | `#d3869b` |
| gray     | `#928374` |

## Notes

- **Night light**: `wlsunset` runs with `-l 50.45`. Edit
  `niri/config.kdl` (spawn-at-startup block) to match your latitude.
- **Wallpaper**: solid `#282828` via `layout.background-color`. For
  image-based wallpapers, install `swaybg` and replace with
  `spawn-at-startup "swaybg" "-i" "/path/to/img.png"`.
- **Waybar niri modules**: `niri/workspaces`, `niri/window`,
  `niri/language` (XKB layout indicator). Needs waybar 0.11+.
- **Idle & DPMS**: swayidle uses `niri msg action power-off-monitors`
  / `power-on-monitors` for DPMS.
- **Yazi**: `configs/yazi/` ships the full config — `yazi.toml`
  (previewers + git fetcher), `keymap.toml` (plugin keybindings),
  `init.lua` (guarded `:setup()` calls), `theme.toml` (flavor pin) and
  `package.toml` (32 `ya pkg` deps + the gruvbox flavor). Plugins not
  reachable via `ya pkg` (dual-pane, easyjump, searchjump, whoosh)
  are git-cloned by `scripts/yazi-plugins.sh`. Run that script after
  `sy apply` (or `make yazi-plugins`); it is idempotent and safe to
  re-run on upgrade.

## Contributing

PRs and issues are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md)
for how to file a bug, propose a change, run the test gates
(`make lint`, `make test`), sign your commits (DCO), and the
versioning policy (SemVer + Conventional Commits). Participation in
the project is bound by the [Contributor Covenant 2.1](CODE_OF_CONDUCT.md).
For security-sensitive reports, follow [`SECURITY.md`](SECURITY.md)
instead of opening a public issue.

## Maintainers

- [@dmytrogajewski](https://github.com/dmytrogajewski) — sole
  maintainer. For project questions, prefer a
  [GitHub issue](https://github.com/dmytrogajewski/sy/issues) (or a
  discussion if enabled); see [`SUPPORT.md`](SUPPORT.md) for the
  full channel breakdown. For vulnerability reports, use the
  private channel in [`SECURITY.md`](SECURITY.md).

## License

MIT — see [LICENSE](LICENSE).
