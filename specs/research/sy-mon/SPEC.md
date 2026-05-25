# SPEC: `sy mon` — on-demand, layer-shell health dashboard for every sy plane

## 1. Summary

`sy mon` is a glanceable, **terminal-aesthetic-but-native-Wayland** health
dashboard for the entire `sy` stack — CPU/RAM, AMDGPU/NVIDIA GPU(s), AMD
XDNA NPU, network, disk, plus the sy planes themselves (aiplane queue
depth and latency, knowledge embed/search throughput, agent runner
denials, power-governor arm and dwell, supervisor plane states). It
opens on `Super+M` as an `iced_layershell` overlay (same UI stack as
`sy stack bar`), reads its data exclusively from a host-local
Prometheus exposition surface that every sy plane already promises to
ship (the deferred Zone 6.2 UDS exporter in
[arch-observability ROADMAP][aobs-roadmap]), and exits cleanly on
`Esc` / `Super+M` toggle. `sy mon --json` prints the same dashboard as
a versioned `SystemSnapshot` for MCP agents.

## 2. Background & Research

### Market context

The "system-monitor with a fancy face" niche splits into three camps,
each informing one design choice for `sy mon`:

- **Terminal-native (btop, bottom, glances, nvtop)**: zero
  dependencies, runs over SSH, instantly recognisable aesthetic.
  [`btop`][btop] uses Unicode block-drawing with a 256-color terminal;
  [`nvtop`][nvtop] (Syllo) is the de facto cross-vendor accelerator
  monitor and explicitly handles AMD/Apple/Huawei/Intel/Qualcomm
  alongside NVIDIA. Limit: terminal coupling — no AA lines, font
  variety locked to the host terminal, font emoji rendering varies.
  Per the user's brief, `sy mon` deliberately departs from this camp
  by being "terminal-ish but not a terminal".
- **Native GUI (Mission Center, Plasma System Monitor, GNOME System
  Monitor)**: [Mission Center][mc] is Rust + GTK4 and is the modern
  reference UX — Windows-Task-Manager-style cards, NPU support landed
  in v1.10 (Feb 2026, [omg!ubuntu][mc-npu]). [Resources][resources]
  (Flatpak, GTK4) does the same with a more compact card layout. Both
  bind directly to libsystemd/procfs/sysfs and re-implement the wheel
  per-vendor. Limit: alien dependency tree (GTK4) and they don't
  consume Prometheus, which is sy's chosen wire.
- **Web dashboards (Netdata, Beszel, Grafana + node_exporter)**:
  [Netdata][netdata] auto-discovers and renders everything; Grafana
  needs configuration. All require a browser, all require an HTTP
  port. Limit: violates sy's single-host, layer-shell-first ethos;
  Mod+M should never spawn a browser process.

The vendor-specific NPU/GPU layer (which `sy mon` must consume) is in
flux in 2026:

- AMD XDNA NPU power telemetry landed in Linux 7.1
  ([Phoronix][phoronix-npu]).
- `amdgpu_top --xdna` ([Umio-Yasuno/amdgpu_top][amdgpu-top]) is the
  reference for AMD GPU + NPU sysfs reads; `sy/src/npu.rs` already
  reads the same `runtime_active_time` / `runtime_suspended_time`
  pair and is the model we extend.
- AMD's own `xrt-smi` ([Ryzen AI docs][xrt-smi]) is Windows-only;
  not usable on Fedora.

**Key takeaway:** existing tools either lock the UX to a terminal,
re-implement vendor reads in C/GTK, or assume a browser. None
consume a stable Prometheus surface for a *single-binary,
single-host* agentic Linux. There is a clear gap, and we already
have most of the pieces in tree.

### Technical context

In-tree primitives we extend rather than re-implement:

- **`iced` 0.14 + `iced_layershell` 0.17 + `plotters` 0.3.7** are
  already workspace deps ([`Cargo.toml`][cargo-toml]). `sy stack bar`
  uses them via the `bar-iced` feature
  ([`src/stack/bar/mod.rs`][bar-mod]).
- **Metric catalogue** is registered in
  [`crates/sy-core/src/metrics.rs`][core-metrics] with
  `metrics::describe_*!` for every counter / gauge / histogram. The
  recorder is wired but the **exporter is not** — the Zone 6.2 UDS
  exposition over `metrics-exporter-prometheus` is explicitly OUT of
  current scope ([arch-observability ROADMAP §483-488][aobs-roadmap]).
- **Sysfs sensors** for the host already live in `src/npu.rs`,
  `src/gpu.rs`, `src/pwr.rs`, `src/net.rs`, `src/disk.rs`,
  `src/bat.rs`. Each reads sysfs/procfs/nvidia-smi and emits a
  **waybar tile**. They are the per-plane shells for the central
  sensors crate we will create.
- **Layer-shell popup pattern** is established in
  [`src/popup.rs`][popup-rs]: PID-file at `/tmp/sy-popup-<key>.pid`,
  toggle behaviour, spawn-or-kill semantics. Today it spawns a `foot`
  terminal; `sy mon` replaces the spawn target with a native iced
  popup but keeps the toggle contract.
- **Keybinds** live in [`configs/niri/config.kdl`][niri-config]; the
  existing pattern is
  `Mod+P { spawn "{{ home }}/.local/bin/sy" "stack" "toggle"; }`.
  `Mod+M` is **verified free** in that file.

### Deep dives

- **`metrics-exporter-prometheus uds-listener` feature**
  ([crate docs][meprom-uds]) provides an in-process HTTP listener
  over a Unix Domain Socket, responding to GET on any path with the
  Prometheus text exposition format. This is the canonical primitive
  for the Zone 6.2 exporter — feature-flag opt-in, no TCP, no
  external HTTP stack on the daemons that don't already have one.
- **Parsing** the exposition format on the consumer side:
  [`prometheus-parse`][promparse] (Apache-2.0, ~200 LoC, zero
  required deps beyond serde/regex) covers TYPE/HELP + samples and
  is the standard choice across Rust exporters. We use it once in
  the aggregator (`sy mon collect`), never in hot paths.
