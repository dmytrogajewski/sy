# SPEC: `sy file` — native, niri-tiled, yazi-shaped file manager

## 1. Summary

`sy file` is a **native Wayland file manager** that mirrors
[yazi][yazi]'s three-pane UX (parent · current · preview) but renders
through `iced` + `iced_layershell`/`xdg-toplevel` — the same UI stack
already shipping in `sy mon` and `sy stack bar` — so previews work
without escaping into a terminal image protocol (the latest yazi
experiment that produced sub-readable preview text in
`specs/bugs/...md-rich.yazi`). The window lives as a normal
xdg-toplevel under [niri][niri-design]'s scrollable tiling, collapses
its 3-pane layout to 2-pane and 1-pane modes as the tile column
shrinks, ships **always-on keyboard shortcuts via niri keybinds → IPC
ops** (Wayland forbids true global grabs from a client), uses kernel
APIs (`copy_file_range` / `io_uring` for large copies, `inotify` for
live updates, `notify-rs` for cross-platform fallback, `trash` for
freedesktop trash), pulls semantic file ranking inline from
`sy knowledge`, and exposes the same surface over JSON IPC so an
agent (Claude, MCP client, or another sy plane) can drive every
file-manager action a human can.

The plugin system is specified in a sibling SPEC
([`sy-file-manager-plugins`][plugin-spec]).

## 2. Background & Research

### 2.1 Market context — peers and gaps

| Manager | Stack | 3-pane | Image preview | Plugins | Async ops | Tabs/dual | Knowledge-aware | Tiling-WM-aware |
|---|---|---|---|---|---|---|---|---|
| [yazi][yazi] | Rust TUI, ratatui-style | ✓ | sixel/kitty/iterm only | Lua, in-process | ✓ | dual-pane plugin | ✗ | terminal-confined |
| [nnn][itsfoss-tfm] | C TUI | ✗ | ✗ | shell plugins | partial | ✗ | ✗ | ✗ |
| [lf][itsfoss-tfm] | Go TUI | optional | shell-driven | shell scripts | partial | ✗ | ✗ | ✗ |
| [ranger][itsfoss-tfm] | Python TUI | ✓ | sixel via w3m/ueberzug | Python | partial | tabs | ✗ | ✗ |
| [broot][itsfoss-tfm] | Rust TUI | tree+search | ✗ | verb scripts | ✓ | ✗ | ✗ | ✗ |
| [xplr][xplr] | Rust TUI | multi-panel | external | Lua | ✓ | ✗ | ✗ | ✗ |
| [Superfile][superfile] | Go + Bubble Tea | multi-pane | ✓ (PDF/video) | community plugins | ✓ | multi-pane | ✗ | ✗ |
| [Cosmic Files][cosmic-files] | Rust + libcosmic (iced) | tabs + dual-pane | thumbnailing | ✗ | per-op compio thread, pause/cancel/resume | ✓ | ✗ | ✗ |
| [Nautilus][archwiki-nautilus] | C + GTK4 | nav-pane + list | thumbnailing | extensions (Python/JS) | ✓ | tabs | ✗ | ✗ |
| [Dolphin][dolphin] | C++ + Qt | nav + multi-view | thumbnailing | KIO slaves | ✓ | tabs + split | ✗ | ✗ |
| **`sy file` (this spec)** | Rust + iced + wgpu | ✓ (collapses 3 → 2 → 1 by tile width) | wgpu-native (decoded once, rendered as iced::widget::image) | binaries-over-stdio (separate SPEC) | io_uring/copy_file_range, progress IPC | dual-pane via niri tile, no in-window tabs | semantic re-rank via `sy knowledge` | yes — listens to `xdg_toplevel.configure` and reflows |

**What yazi users miss in a GUI:** real font choice (currently locked
to terminal font), real image scaling (terminal image protocols are
either sixel = blurry/slow or kitty/iterm = terminal-locked),
drag-and-drop into other Wayland apps, system-wide "open file from
anywhere" via the compositor. `sy file` solves all four.

**What GUI users miss in yazi:** scriptability, vim-style keymaps,
agent-drivability. `sy file` keeps all three (yazi-shaped keymap by
default, binaries-over-stdio plugins, full JSON IPC surface).

Cited details:

- yazi's plugin architecture and async-first execution model is the
  reference: every plugin lives in
  `~/.config/yazi/plugins/<name>.yazi/main.lua` and implements
  `peek`/`seek`/`preload` for previewers, with the host providing
  six globally accessible namespaces ([yazi-plugins-arch][yazi-pi]).
  We rebuild the contract surface over stdio so plugins don't have
  to embed Lua (see [`sy-file-manager-plugins`][plugin-spec]).
- Cosmic Files' `Operation` enum + per-op compio thread for
  copy/move/extract is the right async-ops model
  ([cosmic-files-ops][cf-ops]). It survives pause/cancel/resume and
  emits structured progress — exactly what an agent driver needs.
  Recent (Jan 2026) releases focused on copy & extract perf
  ([cosmic-1.0.8][cf-release]).
- Superfile's selling points are exactly what's missing from yazi
  for non-vim users: refined UI, multi-pane, mouse support, native
  themes via `~/.config/superfile/theme/*.toml` ([superfile][sf]),
  PDF/video preview, vim keys as an alt ([sf-overview][sf-overview]).
  These all reach `sy file` for free because we render natively.

### 2.2 Technical context

#### iced + iced_layershell stack (already in repo)

- `Cargo.toml` declares `iced = { version = "0.14", features = ["tokio", "wgpu", "image", "canvas"] }`
  and `iced_layershell = "0.17"`. The `bar-iced` cargo feature gates
  the layer-shell builds.
