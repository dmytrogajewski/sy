# sy

Personal Linux setup tool — niri rice config templater + Rust daemon
collection for a Fedora 43 laptop. Single binary, single source of truth.

```
sy apply              # render configs/* with the active theme → ~/.config/
sy stack bar          # waybar-replacement layershell stack visualizer
sy knowledge daemon   # semantic search over local files (NPU-accelerated)
sy knowledge search "kubernetes secrets"
sy auto               # auto-configure MCP servers across agents (Claude, …)
…
```

The repo deliberately collapses **rice configs**, **a CLI tool**, and a
**knowledge/embedding daemon** into one place so a fresh install of
this repo + a `cargo build --release && ./target/release/sy apply`
reproduces the entire desktop. See [`CLAUDE.md`](CLAUDE.md) for the
"no snowflakes" rule that drives that choice.

## Rice (niri + waybar + …)

Gruvbox Material Dark Medium theme. Directory layout mirrors
`~/.config/`, so each subfolder maps 1:1 to its destination.

```
.
├── Cargo.toml
├── sy.toml                   # active theme + sy config
├── src/                      # sy CLI + daemons
├── scripts/                  # one-shot helpers (prep_npu_embed.py …)
├── themes/<name>.toml        # palettes
└── configs/                  # templated rice configs
    ├── niri/config.kdl
    ├── waybar/{config.jsonc,style.css}
    ├── i3status-rust/config.toml
    ├── mako/config
    ├── fuzzel/fuzzel.ini
    ├── foot/foot.ini
    ├── swaylock/config
    └── yazi/{package.toml,theme.toml}
```

### Stack

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

Configs live as [minijinja](https://github.com/mitsuhiko/minijinja)
templates under `configs/`, driven by the active theme under `themes/`.

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

ya pkg install                          # yazi gruvbox flavor (one-time)
```

Override target dir with `--target` or `$XDG_CONFIG_HOME`. Override
repo root with `--root` or `$SY_ROOT`. The active theme lives in
`sy.toml`. Log out and pick **Niri** from your display manager, or
start a TTY session with `niri --session`.

## Knowledge plane — semantic search on your NPU

`sy knowledge` is an in-process vector search index over your local
files, served by an embedded **qdrant** + a 768-dim `multilingual-e5-base`
embedding model running on the **AMD Ryzen AI NPU** via ORT's VitisAI
execution provider.

```
sy knowledge add ~/Documents/notes   # register a tree to be indexed
sy knowledge daemon                  # background sync (default schedule 30m)
sy knowledge status --json           # snapshot (backend, latency, queue depth)
sy knowledge search "rust async cancellation"
sy knowledge bench --n 256           # throughput probe
```

### Hardware tier

| Backend    | Trigger                                | Throughput | VRAM | Notes |
|------------|----------------------------------------|------------|------|-------|
| `vitisai`  | `/opt/AMD/ryzenai/venv` is present     | ~7 chunks/s | 0 GB | Pre-compile cache under `~/.cache/sy/npu-embed/`. Best fit for laptops where you also want to run an LLM on the dGPU. |
| `cuda`     | pip `onnxruntime-gpu==1.24.*` installed, NVIDIA GPU | depends | ≈1 GB | Legacy fastembed path. Sees CUDA libs via `~/.local/lib/python*/site-packages/nvidia/*/lib`. |
| `cpu`      | Always-available fallback              | slow       | 0 GB | Last resort. |

Pick is automatic at startup in that priority order; surface with
`sy knowledge status --json` (look at `embed_backend`).

### One-time NPU setup

1. Install AMD Ryzen AI 1.7.1 system packages via the companion repo
   [`ryzenai-rpm`](https://github.com/dmytrogajewski/ryzenai-rpm)
   (RPM-ified XRT runtime, XDNA DKMS module, memlock config, AMD's
   Python wheel set).

2. Build the model + the NPU compile cache from this repo:

   ```bash
   source /opt/AMD/ryzenai/venv/bin/activate
   python ~/sources/sy/scripts/prep_npu_embed.py
   ```

   Downloads `intfloat/multilingual-e5-base` from HF, exports to
   static-shape ONNX, BF16-quantises with AMD's Quark, and runs a
   one-shot VitisAI compile to produce the `.rai` NPU artifact
   (≈75 s, cached forever). All outputs land in
   `~/.cache/sy/npu-embed/`.

3. Start the daemon:

   ```bash
   sy knowledge daemon          # foreground
   # or
   systemctl --user start sy-knowledge.service   # if your unit is wired
   ```

That's it — sy auto-detects the AMD venv at startup, re-execs itself
with the right `LD_LIBRARY_PATH` + `ORT_DYLIB_PATH` + `RYZEN_AI_*`
env baked in, and routes embeddings through the NPU.

### Why `multilingual-e5-base`, not `-large`?

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

### Knowledge plane CLI cheat-sheet

```bash
sy knowledge add <path>            # register a tree (respects .gitignore)
sy knowledge rm <path>             # unregister
sy knowledge schedule 30m          # rewrite [knowledge].schedule
sy knowledge sources               # list registered roots
sy knowledge manifests --json      # active per-folder manifests

sy knowledge daemon                # supervises qdrant + scheduled embed
sy knowledge status [--json]       # snapshot (backend, queue, last sync)
sy knowledge pause / resume / toggle-pause / cancel
sy knowledge bench --n 1024        # throughput probe + active backend

sy knowledge search <query>        # interactive semantic search
sy knowledge mcp                   # MCP server (stdio) for AI agents
```

The MCP server is auto-registered in Claude / Cursor / Codex / Gemini
configs by `sy auto` (toggle with `sy knowledge mcp-enable/disable`).

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
- **Yazi flavor**: `yazi/package.toml` pins `bennyyip/gruvbox-dark`.
  Run `ya pkg install` after deploying.

## License

MIT — see [LICENSE](LICENSE).