- **`plotters-iced` version skew** ([crates.io][plotters-iced]): the
  current 0.11 release targets iced 0.13; iced 0.14 is in tree
  already and downgrading would regress `sy stack bar`. A community
  fork `plotters-iced2` claims 0.14 but is unmaintained. Conclusion:
  **do not use plotters-iced for live charts.** Use iced's native
  `canvas` feature ([already enabled implicitly][iced-canvas-feat] —
  see Decision D-CHART). Keep `plotters` for static SVG report
  export only, matching how `power report` already uses it.
- **Layer-shell focus on niri** ([niri layer-shell wiki][niri-ls]):
  layer-shell apps get keyboard focus when their `keyboard-
  interactivity=on_demand`; `Esc` is the conventional dismiss key.
  Niri's existing `Mod+Esc allow-inhibiting=false` binding is
  preserved so the popup can never lock the user out.
- **Aiplane single-context constraint** ([`AGENTS.md`][agents]):
  one process per NPU. `sy mon` MUST NOT open ONNX/VitisAI sessions
  itself; it consumes metrics that the aiplane daemon already emits.
- **History without an always-on UI** is the central architectural
  question. The reference here is Netdata's "agent" model — a tiny
  scraper daemon with a ring buffer, with the GUI being a thin
  client. We mirror that with `sy mon collect` (the aggregator) and
  `sy mon` (the popup client), both modes of the same binary.

[btop]: https://github.com/aristocratos/btop
[nvtop]: https://github.com/Syllo/nvtop
[mc]: https://missioncenter.io/
[mc-npu]: https://www.omgubuntu.co.uk/2026/02/resources-amd-npu-monitoring-linux
[resources]: https://apps.gnome.org/Resources/
[netdata]: https://github.com/netdata/netdata
[phoronix-npu]: https://www.phoronix.com/news/Ryzen-AI-NPU-Linux-Power-Metric
[amdgpu-top]: https://github.com/Umio-Yasuno/amdgpu_top
[xrt-smi]: https://ryzenai.docs.amd.com/en/latest/xrt_smi.html
[cargo-toml]: ../../../Cargo.toml
[bar-mod]: ../../../src/stack/bar/mod.rs
[core-metrics]: ../../../crates/sy-core/src/metrics.rs
[aobs-roadmap]: ../../roadmaps/arch-observability/ROADMAP.md
[popup-rs]: ../../../src/popup.rs
[niri-config]: ../../../configs/niri/config.kdl
[meprom-uds]: https://docs.rs/metrics-exporter-prometheus/latest/metrics_exporter_prometheus/struct.PrometheusBuilder.html
[promparse]: https://crates.io/crates/prometheus-parse
[plotters-iced]: https://crates.io/crates/plotters-iced
[iced-canvas-feat]: https://docs.rs/iced/0.14
[niri-ls]: https://github.com/YaLTeR/niri/wiki/Layer%E2%80%90Shell-Components
[agents]: ../../../AGENTS.md

## 3. Proposal

### Approach

Three cooperating pieces, all inside the existing `sy` binary:

1. **Producer side — `mon-exporter` feature on every plane.** Each
   daemon (`aiplane`, `knowledge`, `agt`, `supervisor`, `stack bar`,
   `power`, `wallpaper`) enables `metrics-exporter-prometheus`'s
   `uds-listener` feature and listens on
   `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock`. This activates the
   already-deferred Zone 6.2 work in `arch-observability`. Planes
   that have no daemon (host sensors, knowledge CLI one-shots) are
   served by the aggregator directly.
2. **Aggregator — `sy mon collect`.** A new long-lived subcommand,
   supervised by `sy-mon-collect.service` (WantedBy=`sy.target`).
   On a 1Hz tick it (a) scrapes every plane UDS, (b) samples host
   sensors (`/proc`, `/sys/class/...`, `nvidia-smi` when present)
   via a new `crates/sy-core/src/sensors/` module that absorbs the
   read-fns currently scattered across `src/npu.rs`, `src/gpu.rs`,
   `src/pwr.rs`, `src/net.rs`, `src/disk.rs`, `src/bat.rs`, and
   (c) writes a fixed-size, mmap-backed ring buffer at
   `$XDG_RUNTIME_DIR/sy/mon/history.bin`. It also serves a
   `system.mon.snapshot` IPC op on the existing sy-ipc UDS that
   returns the latest snapshot as JSON.
3. **UI client — `sy mon` (no args toggle, `open`, `close`, `--json`).**
   A layer-shell `iced_layershell` overlay anchored centre-top,
   `keyboard-interactivity=on_demand`. On open it (i) reads the ring
   buffer for instant history, (ii) subscribes to a 1Hz IPC tick from
   the aggregator for live frames, (iii) renders a grid of panels
   using iced's native `Canvas` widget for sparklines/area
   charts/gauges and `Text` (JetBrainsMono Nerd Font, matching
   `src/popup.rs`) for numeric readouts. Esc or another Mod+M closes
   the window via the PID-file pattern from `src/popup.rs`.

The wire is **always** Prometheus exposition — there is exactly one
"how do plane metrics get out of a process" answer in sy after this
spec lands, and external Grafana / `socat` users get it for free.

### Key decisions