- `src/mon/app.rs` is the reference layer-shell app: `Anchor::Center`,
  `KeyboardInteractivity::OnDemand`, 1280×800, exclusive zone 0,
  iced subscription emits only on IPC events or keypress (no per-frame
  polling).
- `src/stack/bar/app.rs` is the reference bar — wlr-layer-shell
  anchored to the right edge, IPC ops on a side thread, polls
  `items.json` on a tick.
- iced exposes `window::resize_events()` for runtime resize handling
  ([iced-window][iced-window]); per-widget layouts mean responsive
  collapse is implemented as a manual `match self.width { … }` ladder
  in the root `view` ([iced-responsive][iced-responsive]).
- iced has no unified layout engine; each widget self-lays-out. Wide
  layouts use `row! [pane1, pane2, pane3]` with `Length::FillPortion`.

#### Niri tiling integration

- niri is a [scrollable tiling compositor][niri]: every window is an
  xdg-toplevel placed into a column with configurable proportional
  width. The compositor sends `xdg_toplevel.configure` with the
  tile-assigned size; the client (iced/winit) receives a resize
  event and reflows ([niri-rules][niri-rules],
  [niri-layout][niri-layout]).
- Niri does NOT support [wlr-foreign-toplevel-management][wlr-fwt]
  for arbitrary input grabbing; clients cannot grab global keys.
  "Always-on" hotkeys must live in `configs/niri/config.kdl`'s
  `binds {}` block and dispatch to the running daemon over IPC
  ([cachyos-niri][cachy-niri]).
- For drag-from / drop-into other apps: standard `wl_data_device`
  (winit + iced handle this transparently via the underlying smithay
  client).

#### Kernel file-ops APIs

- **`copy_file_range(2)`** does intra-fs zero-copy in the kernel.
  On btrfs / xfs this is reflink (CoW): instantaneous, no extra
  blocks. On ext4 / tmpfs it's a kernel-side memcpy that avoids the
  read/write syscall pair. Cosmic Files' 1.0.8 perf win was exactly
  switching to `copy_file_range` for same-fs moves.
- **`io_uring`** wins on small-file workloads — Vincent Du's
  Jan 2026 benchmark shows 4.2× speedup over `cp` for ML datasets on
  NVMe ([devto-iouring][iouring-blog]); the [io_uring 2025 DBMS
  paper][iouring-paper] reports >2× over naive use only when batches
  fully exploit submission queues. For sy: use `tokio-uring` for the
  bulk-copy worker; the steady-state win is real but cold-start
  overhead is non-zero, so single-file ops fall back to
  `copy_file_range`.
- **`inotify`**: O(n) memory because every watched dir needs a fd;
  hits `fs.inotify.max_user_watches` (default 8192 on Fedora) for
  large trees ([notify-rs][notify-rs]). Use it for the current dir
  + parent + selected subtree only.
- **`fanotify`** (FAN_MARK_FILESYSTEM): O(1) memory, whole-fs view,
  but needs `CAP_SYS_ADMIN` ([notify-rs][notify-rs]). Out of scope
  — sy refuses to require root in user planes.
- **`statx`**: modern, gives `STATX_BTIME` (creation time) and
  `STATX_MNT_ID` (mount detection) without a `mount(2)` table walk.
- **`renameat2(RENAME_EXCHANGE)`**: atomic file swap; used for
  bulk-rename to avoid clobbering on conflicts.

#### Trash, MIME, mounts

- [Trash Specification v1.0][trash-spec] / [`trash` crate][trash-crate]:
  the de facto target. Both Cosmic Files and Nautilus comply.
- MIME via `shared-mime-info` (XDG); the [`xdg-mime`][xdg-mime]
  crate wraps lookups. For more accurate sniffing on extensionless
  files, use libmagic via `tree_magic_mini` (Cargo: small, pure-Rust).
- Mount monitoring: udisks2 over D-Bus is the canonical interface;
  `udisks2` crate exists but is half-maintained. Fallback: parse
  `/proc/self/mountinfo` and `inotify` it.

#### Image / video / archive preview without a terminal

- `iced` 0.14 ships `widget::image` with a wgpu texture path; this
  is what `sy file` uses for previews. Markdown → PNG via `pulldown-cmark`
  + `cosmic-text` to a wgpu canvas (no chrome-headless, no keyring
  popups — fixes [BUG-20260527-...md-rich][md-rich-bug]).
- For PDF / video / 3D: spawn ImageMagick / ffmpeg / f3d as a
  short-lived subprocess that writes a PNG to a `tmpfs` cache slot.
  Plugin-driven (see plugin SPEC).

### 2.3 Deep dives — why these choices

- **Why iced + xdg-toplevel, not iced_layershell**: a file manager
  is a regular tiled application, not an overlay. Layer-shell is for
  bars, popups, lock screens — niri does not tile layer-shell
  surfaces. Existing sy planes show the toolkit choice works for
  both modes; we keep the same iced 0.14 dep, same gruvbox `Palette`,
  same custom-widget patterns (`src/mon/widgets/`).
- **Why niri keybinds + IPC for "global hotkeys"**: Wayland
  deliberately removed `XGrabKey`. The [xdg-desktop-portal Global
  Shortcuts portal][portal-shortcuts] is a fix-in-progress (released
  Feb 2025 but only Plasma and partial GNOME implement it; niri does
  not). The portable answer is exactly what niri encourages:
  user-bound keys in `configs/niri/config.kdl` spawn
  `sy file --ipc <action>` which IPCs into the running daemon. Same
  pattern sy already uses for `Super+M` opening `sy mon`.