| ID | Decision | Choice | Reasoning | Alternatives |
|---|---|---|---|---|
| D-WIRE | Producer→aggregator wire format | `metrics-exporter-prometheus` `uds-listener` per daemon at `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock` | Activates already-planned Zone 6.2 work ([arch-observability ROADMAP §483-526][aobs-roadmap]); single, well-documented exposition; external Grafana scrapes via `socat`; no bespoke wire | (a) Bespoke `system.metrics.snapshot` IPC op — duplicates `metrics::Recorder` plumbing and locks in sy-only consumers. (b) StatsD UDP — push not pull; CNCF-deprecated direction. (c) OTLP — pulls collector deps; explicit anti-goal in K6 of [architecture-refactor SPEC][arch-spec] |
| D-AGG | Aggregator placement | New `sy mon collect` daemon, separate plane under `sy.target` | History needs continuous recording, which the popup can't do; supervisor already has enough responsibility; per-plane history would force N-fan-out on every popup open | (a) Bake history into supervisor — bloats blast radius. (b) Per-plane ring buffer — fan-out latency, no cross-plane correlation. (c) Live-only popup (no history) — defeats sparklines, defeats the whole feature |
| D-CHART | Live chart rendering | iced 0.14 native `Canvas` widget; bespoke `Sparkline`, `AreaChart`, `Gauge`, `Heatmap` widgets in `src/mon/widgets/` | `plotters-iced` 0.11 tracks iced 0.13; downgrade would regress `sy stack bar`. iced `Canvas` (`Frame`/`Path`/`Stroke`) handles every chart sy mon needs in <300 LoC each; no upstream pin risk | (a) `plotters-iced` — version skew blocker. (b) `plotters → SVG → usvg → tiny-skia → iced::image` — works (this is how `power report` ships static images) but is too heavy for 1Hz live frames. (c) Custom wgpu shaders — yak-shaving |
| D-FRAME | Window/anchor strategy | Layer-shell overlay anchored centre-top, 1280×800 default, `keyboard-interactivity=on_demand`, `Esc`/Mod+M dismisses | Matches `sy stack bar`'s already-vetted niri integration; on-demand keyboard never steals focus from a typing user; Esc/Mod+M is the convention reinforced by `Mod+Slash` cheatsheet popup already in `configs/niri/config.kdl` | (a) Full-screen kiosk — wastes pixels, hides what user was doing. (b) `xdg-popup` anchored to waybar — depends on a waybar tile we don't have. (c) Always-on hidden window — wastes wgpu surface |
| D-TOGG | Toggle semantics | PID-file at `/tmp/sy-popup-mon.pid`; subsequent `sy mon` invocations kill the live popup | Reuses the proven [`src/popup.rs`][popup-rs] pattern that backs `Mod+P`, `Mod+A`. One keybind, two outcomes — already a learned mental model for sy users | (a) Two keybinds (open/close) — burns Mod-key real-estate. (b) Daemon-mode bar that hides — wastes a wgpu surface in the idle case. (c) systemd-socket-activated popup — `iced_layershell` can't accept a passed fd |
| D-SCHEMA | `--json` schema home | `crates/sy-core/src/mon/snapshot.rs::SystemSnapshot` with `#[serde]` + `schema_version: u32` | Same struct serves UI, IPC op, MCP tool; schema_version makes drift visible; sy-core is the natural home (already the metrics catalogue's home) | (a) Per-panel JSON ops — N round-trips, no atomicity. (b) Raw Prom passthrough — forces every agent to embed a Prom parser. (c) New crate — premature; nothing else needs this struct |
| D-AESTHETIC | Visual style | Monospace (`JetBrainsMono Nerd Font`, matching `src/popup.rs`); Nerd-Font glyphs for plane icons; iced `Canvas` lines & area fills sampled at theme accent colours from `configs/sy/themes/`; thin 1px borders mirroring `sy stack bar`'s tile chrome | Reads the user's "terminal-ish but not a terminal" brief literally: mono typography + glyphs + flat fills evoke btop without inheriting terminal limits; reuses theme tokens so the whole sy surface stays coherent | (a) Pure Material/Cosmic chrome — looks alien against `sy stack bar`. (b) ASCII-art borders via `Text` — loses sub-pixel sharpness on HiDPI. (c) Per-panel emoji icons — depends on system emoji font; varies per machine |
| D-NPU | NPU data source | sysfs (`/sys/class/accel/accel0/...`) read by the aggregator; falls back to amdgpu_top JSON if sysfs schema changes | Already proven in [`src/npu.rs`][npu-rs]; matches the `runtime_active_time` / `runtime_suspended_time` deltas convention. Sysfs is the kernel-stable interface; amdgpu_top is the documented fallback | (a) `xrt-smi` — Windows only. (b) Open an XRT session — would steal `/dev/accel/accel0` from the aiplane daemon (single-context constraint). (c) eBPF kfunc trace — needs CAP_BPF, out of scope for desktop |

### Scope

Every item below is part of this feature's correct, useful end state.
Nothing here is "v1 / phase 1 / later".

1. **`mon-exporter` feature on every existing plane daemon**
   - Cargo feature on the workspace crate, enabled by default in
     release builds. Each daemon's `main.rs` calls
     `PrometheusBuilder::new().with_http_uds_listener(path).install()`
     in its startup path, behind an `Option<UdsBind>` that defaults
     to `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock`.
   - Per-plane socket cleanup on graceful shutdown (Unix idiom: unlink
     on `SIGTERM` / `Drop` of the listener guard).
   - SELinux file context entry for each socket path under
     `configs/selinux/sy.fc`.
2. **`crates/sy-core/src/sensors/` — shared host-sensor crate**
   - `cpu` (procfs /proc/stat per-core + /sys/devices/system/cpu freq
     + thermal_zone), `mem` (/proc/meminfo), `net` (/proc/net/dev),
     `disk` (/proc/diskstats + /sys/block/*/queue), `bat`
     (/sys/class/power_supply), `gpu_amd`
     (/sys/class/drm/card*/device/{gpu_busy_percent,mem_info_vram_*,
     hwmon/.../temp1_input,hwmon/.../power1_average}), `gpu_nvidia`
     (`nvidia-smi --query-gpu=… --format=csv,noheader` JSON parse),
     `npu_xdna` (the existing `src/npu.rs` logic, promoted), `power`
     (already in `src/power/` — adapter only).
   - Each sensor exposes `fn sample() -> Sample` returning a typed
     struct; the aggregator owns the polling loop.
   - The existing waybar tile commands (`sy npu --waybar`, `sy gpu
     --waybar`, etc.) get rewritten to call these shared sample fns
     so there is one read path per metric, not two.
3. **`sy mon collect` aggregator**
   - `clap` subcommand under the top-level `sy mon` group.
   - Tokio-multi-thread runtime; one task per scrape source (plane
     UDS + host sensors).
   - Ring buffer: `crates/sy-core/src/mon/ring.rs` — fixed N×M f32
     grid (default N=600 seconds, M=metric count), mmap-backed,
     `[u8; 32]` magic header with seq counter for crash detection.
   - IPC op: `system.mon.snapshot` on the existing sy-ipc UDS,
     returns `SystemSnapshot` JSON.
   - IPC op: `system.mon.subscribe` — emits one frame per tick over
     the open stream (used by the popup; cancellable on stream
     close).
   - systemd unit `configs/systemd/user/sy-mon-collect.service` with
     `Restart=on-failure`, `Type=notify`, `WantedBy=sy.target`.
4. **`sy mon` (the popup) — iced + iced_layershell**
   - New `src/mon/` tree: `cli.rs`, `app.rs`, `view/`, `widgets/`
     (sparkline, area_chart, gauge, heatmap, tile, header), `theme.rs`
     (re-exports `src/stack/bar/theme.rs` tokens).
   - Layer-shell config: anchor centre, size 1280×800, exclusive
     zone 0, `keyboard-interactivity=on_demand`.
   - Subscription to `system.mon.subscribe` over sy-ipc; reads ring
     buffer on first paint for instant history.
   - Panels (one tile per row block):
     - **Host**: CPU sparkline grid (per-core), RAM/swap gauge, load avg.
     - **Accel**: per-GPU util/VRAM/temp/power; NPU util/power/state;
       active workload label streamed from aiplane queue.
     - **Net**: per-interface rx/tx sparkline + total counter.
     - **Disk**: per-device IO sparkline + capacity ring.
     - **Aiplane**: queue-depth bars per workload kind, warm-pool
       gauges, p99-latency histogram (from `sy_workload_latency_seconds`).
     - **Knowledge**: indexed-doc counter, embed throughput sparkline,
       search QPS, qdrant collection count.
     - **Agents**: running-agent count, RSS sum, policy denial
       sparkline (`sy_policy_denials_total`).
     - **Power**: current shield arm, dwell distribution donut,
       cumulative regret line (from `src/power/report/metrics.rs`).
     - **Supervisor**: plane-state grid (green/yellow/red per plane)
       + restart counters.
   - Keybinds inside the popup:
     - `Esc` / `Mod+M` / click-outside: close.
     - `Tab` / `Shift+Tab`: cycle panel focus.
     - `Enter`: expand focused panel to full window.
     - `1`..`9`: jump to panel N.
     - `/`: filter overlay (regex on metric name).
     - `j`/`k`: scroll panel grid.
   - PID-file at `/tmp/sy-popup-mon.pid` for toggle (reuses
     `src/popup.rs::toggle` after generalising it to accept native
     `sy` subcommands, not just `foot` spawns).
5. **`sy mon --json` / `sy mon snapshot --json`**
   - Identical output: one `SystemSnapshot` JSON document to stdout,
     exit 0 on success.
   - Talks to `sy mon collect` via the existing sy-ipc client; if
     aggregator is down, exits 3 (drift, per CLAUDE.md) with a
     stderr message naming the unit.
6. **`sy mon doctor`**
   - Linear-check shape (like `sy doctor`): for each plane, verifies
     the UDS exists, is connect()-able, returns valid Prom on GET.
     For each host sensor, verifies the sysfs path exists. Emits
     `--json` machine output.
7. **MCP surface**
   - New MCP tool `system.mon.snapshot` (no args) → `SystemSnapshot`.
   - New MCP tool `system.mon.history` (args:
     `metric: String, seconds: u32`) → array of `(ts, value)` from
     the ring buffer.
   - Wired in `src/auto_mcp.rs` (the existing MCP route table).
8. **`configs/niri/config.kdl` keybind**
   - `Mod+M hotkey-overlay-title="sy mon — system dashboard" {
       spawn "{{ home }}/.local/bin/sy" "mon"; }`.
9. **`configs/waybar/` indicator**
   - Optional waybar tile `sy mon --waybar` that shows a single
     condensed "system status" glyph (green/yellow/red) sourced from
     the same aggregator snapshot. Click action: spawns `sy mon`.
10. **Theme integration**
    - `src/mon/theme.rs` reads `{{ ui.accent }}`, `{{ colors.bg }}`,
      `{{ colors.bg2 }}` etc. from `configs/sy/themes/<active>.toml`
      (the same source `configs/niri/config.kdl` templates from).
11. **Tests**
    - Unit tests for every sensor parser (fixture-driven).
    - Integration tests for the aggregator (fake plane UDS that
      serves canned Prom; assert ring buffer state).
    - PID-file toggle round-trip test (spawn/kill/respawn idempotent).
    - `sy mon --json` snapshot golden file under
      `tests/snapshots/mon/`.
    - Layer-shell headless E2E gated on a `test-mon-niri` feature
      that launches niri in nested mode (used by stack-bar tests
      already? if not, document the recipe).
12. **Docs**
    - `docs/agents/mon-schema.md` documenting `SystemSnapshot` for
      MCP consumers; `schema_version` SemVer policy.
    - Update `README.md` "Keybinds" + "Surfaces" sections.
    - `CHANGELOG.md` entry under a new release section.

### Anti-goals

- **Remote Prometheus scrape / push gateway.** sy is single-host; a
  network egress surface would require trust-boundary work, secret
  management, and firewall rules — explicit snowflake hazard. If a
  user wants remote scraping, they bridge the UDS to TCP with
  `socat` themselves (documented in `docs/admin/mon-remote.md`),
  which keeps the trust boundary outside sy.
- **Mutating actions inside the popup** (kill workload, drain
  queues, restart agents). `sy mon` is read-only. State change goes
  through the existing subcommands (`sy aiplane`, `sy agt`,
  `sy knowledge`, …) which already enforce `--yes`/`--dry-run`.
  Mixing mutation into a glanceable surface invites accidental
  destructive clicks — a security-boundary concern, not a scope cut.
- **Per-process htop-style drill-down.** sy mon is a *sy planes*
  dashboard, not a process manager. `btop` and `Mission Center`
  already serve that need extremely well; we don't compete with
  them because doing so dilutes the plane-centric mental model.
- **Embedding Grafana / a browser**. Pulls a Chromium/Node trust
  boundary into sy. Architectural mismatch with single-binary.
- **OpenTelemetry / OTLP wire**. The architecture-refactor SPEC's
  K6 decision explicitly classifies OTLP-from-day-one as overkill
  ([SPEC §3.2 K6][arch-spec-k6]); we honour that. A future `--otlp`
  layer is one `tracing` Layer away if it ever lands.
- **Hyprland / KDE Plasma / GNOME ports.** sy targets the niri
  rice; the popup uses wlr-layer-shell. Cross-compositor support
  is a vendor-neutral concern of the layer-shell protocol, not of
  sy mon — if iced_layershell works on another wlroots compositor,
  great, but we don't test or document those targets.

[arch-spec]: ../architecture-refactor/SPEC.md
[arch-spec-k6]: ../architecture-refactor/SPEC.md
[npu-rs]: ../../../src/npu.rs

## 4. Technical Design

### Architecture

```
                    ┌────────────────────────────────────────────────┐
                    │                  sy.target                     │
                    │                                                │
   ┌──────────────┐ │ ┌────────────────┐  scrape (1 Hz)  ┌──────────┐│
   │ aiplane      │─┼─│ /run/.../      │◄────────────────│  sy mon  ││
   │ daemon       │ │ │ aiplane/       │                 │ collect  ││
   │ + uds expose │ │ │ metrics.sock   │                 │ daemon   ││
   └──────────────┘ │ └────────────────┘                 └────┬─────┘│
                    │                                         │      │
   ┌──────────────┐ │ ┌────────────────┐                      │      │
   │ knowledge    │─┼─│ knowledge/     │◄─────────────────────┤      │
   │ daemon       │ │ │ metrics.sock   │                      │      │
   └──────────────┘ │ └────────────────┘   sample (1 Hz)      │      │
                    │ ┌────────────────┐                      │      │
   ┌──────────────┐ │ │ /proc, /sys    │◄─────────────────────┤      │
   │ host kernel  │─┼─│ AMDGPU/XDNA    │                      │      │
   └──────────────┘ │ │ nvidia-smi     │                      │      │
                    │ └────────────────┘                      ▼      │
                    │                              ┌──────────────────┐
                    │                              │ history.bin      │
                    │                              │ (mmap ring buf)  │
                    │                              └─────┬────────────┘
                    │                                    │             │
                    │   IPC: system.mon.snapshot,        │             │
                    │   system.mon.subscribe             ▼             │
                    │                              ┌──────────────────┐│
                    │                              │ sy-ipc UDS       ││
                    │                              └─────┬────────────┘│
                    └────────────────────────────────────┼─────────────┘
                                                         │
       Mod+M (niri)                                      │
            │                                            ▼
            ▼                                  ┌──────────────────┐
       ┌─────────┐  spawn or kill          ┌───│ sy mon (popup)   │
       │ src/    │─────────────────────────►   │ iced+layershell  │
       │ popup.rs│  /tmp/sy-popup-mon.pid  └───│ Canvas widgets   │
       └─────────┘                             └──────────────────┘
```

**Modules touched / created**:

| Path | Verb | Notes |
|---|---|---|
| `src/mon/mod.rs` | NEW | top-level `sy mon` subcommand wiring |
| `src/mon/cli.rs` | NEW | clap definitions for `mon` / `mon collect` / `mon snapshot` / `mon doctor` |
| `src/mon/app.rs` | NEW | iced `Application` + layer-shell config |
| `src/mon/view/{host,accel,net,disk,aiplane,knowledge,agents,power,supervisor}.rs` | NEW | one panel renderer per plane |
| `src/mon/widgets/{sparkline,area_chart,gauge,heatmap,tile,header}.rs` | NEW | iced `Canvas`-based custom widgets |
| `src/mon/theme.rs` | NEW | reads theme tokens, exports `Palette` |
| `src/mon/collect/{mod,scrape,sample,ring,ipc}.rs` | NEW | aggregator daemon |
| `crates/sy-core/src/sensors/{cpu,mem,net,disk,bat,gpu_amd,gpu_nvidia,npu_xdna,power}.rs` | NEW | shared sensor read-fns |
| `crates/sy-core/src/mon/snapshot.rs` | NEW | `SystemSnapshot`, `schema_version` |
| `crates/sy-core/src/mon/ring.rs` | NEW | mmap ring buffer |
| `crates/sy-ipc/src/reserved.rs` | EDIT | reserve `system.mon.snapshot`, `system.mon.subscribe` op names |
| `src/npu.rs` `src/gpu.rs` `src/pwr.rs` `src/net.rs` `src/disk.rs` `src/bat.rs` | EDIT | thin adapters that re-export from `sensors::*` (waybar tiles keep their CLI shape) |
| `src/popup.rs` | EDIT | accept native sy subcommands as toggle targets (currently `foot`-only); add `mon` key |
| `src/aiplane/{daemon,main}.rs` | EDIT | enable `mon-exporter` feature; bind UDS at startup |
| `src/knowledge/daemon.rs` | EDIT | same |
| `src/agt/daemon.rs` | EDIT | same |
| `src/supervision/mod.rs` | EDIT | same; supervisor emits its own metrics (plane states) |
| `src/stack/bar/mod.rs` | EDIT | same; gauge: hover-popup latency |
| `src/power/cli.rs` | EDIT | same |
| `src/auto_mcp.rs` | EDIT | register `system.mon.snapshot`, `system.mon.history` MCP tools |
| `src/doctor/checks/mod.rs` | EDIT | add `mon_collect_running`, `plane_metrics_socket{plane}` checks |
| `configs/systemd/user/sy-mon-collect.service` | NEW | aggregator unit, WantedBy=sy.target |
| `configs/systemd/user/sy.target.wants/sy-mon-collect.service` | NEW | symlink |
| `configs/niri/config.kdl` | EDIT | `Mod+M` binding |
| `configs/waybar/config.jsonc` | EDIT | optional `custom/sy-mon` tile |
| `configs/selinux/sy.fc` | EDIT | file contexts for new UDS paths |
| `Cargo.toml` | EDIT | new features `mon-exporter`, `mon-ui`; new deps `prometheus-parse`, `memmap2` (if not already transitive) |
| `tests/mon/` | NEW | integration tests |
| `docs/agents/mon-schema.md` | NEW | `SystemSnapshot` documentation |
| `README.md` `CHANGELOG.md` | EDIT | user-visible additions |

### Non-functional requirements

- **Performance**:
  - First-paint latency (Mod+M → first pixel) ≤ 150 ms p95 on Ryzen
    AI 9 HX 370 reference machine. Measured by a deterministic test
    that spawns `sy mon` and reads its `--ready-fd` (Unix pipe sent
    by the iced app once the first frame renders).
  - Aggregator scrape tick budget: 1 s wall, ≤ 25 ms CPU @ p99.
  - Popup steady-state: ≤ 2 % CPU on the reference machine; wgpu
    surface bound to 60 Hz max with redraw triggered only on data
    update or pointer/keyboard input (no idle re-render).
  - Memory ceiling: aggregator RSS ≤ 80 MiB (default ring buffer +
    rotational counters); popup RSS ≤ 200 MiB while open.
- **Reliability**:
  - Aggregator: `Restart=on-failure`, `RestartSec=2`, validates ring
    buffer magic on startup; rebuilds if corrupt.
  - Popup: graceful fall-back if aggregator is down — shows a banner
    "sy-mon-collect down" with last cached frame from history.bin
    timestamp; still consumes Esc/Mod+M to exit; exit code 3 on
    `--json` to surface drift to scripts.
  - Per-plane scrape: per-source timeout 500 ms, failure tagged in
    snapshot's `errors[]`, never blocks the tick.
  - Single-context NPU is honoured: `sy mon` opens no ONNX session.
- **Security**:
  - All UDS at `$XDG_RUNTIME_DIR` (user-private, mode 0600).
  - SELinux file contexts in `configs/selinux/sy.fc`.
  - Inputs from sysfs are bounded reads (max 4 KiB per file) with
    `read_to_string` + length check; no `eval`-like surfaces.
  - `nvidia-smi` invocation uses fixed argv with no user-supplied
    fragments.
  - No CAP_* requested by `sy mon collect` (sysfs reads, no
    privileged ops).
- **Observability**:
  - `tracing` spans: `mon.tick`, `mon.scrape{plane}`, `mon.sample
    {sensor}`, `mon.render`, `mon.subscribe.tx`.
  - Aggregator emits its own metrics: `sy_mon_scrape_errors_total
    {plane,reason}`, `sy_mon_tick_duration_seconds`,
    `sy_mon_history_dropped_total`, `sy_mon_clients_connected`. These
    feed back into the dashboard's "Supervisor" panel (recursive but
    not circular — collect scrapes itself last).
  - Structured JSON stderr logs with `target = "mon"` for `--log-
    format json` consumers.
  - `sy doctor` linear checks: `mon_collect_running`,
    `plane_metrics_socket{plane=...}`, `mon_history_writable`.