- **Why JSON over stdio for plugins, not WASM**: Helix
  ([helix-3806][helix-disc]) and pgocode-style projects have
  converged on the same answer: WASM component model isn't stable
  enough in 2026, runtimes are large (wasmtime is ~5 MB, [WebXtism /
  Extism][extism] adds another layer), and the cross-language story
  is weaker than just "spawn a process and exchange JSON". LSP
  exists for exactly this and achieves [10k+ ops/sec over stdio][lsp-perf].
  See the plugin SPEC for the contract.
- **Why integrate `sy knowledge`**: the embed-and-rerank stack
  already runs as a daemon (`sy-knowledge.service` →
  `sy-qdrant.service`). A file manager that can answer "find me the
  PDF where I documented the syauth PAM control flag" is a 10×
  improvement over filename-grep, costs almost nothing because the
  daemon is always-on, and is invisible to non-users (`sy file` falls
  back to `fzf`-style filename search when knowledge is unreachable).
- **Why no in-window tabs**: niri's scrollable tiling IS the tab UI.
  Two `sy file` invocations become two columns; `Mod+H/L` scrolls
  between them. Adding tabs duplicates a compositor primitive and
  forces the user to learn two layers of navigation. Cosmic Files
  ships tabs because it targets `Mod+Tab`-style WMs; niri makes them
  redundant.

## 3. Proposal

### 3.1 Approach

Add a new `Cmd::File` clap variant + `mod file;` under `src/file/`,
mirroring how `sy mon` is structured.

```
src/file/
├── app.rs              # iced Application, root view + update
├── cli.rs              # `sy file [path] [--ipc …] [--json] …`
├── ipc.rs              # JSON IPC ops: open|cd|select|toggle-hidden|…
├── mcp.rs              # MCP tool surface
├── mod.rs
├── state/
│   ├── mod.rs          # `State { panes: Panes, mode: LayoutMode, selection: SelectionSet, … }`
│   ├── panes.rs        # parent/current/preview pane state
│   ├── selection.rs    # multi-select + cursor
│   └── ops.rs          # `Operation` enum + progress / pause / cancel / resume
├── view/
│   ├── mod.rs          # responsive root: 3-pane → 2-pane → 1-pane
│   ├── pane.rs         # list rendering, icons, badges
│   ├── preview.rs      # dispatches to previewer plugin or built-in
│   ├── statusbar.rs    # path crumbs, mode, ops, knowledge match count
│   └── commandbar.rs   # ":" verb prompt (yazi-style) + fuzzy filter
├── theme.rs            # reuses Palette projection from src/mon/theme.rs
├── widgets/            # custom widgets specific to file UX
│   ├── crumb.rs        # path breadcrumb
│   ├── progress_row.rs # per-op progress chip in statusbar
│   ├── chip.rs         # selection-count / mode chip
│   └── icon.rs         # nerd-font icon resolver (mime → glyph)
├── fs/
│   ├── mod.rs
│   ├── walk.rs         # async dir read, statx fast-path
│   ├── copy.rs         # copy_file_range + io_uring fallback ladder
│   ├── watch.rs        # notify-rs (inotify) per-pane watchers
│   ├── trash.rs        # trash-rs wrapper + restore
│   ├── mime.rs         # tree_magic_mini + xdg-mime
│   └── mounts.rs       # /proc/self/mountinfo poller + udisks2 (optional)
├── search/
│   ├── mod.rs
│   ├── filename.rs     # fuzzy filename matcher (nucleo crate)
│   └── knowledge.rs    # qdrant client wrapping `knowledge::ipc`
├── plugin/             # see sibling SPEC; this is the host runtime
│   ├── client.rs
│   ├── mod.rs
│   └── registry.rs
└── keymap.rs           # default yazi-shaped bindings + remap config
```

Bootstrapping order: `configs/yazi/` is **deleted** and replaced by
`configs/sy/file.toml` + `configs/sy/file-keymap.toml`. The yazi
binary install in `scripts/yazi-plugins.sh` is removed. Replacing the
"no snowflake yazi recipe" with a productivised plane is the headline
delta.

### 3.2 Key decisions