### CLI / MCP surface

```
$ sy mon --help
Open or toggle the sy system-health dashboard popup.

Usage:
  sy mon                         Toggle the popup (open if closed, close if open)
  sy mon open                    Idempotent open
  sy mon close                   Idempotent close
  sy mon snapshot [--json]       Print a SystemSnapshot to stdout
  sy mon collect [OPTIONS]       Run the aggregator daemon (supervised)
  sy mon doctor [--json]         Validate every metrics socket & sensor path

Options for `sy mon collect`:
      --history-size N           Ring buffer depth in seconds
                                 (env SY_MON_HISTORY_SIZE, default 600)
      --tick-ms N                Sample interval in ms
                                 (env SY_MON_TICK_MS, default 1000)
      --bind PATH                IPC socket path
                                 (env SY_MON_BIND, default $XDG_RUNTIME_DIR/sy/mon.sock)
      --history-path PATH        Ring buffer file path
                                 (env SY_MON_HISTORY_PATH, default $XDG_RUNTIME_DIR/sy/mon/history.bin)

Common options (all subcommands):
  -v, --verbose
  -q, --quiet
      --json                     Machine-readable output
      --log-format <fmt>         pretty | json (env SY_LOG_FORMAT)
      --no-color                 Honour NO_COLOR
  -h, --help
      --version

Exit codes:
  0   ok
  1   generic error
  2   usage error
  3   drift / aggregator down / metric socket unreachable
```

`SY_*` env vars cover every flag, per CLIG. Precedence: flag > env >
defaults (no config-file knobs for `sy mon` — the theme tokens come
from `configs/sy/themes/`, which is already config-file territory).

**MCP tools** (registered in `src/auto_mcp.rs`):

```json
{
  "name": "system.mon.snapshot",
  "description": "Return a one-shot SystemSnapshot of every sy plane.",
  "input_schema": {"type": "object", "properties": {}},
  "output_schema": {"$ref": "#/components/schemas/SystemSnapshot"}
}
{
  "name": "system.mon.history",
  "description": "Return ring-buffer samples for a specific metric over the last N seconds.",
  "input_schema": {
    "type": "object",
    "required": ["metric", "seconds"],
    "properties": {
      "metric": {"type": "string"},
      "seconds": {"type": "integer", "minimum": 1, "maximum": 600}
    }
  },
  "output_schema": {
    "type": "array",
    "items": {"type": "array", "prefixItems": [{"type": "integer"}, {"type": "number"}]}
  }
}
```

**`SystemSnapshot` JSON schema (excerpt)** — full schema in
`docs/agents/mon-schema.md`:

```json
{
  "schema_version": 1,
  "captured_at_ms": 1747900000000,
  "cpu": {
    "per_core_util_pct": [12.3, 4.1, ...],
    "freq_mhz": [3800, 3800, ...],
    "temp_c": 58.2,
    "load_avg": [1.42, 1.10, 0.95]
  },
  "mem": {"total_mib": 32768, "used_mib": 14210, "swap_used_mib": 0},
  "gpu": [
    {"vendor": "amd", "name": "Radeon 890M", "util_pct": 4, "vram_used_mib": 512, "vram_total_mib": 8192, "temp_c": 49.0, "power_w": 6.3}
  ],
  "npu": {"vendor": "amd-xdna", "util_pct": 73, "active": true, "fw_version": "1.5.10", "power_w": 4.2, "holders": ["sy-aiplane"]},
  "net": [...],
  "disk": [...],
  "aiplane": {"queue_depth": {"embed": 0, "rerank": 2}, "warm": {"embed": 1, "rerank": 1}, "latency_p99_ms": {"embed": 18.4, "rerank": 41.0}, "errors_total": 0},
  "knowledge": {"collections": 4, "docs_indexed": 17402, "embed_throughput_docs_per_s": 32.1, "search_qps": 0.4},
  "agents": {"running": 2, "rss_total_mib": 412, "policy_denials_recent": 0},
  "power": {"current_arm": "balanced", "dwell_pct": {"perf": 0.18, "balanced": 0.71, "save": 0.11}, "regret_cum": 0.034},
  "supervisor": {"planes": [{"name": "aiplane", "state": "active", "restarts": 0}, ...]},
  "errors": []
}
```