| # | Decision | Choice | Reasoning | Alternatives |
|---|----------|--------|-----------|--------------|
| 1 | UI toolkit | **iced 0.14 + xdg-toplevel** | Same dep tree as `sy mon` and `sy stack bar`; wgpu renderer means image preview is sharp at any DPI; no chrome-headless dependency. | `egui` (no Wayland-native, draws into surfaces), `gtk4-rs` (alien dep tree), `libcosmic` (locks into cosmic-comp idioms), stay-in-yazi |
| 2 | Layout strategy | **Responsive ladder: ≥1100 px = 3-pane, ≥720 px = 2-pane (current + preview), <720 px = 1-pane** | Niri's typical column widths are 1/3, 1/2, 2/3, full. Maps cleanly onto our three modes. iced re-renders on `WindowEvent::Resized`. | Fixed 3-pane (clipped in narrow tiles), fixed 1-pane (wastes wide-tile space) |
| 3 | "Always-on" keys | **niri `binds {}` → `sy file --ipc <action>` → daemon IPC** | Wayland deliberately forbids client-side global grabs; the [Global Shortcuts portal][portal-shortcuts] is not in niri yet. niri keybinds + IPC is how `sy mon`, `sy stack`, `sy popup` already do it; consistency win + zero new attack surface. | xdg-portal-global-shortcuts (not in niri), grim-style daemon-key-grab (architectural mismatch with niri) |
| 4 | File ops engine | **Per-op async task + `copy_file_range` fast path + `tokio-uring` for >256 MB or >100 files** | Mirrors Cosmic Files' compio model; lets us emit structured progress over IPC so agents can pause/cancel; matches kernel-API reality (single small files don't pay io_uring setup cost). | All-sync (blocks UI), all-io_uring (slow for one-shot), rsync subprocess (foreign config + parsing) |
| 5 | Plugin runtime | **Binaries-over-stdio JSON-RPC** | Specified in [`sy-file-manager-plugins`][plugin-spec]. TL;DR: language-agnostic, sandbox = OS process boundary, scales to multi-process (one previewer crash doesn't take down the manager), reuses agent-sandbox primitives from `src/agt/`. | Lua-embedded (yazi-style; ties plugin authors to Lua), WASM/Extism (heavier runtime + component-model churn) |
| 6 | Knowledge integration | **In-pane "smart filter" chip** that posts the current pane's `cwd` + filter query to `sy knowledge` and merges qdrant scores into the file list ordering | Knowledge daemon is always-on; cost is one IPC round-trip per query. Falls back to filename-only ranking when daemon is unreachable. | Separate `sy knowledge browse` plane (worse UX, duplicate browsing), out-of-tree plugin (loses MCP wiring + the always-on guarantee) |
| 7 | Theming | **Reuse `Palette` from `src/mon/theme.rs`; ship `configs/sy/file-theme.toml` as a thin override layer** | One palette across mon / stack / file means the gruvbox identity is enforced; per-plane override stays declarative; no snowflakes. | Per-plane palette (drift), hardcoded (breaks theme switching) |
| 8 | Trash | **`trash` crate, freedesktop-spec compliant** | Other DEs see and can undo our trashes; restore is a single `Operation::Restore` IPC op. | `rm -rf` (data loss), custom trash (interop fail) |

### 3.3 Scope

The complete `sy file` plane consists of:

1. **CLI surface** — `sy file [PATH]`, `sy file --ipc <op>`,
   `sy file --json status`, `sy file --dry-run <op>`,
   `sy file doctor`. CLIG-compliant: `-h/--help`, `--version`,
   `--no-color`, `NO_COLOR`, stable exit codes (0 ok, 1 error, 2
   usage, 3 daemon-unreachable, 4 op-cancelled).
2. **MCP tool surface** — `file_open`, `file_list`, `file_select`,
   `file_copy`, `file_move`, `file_trash`, `file_restore`,
   `file_search`, `file_preview` (returns base64 PNG bytes for the
   selected file's previewer output). All under
   `sy-knowledge`-style stable JSON Schema.
3. **GUI application** (iced) — three-pane responsive layout, gruvbox
   palette, nerd-font icons via the existing
   `JetBrainsMono Nerd Font` in `configs/sy/file-theme.toml`. Modes:
   - 3-pane (parent · current · preview)
   - 2-pane (current · preview)
   - 1-pane (current; "p" pulls preview as a transient overlay)
   - mode changes automatically on `WindowEvent::Resized`; user-locked via `Ctrl+1/2/3`.
4. **Navigation** — arrow keys + vim hjkl, `Enter`/`l` open,
   `Backspace`/`h` up, `gg`/`G` first/last, `/` filter, `:` command
   palette, `<Space>` multi-select toggle, `<Esc>` clear selection.
   Default keymap in `configs/sy/file-keymap.toml`; reloadable on
   SIGHUP.
5. **File operations** — open (xdg-open), copy, move, rename, trash,
   restore, delete (permanent, requires `--yes` or hold-shift),
   mkdir, touch, chmod (8-bit picker), symlink, hardlink, bulk
   rename via `$EDITOR`. All async, all emit `OpEvent` over IPC
   (`Started`, `Progress { done, total, throughput_bps }`, `Paused`,
   `Resumed`, `Cancelled`, `Completed`, `Failed { code, msg }`).
6. **Selection** — multi-select with `<Space>`, range with
   `<Shift>+arrow`, `*` selects all, `a` invert, `v` visual mode
   (yazi-style). Selection is part of `State` and survives pane
   navigation as long as cwd unchanged.
7. **Search** —
   - Filename: in-pane `/` fuzzy filter (nucleo).
   - Knowledge: `:k <query>` sends to `sy knowledge search` scoped
     to the current pane's `cwd`, merges qdrant scores into the
     listing.
   - Cross-tree: `sy file --ipc search '<glob>' --root /` returns
     a JSON stream of matches.
8. **Preview pipeline** — built-in previewers for: `text/*` (syntect-
   highlighted), `image/*` (iced::widget::image), markdown
   (pulldown-cmark → cosmic-text → wgpu canvas; **no chrome**), pdf
   (first page via `pdftoppm`), video (first frame via `ffmpeg
   -ss 0 -frames 1`), 3D (`f3d`), archive (`lsar`). Anything else
   dispatches to a plugin (see plugin SPEC).
9. **Plugin host runtime** — discovers manifests under
   `~/.config/sy/plugins/*/plugin.toml` and
   `configs/sy/plugins/*/plugin.toml`. Spawns long-running stdio
   processes per the plugin SPEC. The host re-exposes a
   `host.fs.*` / `host.preview.*` capability surface.
10. **Knowledge integration** — chips in the statusbar show "qdrant:
    online" + match count when a knowledge query is active.
    Reachability probed on startup and every 30 s; degrades silently
    to filename-only mode.
11. **Watch / live updates** — `notify-rs` watcher per visible
    pane (parent + current + selected child dir if expanded).
    Events debounced (50 ms) before triggering a pane refresh.
12. **Drag-and-drop** — `wl_data_device` source for drag-out (other
    Wayland apps receive `text/uri-list`), drop-target for inbound
    URIs (copies/moves per modifier key, matching Nautilus
    convention: Ctrl=copy, Shift=move, no-mod=move-within-fs / copy-
    across-fs).
13. **Trash** — full freedesktop-spec interop via `trash` crate.
    Restore from any DE's trash; `sy file --ipc trash list` returns
    a JSON inventory.
14. **Mounts** — read `/proc/self/mountinfo` at startup, poll on
    `inotify` events. Listed in a sidebar in 3-pane mode; in 2-pane
    mode mounts appear in the command palette under `:m`.
15. **Bookmarks / recent dirs** — XDG `recently-used.xbel` + own
    `~/.local/state/sy/file/bookmarks.toml`. Auto-populated; user
    can pin with `b<key>`.
16. **Observability** — `tracing` spans on every op, structured
    stderr logs on `--log-format json`, waybar tile via
    `sy file --waybar` showing `{ ops_pending, ops_failed }` so the
    user sees background copies without focusing the window.
17. **Theming / configs** — `configs/sy/file-theme.toml`
    (gruvbox-dark default), `configs/sy/file-keymap.toml`,
    `configs/sy/file.toml` (sort, hidden, icons, mounts), all
    deployed by `sy apply`. **No yazi installation step**; the
    productivisation in `scripts/yazi-plugins.sh` is replaced with
    a no-op stub (and eventually deleted) so the rice is fully
    sy-shaped per CLAUDE.md "no snowflakes".
18. **Always-on keybinds** — `configs/niri/config.kdl`'s `binds {}`
    block gains `Mod+E { spawn "sy" "file"; }`,
    `Mod+Shift+E { spawn "sy" "file" "--ipc" "toggle-preview"; }`,
    `Mod+Slash { spawn "sy" "file" "--ipc" "command-bar"; }`,
    documented in `configs/niri/config.kdl` and the README.
19. **Doctor** — `sy file doctor` probes: daemon socket, knowledge
    reachability, JetBrainsMono Nerd Font installed, niri binds
    present, plugins healthy. Mirrors `sy syauth doctor` style.
20. **Migration removal** — `configs/yazi/` deleted; `scripts/yazi-plugins.sh`
    deleted; the four hand-cloned yazi plugins, the 33 ya-pkg
    plugins, and the gruvbox-dark.yazi flavor all stop being
    productivised. README's "yazi" stack-table row replaced with
    `sy file`.

### 3.4 Anti-goals

| Anti-goal | Substantive reason |
|---|---|
| **Remote-fs (SSH/SFTP/SMB) browsing** | sy is by charter single-host; remote-fs invites credential storage (snowflake hazard — see [BUG-20260527-...keyring][md-rich-bug] for the chrome-headless variant we already hit), and the agt sandbox model doesn't extend cleanly across hosts. Use `sshfs` mounts via udisks2 and browse them as local paths. |
| **In-window tabs** | Niri's scrollable tiling IS the tab UI. Two `sy file` invocations become two columns; adding tabs duplicates a compositor primitive and forces the user to learn two layers of navigation. |
| **Built-in image/video editing** | Wrong primitive — file managers are about navigation + ops, not pixel work; sy already declines to ship a media editor for the same reason it doesn't ship `sy edit`. |
| **Embedded Lua/WASM/Python runtime for plugins** | See plugin SPEC §3.4. Adds a 5–20 MB runtime + a fixed plugin language; binaries-over-stdio is the LSP/MCP-shaped answer and matches sy's existing IPC story. |
| **Replace `xdg-open`** | `xdg-open` is the freedesktop integration point — bypassing it breaks every other app's "default app" config. We dispatch through it. |
| **Built-in archive extraction beyond preview** | `bsdtar` / `unar` / `7z` are the universe of formats; reimplementing is a snowflake hazard and a security minefield. We list contents (via `lsar`) and extract via the system tool. |
| **Per-window theme variants** | Theme is sy-wide; per-window overrides are exactly the snowflakes CLAUDE.md forbids. |
| **Tabs synced across instances** | Same reason as in-window tabs: niri's persistence story (and a future `sy session` plane, not in scope here) is the right home for that. |
| **Encrypted-volume mounting from the UI** | LUKS / GPG flows require PAM / agent context that belongs in `syauth`, not in a file manager. We list mounts; mounting is `udisksctl` or `sy syauth mount`. |

## 4. Technical Design

### 4.1 Architecture

```
                          ┌──────────────────────┐
                          │   sy file (iced)     │
   user keypress ─────────│  ┌────┐ ┌────┐ ┌──┐  │◄────── notify-rs inotify events
                          │  │par │ │cur │ │pv│  │
                          │  └────┘ └────┘ └──┘  │
                          │  statusbar + cmdbar  │
                          └──┬───────────────────┘
                             │ Message
                          ┌──▼────────────┐
                          │ State.update  │
                          └──┬────────────┘
                             │
              ┌──────────────┼──────────────┬──────────────┐
              │              │              │              │
        ┌─────▼────┐   ┌─────▼─────┐  ┌─────▼─────┐  ┌─────▼─────┐
        │ fs::ops  │   │ search    │  │ plugin    │  │ ipc       │
        │ async    │   │ knowledge │  │ runtime   │  │ JSON/stdio│
        │ tokio    │   │ filename  │  │ stdio     │  │ + MCP     │
        └─────┬────┘   └─────┬─────┘  └─────┬─────┘  └─────┬─────┘
              │              │              │              │
              ▼              ▼              ▼              ▼
       copy_file_range   qdrant via    plugin procs    cli / mcp /
       io_uring         knowledge IPC                  niri keybinds
       trash
```

Key data flows:

- **Pane render**: cwd → `fs::walk` (async, statx fast-path) →
  `Vec<Entry>` → sort/filter → `view::pane` widget tree.
- **Copy op**: user triggers → `State::ops.spawn(Operation::Copy)` →
  async task → `OpEvent` stream to `update`; statusbar shows
  progress widget; agent driver reads stream over IPC.
- **Preview**: hovered file mime detected → built-in dispatcher →
  if no built-in handler, lookup in `plugin::registry` → spawn
  request → stream PNG bytes back → `iced::widget::image`.
- **Knowledge search**: `:k foo` → `search::knowledge::search(cwd, "foo")`
  → IPC call to `sy-knowledge.service` → score map merged into
  pane Entry list → re-sort.

### 4.2 Non-functional requirements

- **Performance**:
  - Cold open (open `~`, render 3 panes for a 5k-entry dir):
    p99 < 250 ms on the AMD Ryzen AI 9 HX 370 reference machine.
  - First preview byte: p99 < 150 ms for built-in previewers,
    < 600 ms for plugin previewers (includes process spawn).
  - 10 GB single-file copy on same btrfs subvol: instant (reflink);
    cross-fs: bounded by disk, no UI stalls.
  - Memory ceiling: < 200 MB resident steady-state, < 50 MB for
    each inactive plugin process.
- **Reliability**:
  - Op pause/resume/cancel survives daemon restart (op state in
    `~/.local/state/sy/file/ops/<uuid>.json`).
  - Plugin crash never crashes the host — process boundary +
    JSON-RPC error envelope.
  - Trash always uses freedesktop layout so other DEs can restore.
- **Security**:
  - All file ops happen as the invoking user; no setuid surface.
  - Plugins run under the existing `agt` SELinux confinement;
    inherits the "plugin can't reach $HOME unless granted" rule
    from `src/agt/policy/`.
  - IPC socket mode 0600 in `$XDG_RUNTIME_DIR/sy-file.sock`.
- **Observability**:
  - `tracing` spans: `pane.scan`, `op.<kind>`, `plugin.spawn`,
    `plugin.request`, `knowledge.query`.
  - Structured logs: `--log-format json` emits one event per
    op-state transition.
  - waybar pill via `sy file --waybar` shows
    `{"text": "📂 3", "tooltip": "3 ops running, 0 failed"}`.

### 4.3 CLI / MCP surface

```
sy file [PATH]                       # opens GUI rooted at PATH (default $PWD)
sy file --json status                # current daemon state as JSON
sy file --ipc <op>                   # send an IPC op (open/cd/select/copy/…)
sy file --ipc copy --src A --dst B   # explicit, scriptable
sy file --waybar                     # emit waybar JSON
sy file doctor [--json]              # health probe
sy file --dry-run --ipc trash …      # planned op, no fs side-effects

Exit codes:
  0  ok
  1  generic error
  2  usage error
  3  daemon unreachable / not started
  4  op cancelled / refused (e.g. cross-fs without --yes)
  5  plugin error (plugin code propagated)

Env:
  SY_FILE_SOCK              override IPC socket path
  SY_FILE_THEME             override theme file
  SY_FILE_NO_KNOWLEDGE=1    disable knowledge integration
  NO_COLOR / TERM=dumb      respected as per CLIG

JSON event schema (--log-format json, --ipc tail):
  { "ts": "...", "kind": "op.progress", "op_id": "uuid",
    "kind": "copy", "done": 4096, "total": 8192, "throughput_bps": 1.2e9 }
```

MCP tools (under `sy file mcp` or via `sy-file.service`):

- `file_list { path, include_hidden, limit, offset } → { entries: [{name, mime, size, mtime, …}] }`
- `file_open { path } → { ok }`
- `file_copy { sources: [path], dest, conflict: "skip"|"replace"|"rename" } → { op_id }`
- `file_move { … } → { op_id }`
- `file_trash { paths } → { trashed: [path] }`
- `file_restore { trashed_path } → { ok }`
- `file_search { query, root, knowledge: bool } → { results: [path] }`
- `file_preview { path, max_width, max_height } → { mime, png_base64 }`
- `file_select { paths, mode: "add"|"replace"|"toggle" } → { selection: [path] }`
- `file_ops_list → { ops: [{ op_id, kind, state, done, total }] }`
- `file_op_cancel { op_id } → { ok }`

### 4.4 Testing strategy

- **Unit**:
  - `fs::walk` — synthetic dir tree, statx-mocked, edge cases
    (symlinks, perm-denied, encoding).
  - `fs::copy` — fault-injected; cancel mid-stream; cross-fs
    detection.
  - `search::knowledge::merge` — given qdrant scores + filename
    matches, produces a stable, deterministic order.
  - `keymap` — yazi-shape default keymap parses + maps every action.
- **Integration**:
  - **Daemon-in-thread**: spawn `sy file` daemon on a tmpfs root,
    drive via IPC, assert state. Same pattern as `sy mon`.
  - **Plugin contract**: in-tree fake plugin
    (`tests/fixtures/fake-previewer/main.rs`) exercises spawn,
    handshake, capability negotiation, request, response, error,
    crash, restart.
  - **Knowledge integration**: daemon-in-thread with a stubbed
    qdrant returning fixed scores.
- **End-to-end / manual recipe** (`docs/how-to/run-sy-file.md`):
  open `sy file`, navigate, copy, trash, restore, preview a
  markdown / image / pdf, ":k" query, drag-out to another app.

### 4.5 Migration & compatibility

- **Removal of yazi**: `configs/yazi/` deleted, `scripts/yazi-plugins.sh`
  deleted, README's stack-table updated. Existing user state under
  `~/.config/yazi/` is preserved on disk but not productivised
  (CLAUDE.md "no snowflakes" applies forward).
- **niri keybinds**: new `binds {}` entries under
  `configs/niri/config.kdl` for `Mod+E` / `Mod+Shift+E` / etc.;
  collisions with existing binds checked via `niri validate` in CI.
- **Schema**: ops state file
  `~/.local/state/sy/file/ops/<uuid>.json` is new; no migration
  needed for first release.
- **Backward-compat**: there is no backward-compat surface — yazi
  was deployed but not part of any documented agent contract; its
  removal is internal to the rice.

### 4.6 Dependencies

| Crate | Purpose | Notes |
|---|---|---|
| `iced` | already vendored | unchanged version (0.14) |
| `iced_layershell` | **not used here** | regular xdg-toplevel |
| `nucleo` | fuzzy matcher | Helix's matcher, MIT, ~30 KB binary impact |
| `notify` + `notify-debouncer-mini` | already transitive | reused |
| `trash` | freedesktop trash | maintained, MIT, no FFI |
| `tree_magic_mini` | MIME sniffing | small, pure Rust |
| `xdg-mime` | XDG MIME db | small, pure Rust |
| `tokio-uring` | io_uring for bulk copies | optional cargo feature `file-iouring`, default on Linux |
| `tokio` | already vendored | unchanged |
| `serde` / `serde_json` | IPC, manifests | already vendored |
| `pulldown-cmark` | markdown parsing for built-in preview | replaces chrome-headless |
| `cosmic-text` | already transitive (via iced) | text shaping |

No new system libraries; `pdftoppm`, `ffmpeg`, `f3d`, `lsar`, `bsdtar`
are subprocess invocations (same as yazi's). All probed by
`sy file doctor` and surfaced via `sy apply`'s "ensure_*" pipeline.

## 5. User Journey Sketch

**Actor / context.** A power user on Fedora 43 with niri, who today
opens yazi in a foot terminal, fights the markdown preview, has to
type `m` to bookmark, can't drag a file into Telegram, and can't
ask "where did I write down the OOM tuning steps?". Also: any MCP
agent that needs to operate on the user's files.

**Trigger.** `Mod+E` (or any niri-bound key the user prefers) /
`sy file ~/sources/sy/` from the agent / agent invokes
`file_open { path: "/etc/dracut.conf.d/sy-amdxdna-defer.conf" }`.

**Phases (rough sketch — `/journey` expands into full SPEC):**

1. **Launch.** niri spawns `sy file` as a tiled xdg-toplevel; the
   window claims one column. iced reads `configs/sy/file-theme.toml`,
   `file-keymap.toml`, `file.toml`. Daemon (own process) starts on
   first invocation; subsequent `sy file` calls IPC into it.
2. **Browse.** Arrow keys / hjkl navigate; selection chip in the
   statusbar shows count; preview pane paints on hover. Live
   updates from inotify mean external changes (a download lands)
   are visible without F5.
3. **Search.** `:k tuned override` searches `sy knowledge` scoped
   to `cwd`; qdrant scores merge into the current pane and the
   match-count chip turns gruvbox-yellow.
4. **Operate.** `<Space>` to multi-select, `y` to copy / `x` to
   move / `d` to trash. Progress chip appears in the statusbar;
   `Ctrl+P` pauses, `Ctrl+R` resumes; waybar pill shows count.
5. **Open in agent.** `:` opens the command palette; `agt foo`
   sends the selection as URIs to `sy agt` via existing IPC. Agent
   ingests them and returns a synthesis to a popup.
6. **Tile-shrink.** User moves the window to a 1/3 column;
   `WindowEvent::Resized` fires, the layout drops the parent pane
   and then the preview pane; preview becomes a transient on `p`.

### Friction map

| Friction | Phase | Opportunity |
|---|---|---|
| First-time keymap discovery (yazi muscle memory vs. anyone else's) | 2 | Yazi-shape keymap by default; `?` opens an in-app cheatsheet rendered from the keymap config so it's never stale. |
| "I want to open this file in a specific app" | 4 | `:` palette suggests `xdg-mime`-resolved candidates + recent custom commands. |
| niri column-width and 3-pane layout disagreement (user expects 3 panes; column is too narrow) | 6 | Resize listener auto-collapses with a 200 ms easing; statusbar chip shows current layout so the user knows why. |
| Agent wants to copy 1000 files; the GUI shouldn't lock | 4 | Per-op async task + the op stream is already exposed over MCP; the GUI shows progress, the agent receives `Started/Progress/Completed` events. |
| Plugin crashes when previewing a malformed PDF | 3 | Plugin process boundary catches it; preview pane shows "plugin <name> exited (code 1) — see `sy file doctor`". |
| Knowledge daemon down | 3 | Filename search still works; the qdrant chip turns dim-grey + tooltip explains. |
| Chrome-headless keyring popup (the bug that killed `md-rich.yazi`) | — | No chrome dep; markdown preview is in-process iced+cosmic-text. |

### North star

Open the file manager with one keypress. See everything yazi shows,
sharper, with real fonts and real images. Scroll a 50-MB markdown
without thinking about it. Type one query, get the file you wrote
last month. Multi-select 200 photos, drag them into a chat. Every
single thing the GUI does, an agent can do over IPC with the same
verbs. Zero snowflake config; one `sy apply` reproduces the whole
thing.

## 6. Risks & Mitigation

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| iced 0.14 layout collapse logic is fiddly for 3 → 2 → 1 pane modes | Layout glitches at resize boundaries | Medium | Single root `view` function with an explicit mode enum; integration test that runs the iced runtime headlessly and asserts pane visibility per width. |
| niri-only assumption (column-width events) | Other compositors render but layout collapse never fires | Low (single-host rice is niri-pinned by `configs/niri/`) | Layout listens to `WindowEvent::Resized`, which is compositor-agnostic; works on sway / hyprland too. |
| io_uring portability — Fedora 43 has 6.x kernel, but minor versions may lag tokio-uring | Bulk copy falls back to copy_file_range | Low | Feature-flagged (`file-iouring`); runtime detection via `tokio_uring::Runtime::new().is_ok()` before use. |
| Plugin process spawn latency makes preview feel sluggish for niche file types | Preview UX feels slower than yazi | Medium | Long-running plugin processes (LSP-style), kept warm by the host; cold spawn budget < 200 ms verified in plugin contract test. See plugin SPEC §4. |
| `sy knowledge` indexing not yet covering user's `cwd` | `:k` returns 0 hits and looks broken | Medium | Auto-index hint: on first `:k` in an unindexed dir, the statusbar offers `:index .` which fires the knowledge daemon's reindex. |
| LOC ceiling (`scripts/check_main_rs_loc.sh`) | `main.rs` gains a Cmd::File variant + dispatch | Low | Variant is ~12 lines; bump ceiling commensurately, document in the check script's running total. |
| Wayland drag-and-drop edge cases (cross-toolkit DnD with GTK / Qt apps) | "Drag to Telegram" doesn't always work | Medium | Use the same `wl_data_device` + `text/uri-list` MIME that Nautilus uses; both Telegram (Qt) and Firefox (GTK) handle it; integration recipe tested manually. |
| Markdown preview parser parity with chrome/glow (tables, fenced code, GFM extensions) | Some MD looks worse than glow's terminal output | Medium | `pulldown-cmark`'s `OPT_ENABLE_TABLES | TASKLISTS | STRIKETHROUGH | SMART_PUNCTUATION` covers GFM. Syntax highlighting via `syntect` reused from `sy mon`. |
| Niri's `Mod+E` may collide with existing user binding | Bind silently shadowed | Low | `configs/niri/config.kdl` lints with `niri validate`; collisions surfaced in `sy file doctor`. |

## 7. Open Questions

1. **Bookmark format**: own TOML at `~/.local/state/sy/file/bookmarks.toml`
   vs. `recently-used.xbel`. Recommendation: write both — XBEL for
   interop, TOML for sy-specific tags (pin key, color, knowledge
   collection scope).
2. **Knowledge query language**: `:k <free text>` only, or expose
   qdrant filters (`:k tuned has:pdf modified:<7d`)? The latter is
   powerful but bleeds qdrant detail into the UX.
3. **Theme file shape**: extend `themes/<name>.toml` with a
   `[file]` block, or ship `configs/sy/file-theme.toml` separately?
   Existing palettes already drive `sy mon` via `Palette`; favour
   the existing theme file with a new block to keep one source of
   truth.
4. **Bulk-rename UX**: in-window editor, or spawn `$EDITOR`? `$EDITOR`
   is consistent with yazi's `R` keybind; in-window adds widget
   debt. Recommendation: `$EDITOR` for now; revisit if multi-line
   regex flows demand it.
5. **Always-on bind for `sy file` itself**: should `Mod+E` always
   open at `$PWD`, at the last-opened path, or at `$HOME`? Yazi
   opens at the spawn cwd; we'd default to that, with `:cd ~` as
   the universal home jump.

## 8. Hand-off

- **Plugin sub-spec**: [`specs/research/sy-file-manager-plugins/SPEC.md`][plugin-spec]
  for the binaries-over-stdio plugin runtime.
- **Journey**: run `/journey` against this spec →
  `specs/journeys/JOURNEY-<dt>-sy-file-manager.md`.
- **Roadmap**: run `/roadmap` against the journey →
  `specs/roadmaps/sy-file-manager/ROADMAP.md`.
- **Implement**: `/implement` per roadmap step.
- **NPU**: not applicable (this plane is CPU + GPU rendering only).
- **Workload**: not applicable.

[yazi]: https://github.com/sxyazi/yazi
[yazi-pi]: https://deepwiki.com/sxyazi/yazi/4.4-plugin-api-reference
[niri]: https://github.com/niri-wm/niri
[niri-design]: https://github.com/niri-wm/niri/wiki/Development:-Design-Principles
[niri-rules]: https://github.com/niri-wm/niri/wiki/Configuration:-Window-Rules
[niri-layout]: https://github.com/niri-wm/niri/wiki/Configuration:-Layout
[cachy-niri]: https://wiki.cachyos.org/configuration/desktop_environments/niri/
[wlr-fwt]: https://wayland.app/protocols/wlr-foreign-toplevel-management-unstable-v1
[portal-shortcuts]: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html
[iced-window]: https://docs.rs/iced/latest/iced/window/index.html
[iced-responsive]: https://reintech.io/blog/implementing-graphical-user-interfaces-in-rust
[cosmic-files]: https://github.com/pop-os/cosmic-files
[cf-ops]: https://deepwiki.com/pop-os/cosmic-files/3.1-basic-operations
[cf-release]: https://blog.system76.com/post/cosmic-1-0-8-released/
[itsfoss-tfm]: https://itsfoss.com/terminal-file-managers/
[xplr]: https://www.x-cmd.com/install/25-ls/
[superfile]: https://superfile.dev/
[sf]: https://github.com/yorukot/superfile
[sf-overview]: https://superfile.dev/overview/
[archwiki-nautilus]: https://wiki.archlinux.org/title/GNOME_Files
[dolphin]: https://apps.kde.org/dolphin/
[iouring-blog]: https://dev.to/vincentdu2021/building-a-file-copier-4x-faster-than-cp-using-iouring-4b5n
[iouring-paper]: https://arxiv.org/html/2512.04859v1
[notify-rs]: https://docs.rs/notify
[trash-spec]: https://specifications.freedesktop.org/trash/latest/
[trash-crate]: https://docs.rs/trash
[xdg-mime]: https://crates.io/crates/xdg-mime
[extism]: https://extism.org/
[helix-disc]: https://github.com/helix-editor/helix/discussions/3806
[lsp-perf]: https://kirkryan.co.uk/stdio-vs-streamable-http-choosing-the-right-mcp-transport/
[md-rich-bug]: ../../bugs/
[plugin-spec]: ../sy-file-manager-plugins/SPEC.md