### Testing strategy

- **Unit**:
  - `sensors::cpu::parse_proc_stat` — fixtures for 16-core Ryzen,
    NUMA, hot-plug, missing data.
  - `sensors::npu_xdna::parse_pm_runtime` — first-tick, wrap-around,
    counter reset.
  - `sensors::gpu_amd::parse_drm_card` — fixtures for present/absent
    iGPU + dGPU.
  - `mon::ring::push_pop` — wrap-around, magic header validation,
    seq monotonicity.
  - `mon::scrape::prom_to_snapshot` — captured `/metrics` docs from
    each plane as fixtures under `tests/fixtures/mon/prom/<plane>/`.
  - `mon::cli::parse_args` — every flag/env precedence rule.
- **Integration** (daemon-in-thread + fake UDS):
  - `mon::collect::fake_planes_yield_snapshot`: spin up two fake
    plane UDS servers that return canned exposition; assert the
    aggregator's ring buffer reflects them after N ticks.
  - `mon::collect::scrape_timeout_does_not_block_tick`: one plane
    blackholes; tick still completes within budget; failure tagged
    in `errors[]`.
  - `mon::collect::history_corruption_rebuilds`: pre-corrupt
    `history.bin`; aggregator restart rewrites magic, starts fresh.
  - `mon::ipc::snapshot_roundtrip`: client connects, `--json`
    payload deserialises into `SystemSnapshot`.
  - `mon::popup::pid_file_toggle`: spawn / re-invoke / kill /
    re-invoke / spawn → all idempotent.
- **End-to-end / manual**:
  - `recipes/mon/local-smoke.md` — start sy.target, hit Mod+M, take
    a screenshot, diff against `tests/snapshots/mon/reference.png`.
  - Manual: under `sy aiplane run --workload embed --batch 1024`
    the NPU panel reflects load within 2 ticks.
  - Manual: kill `sy-mon-collect`; popup shows "down" banner and
    last cached frame; `sy mon --json` exits 3.
  - Headless layer-shell E2E gated on `test-mon-niri` feature
    (mirrors `sy stack bar`'s nested-niri pattern).

### Migration & compatibility

- **New on-disk artefacts** (all under `$XDG_RUNTIME_DIR`, ephemeral):
  `$XDG_RUNTIME_DIR/sy/mon/history.bin` (mmap ring buffer);
  `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock` per plane;
  `/tmp/sy-popup-mon.pid` (popup PID file, matches existing pattern).
- **Schema versioning**: `SystemSnapshot.schema_version: u32`,
  starts at 1. Breaking changes bump major and add a deprecation
  notice in `CHANGELOG.md`. The MCP tools accept an optional
  `min_schema_version` field (additive, no breaking change).
- **Backward compat for planes without the exporter**: the
  aggregator silently treats a missing socket as a zero-metric
  source; `sy mon doctor` is the surface for surfacing it. This
  means rolling out `mon-exporter` to each plane can land in any
  order without breaking `sy mon`.
- **No qdrant / model-cache layout changes.** `sy mon` is a
  read-only consumer of state owned elsewhere.

### Dependencies

| Crate | Version | New? | Notes |
|---|---|---|---|
| `iced` | 0.14 | already in tree | reuse `bar-iced` feature toolchain |
| `iced_layershell` | 0.17 | already in tree | reuse |
| `metrics` | 0.23 | already in tree | reuse |
| `metrics-exporter-prometheus` | 0.15 | already in tree, **feature `uds-listener` newly enabled** | upstream-supported; pin `+http-listener` for the UDS bind |
| `prometheus-parse` | ~0.2 | NEW | Apache-2.0; aggregator parses scraped docs; small surface, no async deps |
| `memmap2` | ~0.9 | likely already transitive (verify) | ring-buffer mmap |
| `nix` | already in tree | reuse | for `flock` on history file |
| `plotters` | 0.3.7 | already in tree | not used by live popup; reserved for static SVG export from `sy mon snapshot --svg <path>` follow-on (deferred but listed under SCOPE only if added; see Anti-goals if not) |

We deliberately **do not** add `plotters-iced` (version-skew per
D-CHART) and **do not** add `tonic`/`hyper`/`axum` to the aggregator
(the existing `metrics-exporter-prometheus uds-listener` ships its
own HTTP/1.1 minimal stack).

## 5. User Journey Sketch

(Will be expanded by `/journey` into `specs/journeys/JOURNEY-<dt>.md`.)

- **Actor & context**: power user at the niri keyboard ("did my NPU
  workload kick off?"); MCP-driven agent that wants a coherent
  snapshot ("what is this machine doing right now before I plan my
  next action?"); operator post-incident ("which plane crashed?").
- **Trigger**: suspicion or routine glance; pre/post NPU run
  sanity check; agent reflective step.
- **Phases**:
  1. **Open**: `Super+M` → niri spawns `sy mon` → PID file shows no
     live popup → iced opens centred overlay → first frame paints
     from ring buffer (instant history) → 1Hz IPC tick keeps it
     live.
  2. **Glance**: user scans CPU/RAM cards; eyes flick to NPU card,
     sees util at 73 % and active workload `embed`. Aiplane card
     shows queue depth 2 for `rerank`, p99 41 ms — within budget.
  3. **Drill-in**: user Tab-focuses NPU card, presses Enter → card
     expands; sees per-second util sparkline, power line, current
     holders list, last 5 workload-completion events.
  4. **Headless variant**: agent calls MCP `system.mon.snapshot` →
     receives same struct → reasons about whether to schedule
     another embed batch right now.
  5. **Dismiss**: `Esc` → popup closes; PID file removed; aggregator
     keeps recording.
- **North star**: `Super+M` → answer in under 2 seconds; agent
  reasons over `SystemSnapshot` without scraping anything.

### Friction map

| Friction | Phase | Opportunity |
|---|---|---|
| User doesn't know `Super+M` opens mon | Discovery | `hotkey-overlay-title="sy mon — system dashboard"` on the niri binding surfaces it in the Mod+Slash cheatsheet that already exists |
| Popup steals focus from a typing context | Open | Layer-shell `keyboard-interactivity=on_demand`; user must intentionally hit Esc/Mod+M (no auto-grab); `Mod+Esc` (already configured) is the universal escape hatch |
| User wants only the NPU panel | Glance | Tab/Enter expands the focused panel to full window; number keys `1`..`9` jump direct |
| Aggregator down → empty popup | Open | Doctor surfaces "sy-mon-collect down"; popup shows a banner with last frame; exit code 3 on `--json` so scripts notice |
| `--json` schema drift breaks agent scripts | Headless | `schema_version` field + `docs/agents/mon-schema.md` + SemVer policy; major bump is a `CHANGELOG.md`-gated event |
| Metric name typo in MCP `history` call | Headless | `system.mon.history` returns an error with the closest catalogue match (Levenshtein over `CORE_METRICS`) |
| NPU sysfs schema changes in kernel upgrade | Open / Glance | `sensors::npu_xdna` is versioned (`v1`/`v2`); falls back to `amdgpu_top --xdna` parse if both fail; doctor warns |
| User on Hyprland/KDE wants this too | Out of audience | Documented as best-effort cross-wlroots — works wherever `iced_layershell` works, no test coverage promised |

## 6. Risks & Mitigation

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| `iced_layershell` 0.17 + niri output hot-plug → popup mispositioned | misleading dashboard | M | `output_name` bind; recompute on `OutputChanged`; nested-niri E2E covers the hot-plug path |
| `metrics-exporter-prometheus uds-listener` pulls a second HTTP stack into daemons that already have one | binary bloat | L | gate behind `mon-exporter` feature; release-build default-on but a `--no-default-features` opt-out is supported; verify via `cargo tree` in CI |
| Per-plane scrape fan-out at popup open is slow | sluggish first-paint | M | aggregator pre-scrapes on a steady tick; popup reads ring buffer FIRST, then subscribes — first frame never blocks on a live scrape |
| AMD XDNA sysfs path renames in Linux 7.x | NPU panel goes blank | H | versioned `sensors::npu_xdna::{v1,v2}`; doctor check; documented fallback to `amdgpu_top --xdna --json` parse |
| iced popup steals focus from a fullscreen game | user lockout | L | `keyboard-interactivity=on_demand`; `Mod+Esc allow-inhibiting=false` (already in niri config) is the global escape |
| Ring-buffer corruption on aggregator OOM-kill | first frame after restart is wrong | L | mmap magic + monotonic seq; aggregator validates on start, clears if invalid; tests cover the corrupt-restart path |
| Theme tokens missing → unreadable popup | wrong colours | L | `src/mon/theme.rs` falls back to a hard-coded "ink" palette if tokens missing; doctor warns; integration test asserts fallback paths render |
| `prometheus-parse` crate is small but lightly maintained | parser bug | L | vendor a parser fork if upstream goes silent (~200 LoC); regex-PEG approach is well-trodden |
| `sy mon --json` over IPC race with aggregator restart | exit 3 false positive | L | client retries with 100 ms backoff up to 1 s before exit 3; documented |
| New `mon-exporter` feature defaults change CI build time | slower CI | L | per-feature CI job already exists for `bar-iced`; reuse |

## 7. Open Questions

1. **Waybar tile**: do we want the optional `sy mon --waybar` tile
   shipping in `configs/waybar/config.jsonc` by default, or behind
   an opt-in profile? (Recommendation: default-on; one extra tile,
   green/yellow/red dot only.)
2. **Theme tokens**: should `sy mon` get its own palette entry
   (`mon.*`) in `configs/sy/themes/`, or strictly reuse `ui.*` +
   `colors.*`? (Recommendation: strict reuse for now; revisit only
   if a panel needs a colour that nothing else does.)
3. **`sy mon snapshot --svg <path>`** static export via `plotters`
   for incident reports — listed as a separate item under Scope but
   could be argued as gold-plating. Keep or drop? (Recommendation:
   drop from this spec, re-propose under `sy report` if and when
   needed; not enough evidence of demand.)
4. **MCP tool surface granularity**: do we add `system.mon.subscribe`
   as a streaming MCP tool, or keep streaming sy-ipc-only and have
   agents poll `system.mon.snapshot`? (Recommendation: poll-only for
   MCP; streaming is sy-ipc-local; revisit when an MCP client
   actually asks.)
5. **Cross-plane correlation IDs**: should we propagate the
   `traceparent` from architecture-refactor K6 into the aggregator's
   per-tick span so a workload latency spike can be tied to a
   knowledge query? (Recommendation: yes, but punt the rendering of
   that correlation to a follow-up spec.)

## 8. Hand-off

- **Journey**: `/journey` against this spec → `specs/journeys/
  JOURNEY-sy-mon-<dt>.md`. Use the actor / phases from §5 as the
  outline.
- **Roadmap**: `/roadmap` against the journey → `specs/roadmaps/
  sy-mon/ROADMAP.md`. Order suggestion:
  (1) sensors crate + tests,
  (2) `mon-exporter` feature on aiplane (smallest blast radius),
  (3) `sy mon collect` + ring buffer + IPC,
  (4) sy-ipc reservations + MCP tool stubs,
  (5) iced widgets (`Sparkline`, `AreaChart`, `Gauge`, `Heatmap`),
  (6) panel views (host first, then accel, then planes),
  (7) layer-shell wiring + PID-file toggle,
  (8) niri keybind + systemd unit + selinux fc,
  (9) `mon-exporter` rollout to remaining planes,
  (10) doctor checks + waybar tile + docs.
- **Implement**: `/implement` one roadmap step at a time, micro-TDD.
- **No new Workload impl** — `sy mon` consumes aiplane metrics; it
  does not load an ONNX model. So `/workload` is not invoked.
- **No new NPU model** — so `/npu-prep` is not invoked.
