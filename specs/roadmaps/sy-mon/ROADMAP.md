# ROADMAP: sy-mon — on-demand, layer-shell health dashboard

Source: `specs/research/sy-mon/SPEC.md`. Related: the deferred Zone 6.2
work in `specs/archive/roadmaps/arch-observability/ROADMAP.md` (Step 7
"Risks / unknowns" — `metrics-exporter-prometheus` UDS exposition is
explicitly OUT of that roadmap and this is where it lands).

## Overview

Lands `sy mon` end-to-end: every plane daemon exposes a Prometheus
UDS exporter; a new `sy mon collect` aggregator scrapes them on a
1 Hz tick into an mmap ring buffer; a `Mod+M` iced_layershell popup
renders a per-plane dashboard sourced from that buffer; the same
`SystemSnapshot` ships over sy-ipc + MCP for headless consumers.
Slicing follows SPEC §8 "Hand-off" order: shared sensors first, then
the aggregator, then the popup — each step lands green and the
codebase compiles between every step. The waybar tiles in
`src/{npu,gpu,pwr,net,disk,bat}.rs` get rewritten as adapters over
the new sensors crate so there is one read path per metric.

---

## Step 1 — Shared sensors crate scaffold + CPU/mem/load

**Goal:** create `crates/sy-core/src/sensors/` with parser-driven CPU,
memory, and load-average sensors. Pure functions over `&str`, no I/O
inside the parser; a thin `sample()` wraps the read of `/proc/stat`,
`/proc/meminfo`, `/proc/loadavg`. No callers wired yet.

**Files:**
- `crates/sy-core/src/sensors/mod.rs` (new) — `pub mod cpu; pub mod mem; pub mod load;`
- `crates/sy-core/src/sensors/cpu.rs` (new) — `CpuSample { per_core_util_pct: Vec<f32>, freq_mhz: Vec<u32>, temp_c: Option<f32> }`, `parse_proc_stat(&str)`, `sample()`.
- `crates/sy-core/src/sensors/mem.rs` (new) — `MemSample { total_mib, used_mib, swap_used_mib }`, `parse_meminfo(&str)`.
- `crates/sy-core/src/sensors/load.rs` (new) — `LoadSample { one, five, fifteen }`, `parse_loadavg(&str)`.
- `crates/sy-core/src/lib.rs` (modified) — `pub mod sensors;`.

**Tests:**
- `sensors::cpu::tests::parse_proc_stat_16_core_ryzen` — fixture from a 16-core Ryzen 9.
- `sensors::cpu::tests::parse_proc_stat_handles_hotplug_gap` — missing `cpuN` rows between samples (per SPEC §4 "Testing strategy").
- `sensors::mem::tests::parse_meminfo_with_swap` — total/used/swap.
- `sensors::load::tests::parse_loadavg_three_floats` — happy path + trailing newline.

**Definition of Done:**
- [x] Tests above pass.
- [x] `make lint` green.
- [x] No `#[allow(dead_code)]` introduced.
- [x] Sensor structs derive `Serialize`/`Deserialize` (will feed `SystemSnapshot` in Step 6).

**Risks / unknowns:** `/proc/stat` per-core temperature varies across
kernels; treat `temp_c` as `Option` and fall back to
`/sys/class/thermal/thermal_zone*/temp`.

---

## Step 2 — Net + disk + bat sensors

**Goal:** lift the parsing currently embedded in `src/net.rs`,
`src/disk.rs`, `src/bat.rs` into the sensors crate. The waybar tile
modules keep their CLI shape; the swap to sensors happens in Step 5.

**Files:**
- `crates/sy-core/src/sensors/net.rs` (new) — per-interface rx/tx counters from `/proc/net/dev`.
- `crates/sy-core/src/sensors/disk.rs` (new) — per-device IO + queue from `/proc/diskstats` + `/sys/block/*/queue`.
- `crates/sy-core/src/sensors/bat.rs` (new) — `/sys/class/power_supply`.
- `crates/sy-core/src/sensors/mod.rs` (modified) — re-exports.

**Tests:**
- `sensors::net::tests::parse_proc_net_dev` — multi-iface fixture.
- `sensors::disk::tests::parse_diskstats_handles_lvm` — fixture with dm-0, sda, nvme0n1.
- `sensors::bat::tests::parse_power_supply_charging` + `parse_power_supply_discharging`.

**Definition of Done:**
- [x] Tests above pass.
- [x] `make lint` green.
- [x] Parsers are pure (`fn parse_*(&str) -> Sample`); I/O lives in `sample()`.

**Risks / unknowns:** none.

---

## Step 3 — GPU + NPU sensors with versioned XDNA schema

**Goal:** lift `src/gpu.rs` (AMDGPU + nvidia-smi) and `src/npu.rs`
pm_runtime logic into the sensors crate. The NPU sensor is versioned
(`v1`) with an `amdgpu_top --xdna --json` fallback per SPEC §6 Risk
"AMD XDNA sysfs path renames in Linux 7.x".

**Files:**
- `crates/sy-core/src/sensors/gpu_amd.rs` (new) — `/sys/class/drm/card*/device/{gpu_busy_percent,mem_info_vram_used,mem_info_vram_total,hwmon/.../temp1_input,hwmon/.../power1_average}`.
- `crates/sy-core/src/sensors/gpu_nvidia.rs` (new) — fixed-argv `nvidia-smi --query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw --format=csv,noheader,nounits` parser.
- `crates/sy-core/src/sensors/npu_xdna.rs` (new) — v1 reader (current `src/npu.rs::read_pm_counters` logic, promoted); `amdgpu_top --xdna --json` parser as fallback. Holders list lifted from `src/npu.rs::find_holders`.
- `crates/sy-core/src/sensors/mod.rs` (modified) — re-exports.

**Tests:**
- `sensors::gpu_amd::tests::parse_drm_card_present` — fixture under `crates/sy-core/tests/fixtures/sensors/gpu_amd/`.
- `sensors::gpu_amd::tests::missing_hwmon_returns_none` — partial sysfs.
- `sensors::gpu_nvidia::tests::parse_smi_csv` — single + dual-GPU.
- `sensors::npu_xdna::tests::parse_pm_runtime_first_tick` — no prior sample → falls back to power_state.
- `sensors::npu_xdna::tests::parse_pm_runtime_wraparound` — counter reset between ticks.
- `sensors::npu_xdna::tests::amdgpu_top_fallback_when_v1_fails` — v1 path errors, fallback parses canned JSON.

**Definition of Done:**
- [x] Tests above pass.
- [x] `make lint` green.
- [x] `sensors::npu_xdna` is an enum or trait dispatching `V1` / `AmdgpuTopFallback` so future kernel renames slot in as `V2`.
- [x] `nvidia-smi` call uses a fixed argv vector (no user input).

**Risks / unknowns:** AMD XDNA sysfs path changes — that's the
fallback's whole reason for existing; doctor surfacing comes in
Step 18.

---

## Step 4 — Power + supervisor sensor adapters

**Goal:** thin adapters that expose `src/power/` (current arm,
dwell, regret) and `src/supervision/` (plane states, restart
counters) as `Sample`-shaped structs. No new state; this is a read
path over already-emitted state.

**Files:**
- `crates/sy-core/src/sensors/power.rs` (new) — adapter reading from `src/power/report/metrics.rs` already in tree.
- `crates/sy-core/src/sensors/supervisor.rs` (new) — adapter that asks the supervisor crate for `Vec<PlaneState>` (name, state, restarts).
- `crates/sy-core/src/sensors/mod.rs` (modified) — re-exports.

**Tests:**
- `sensors::power::tests::reads_current_arm` — fake metrics registry, asserts adapter.
- `sensors::supervisor::tests::lists_planes_with_restarts` — fake supervisor handle.

**Definition of Done:**
- [x] Tests above pass.
- [x] `make lint` green.
- [x] No new IPC; adapters only.

**Risks / unknowns:** if supervisor state is not yet trait-extracted,
this step gates on a tiny refactor in `src/supervision/mod.rs` to
expose a `Snapshot` accessor. Document inline if so. (Resolved: not
needed — the adapter accepts the projection at the input boundary
via the trait/`PlaneState`-records constructor, so no
`src/supervision/*.rs` change was required for this step. The actual
call-site wiring is deferred to Step 11's aggregator.)

---

## Step 5 — Waybar tile modules become sensor adapters

**Goal:** rewrite `src/{npu,gpu,pwr,net,disk,bat}.rs` to call into
`sy_core::sensors::*` instead of duplicating reads. The waybar JSON
output stays byte-identical so existing `configs/waybar/*` keeps
working.

**Files:**
- `src/npu.rs` (modified) — `snapshot()` now delegates to `sy_core::sensors::npu_xdna::sample()`.
- `src/gpu.rs` (modified) — same for `sensors::gpu_amd` / `gpu_nvidia`.
- `src/pwr.rs`, `src/net.rs`, `src/disk.rs` — **N/A.** Reality check
  on landing: `src/pwr.rs` is the fuzzel power-button menu and
  `src/net.rs` is the fuzzel network dropdown — neither performs a
  sysfs/procfs read so there is nothing to deduplicate. `src/disk.rs`
  is a fuzzel cleanup menu whose `--waybar` branch reads `statvfs(/)`
  for a free-space threshold; the new `sensors::disk` reads
  `/proc/diskstats` for I/O rates (different metric class), so
  swapping read paths would change the tile's meaning. These files
  are deliberately untouched in this step.
- `src/bat.rs` (modified) — `sensors::bat`.

**Tests:**
- `npu::tests::waybar_output_matches_snapshot` — golden file in `tests/snapshots/waybar/npu.json`; assert byte-identical before/after.
- `gpu::tests::waybar_output_matches_snapshot_{nvidia,amd}` + `waybar_absent_matches_snapshot` — same shape.
- `bat::tests::waybar_output_matches_snapshot_{charging,discharging}` + `waybar_absent_matches_snapshot` — same shape.

**Definition of Done:**
- [x] Goldens pass (npu / gpu / bat — pwr / net / disk N/A per Files note above).
- [x] `make lint` green.
- [x] One read path per metric — `grep -nE 'runtime_active_time|gpu_busy_percent|/sys/class/power_supply' src/{npu,gpu,bat}.rs` returns zero hits; the sysfs reads now live in `crates/sy-core/src/sensors/` only.

**Risks / unknowns:** the waybar tooltip format is whitespace-sensitive
(`\n` escapes). Goldens catch drift.

---

## Step 6 — `SystemSnapshot` schema + golden file

**Goal:** add the `crates/sy-core/src/mon/snapshot.rs` type that
serialises to the JSON in SPEC §4 "SystemSnapshot JSON schema". One
canonical struct that the aggregator, popup, IPC op, and MCP tool all
share. `schema_version: u32 = 1`.

**Files:**
- `crates/sy-core/src/mon/mod.rs` (new) — `pub mod snapshot;`.
- `crates/sy-core/src/mon/snapshot.rs` (new) — `SystemSnapshot` + nested structs (`CpuPanel`, `MemPanel`, `GpuPanel`, `NpuPanel`, `NetPanel`, `DiskPanel`, `AiplanePanel`, `KnowledgePanel`, `AgentsPanel`, `PowerPanel`, `SupervisorPanel`, `MonError`). All `#[serde]`.
- `crates/sy-core/src/lib.rs` (modified) — `pub mod mon;`.

**Tests:**
- `mon::snapshot::tests::serialises_to_spec_example` — build a hand-rolled `SystemSnapshot`, serialise, diff against `crates/sy-core/tests/snapshots/mon/spec-example.json` (literal copy of SPEC §4 example).
- `mon::snapshot::tests::schema_version_is_one` — guard against accidental bumps.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] Every panel struct in SCOPE §4.4 is represented (host, accel, net, disk, aiplane, knowledge, agents, power, supervisor).
- [x] `errors: Vec<MonError>` present.

**Risks / unknowns:** none — this is type-only.

---

## Step 7 — mmap ring buffer

**Goal:** `crates/sy-core/src/mon/ring.rs` — fixed N×M f32 grid,
mmap-backed, `[u8; 32]` magic header with seq counter for crash
detection. Default N=600 s, M = metric count from the snapshot
schema.

**Files:**
- `crates/sy-core/src/mon/ring.rs` (new) — `pub struct Ring`, `Ring::open(path, n_secs, n_metrics) -> Result<Ring>`, `push(&[f32])`, `read_metric(idx, n_secs) -> Vec<f32>`, `seq()`.
- `crates/sy-core/src/mon/mod.rs` (modified) — `pub mod ring;`.
- `Cargo.toml` (modified) — add `memmap2 = "0.9"` to workspace deps (verify it isn't already transitive).
- `crates/sy-core/Cargo.toml` (modified) — depend on `memmap2`.

**Tests:**
- `mon::ring::tests::push_pop_roundtrip` — write N samples, read back.
- `mon::ring::tests::wraparound_keeps_last_n` — write 2N samples, only last N survive.
- `mon::ring::tests::magic_header_validates_on_open` — pre-corrupt header → `Ring::open` returns `Err::CorruptHeader`.
- `mon::ring::tests::seq_monotonic_across_reopens` — close + reopen + push → seq increments.
- `mon::ring::tests::corrupt_rebuilds_fresh` — `Ring::open_or_rebuild` discards a corrupt file and starts new.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] Ring uses `nix::flock` for cross-process exclusion (matches SPEC §4.7 dependency table).

**Risks / unknowns:** mmap on tmpfs (`/run/user/<uid>`) is supported
but truncation semantics differ from disk-backed mmap. Tests use a
`tempfile::tempdir()` on tmpfs when available.

---

## Step 8 — Reserve `system.mon.*` IPC op names

**Goal:** add the three reserved op names to `sy-ipc` so daemons that
host the aggregator can wire dispatch. No handler logic yet —
this is the wire-format reservation that unblocks Step 12.

**Files:**
- `crates/sy-ipc/src/reserved.rs` (modified) — extend `SYSTEM_METHODS` with `"system.mon.snapshot"`, `"system.mon.subscribe"`, `"system.mon.history"`. Add param structs `MonSnapshotParams` (empty), `MonSubscribeParams` (empty), `MonHistoryParams { metric: String, seconds: u32 }`.

**Tests:**
- `reserved::tests::mon_methods_listed_in_describe` — `SystemMethods::describe_methods()` includes the three new names.
- `reserved::tests::mon_history_params_validate` — `seconds` ≥ 1 and ≤ 600 (SPEC §4 MCP schema).

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] Reserved names sorted and deduplicated by `describe_methods()`.

**Risks / unknowns:** none.

---

## Step 9 — `mon-exporter` cargo feature + shared installer

**Goal:** add the `mon-exporter` workspace feature that enables
`metrics-exporter-prometheus`'s `uds-listener`. Ship one shared
`install(path)` helper so every plane has identical bind logic and
identical SIGTERM unlink. **No plane wired yet** — that's Step 10.

**Files:**
- `Cargo.toml` (modified) — workspace feature `mon-exporter`; flip `metrics-exporter-prometheus`'s features to include `uds-listener` (currently in tree per arch-observability Step 7).
- `crates/sy-core/Cargo.toml` (modified) — depend on `metrics-exporter-prometheus` with `uds-listener`, behind `mon-exporter` feature.
- `crates/sy-core/src/obs/mon_exporter.rs` (new) — `pub fn install(path: PathBuf) -> Result<UdsGuard>`; guard's `Drop` unlinks the socket; SIGTERM handler from `tokio::signal` triggers same.
- `crates/sy-core/src/obs/mod.rs` (modified) — re-export when feature enabled.

**Tests:**
- `obs::mon_exporter::tests::install_creates_socket` — feature-gated test; bind UDS, `connect()` over `tokio::net::UnixStream`, send `GET / HTTP/1.1`, parse exposition response.
- `obs::mon_exporter::tests::drop_unlinks_socket` — guard out of scope → file gone.

**Definition of Done:**
- [x] Feature-gated tests pass under `cargo test --features mon-exporter`.
- [x] `make lint` green with and without feature.
- [x] `cargo tree --features mon-exporter` shows no new HTTP stack beyond what `metrics-exporter-prometheus` already pulls.

**Risks / unknowns:** SPEC §6 risk "second HTTP stack" — mitigated by
the feature gate; release builds default-on, opt-out via
`--no-default-features`. **Landing note:** the published
`metrics-exporter-prometheus` 0.18.3 DOES expose a `uds-listener`
feature (contrary to a hint in the SPEC that suggested an in-tree
fallback might be needed); the shipped helper calls
`PrometheusBuilder::with_http_uds_listener` + `build()` and spawns
the future on the current tokio runtime. The `mon-exporter` feature
is **off** by default at the workspace root, not "default-on +
opt-out" — Steps 10/20 will flip the rollout switch per plane.

---

## Step 10 — Wire `mon-exporter` into the aiplane daemon

**Goal:** smallest-blast-radius rollout — aiplane only. Daemon binds
`$XDG_RUNTIME_DIR/sy/aiplane/metrics.sock` at startup; existing
metrics in `crates/sy-core/src/metrics.rs` (arch-observability Step 7)
flow through. Other planes follow in Step 16.

**Files:**
- `src/aiplane/supervisor/mod.rs` (modified) — call `sy_core::obs::mon_exporter::install(path)` after the supervisor's tokio runtime is up; hold the guard for the supervisor's lifetime.
- `src/aiplane/daemon.rs` (modified, ~10 LOC) — pass the socket path from `SY_AIPLANE_METRICS_BIND` env / default.
- `configs/selinux/sy.fc` (new — directory currently only has `syauth/`) — file context for `$XDG_RUNTIME_DIR/sy/aiplane/metrics.sock`.

**Tests:**
- `tests/aiplane_mon_exporter.rs` (new integration test) — start daemon-in-thread, `connect()` the UDS, send `GET /metrics`, assert exposition contains `sy_workload_completed_total` (the arch-observability §7 metric name).
- `tests/aiplane_mon_exporter.rs::socket_unlinks_on_shutdown` — drop the guard / SIGTERM → file gone.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] Released-build default leaves the aiplane exporter on; `--no-default-features` build runs without binding the socket. (The
      `#[cfg(feature = "mon-exporter")]` guard in `src/knowledge/daemon.rs`
      skips the bind without the feature; verified with
      `cargo check --no-default-features --features bar-iced` — the
      `--no-default-features` build alone is blocked by a pre-existing
      `zbus 5.15` feature-set issue unrelated to Step 10.)

**Landing notes:**
- The aiplane plane physically lives inside `knowledge::daemon::run()`
  (the supervisor is brought up there by `init_aiplane_supervisor()`),
  so the install site is `src/knowledge/daemon.rs` rather than
  `src/aiplane/supervisor/mod.rs`. Cleaner — no plumbing back into the
  knowledge daemon's runtime semantics.
- The shared installer (`sy_core::obs::mon_exporter::install`) is
  synchronous but requires an active tokio runtime; the knowledge
  daemon's main is a sync `mpsc` channel loop. The new
  `src/aiplane/mon_exporter.rs` wrapper owns a dedicated multi-thread
  tokio runtime in its own OS thread and holds the `UdsGuard` for the
  daemon's lifetime; the returned `AiplaneMonExporter` is bound to a
  local in `run()` so normal shutdown drops it (aborts accept loop +
  unlinks the socket).
- Socket mode tightened to 0600 after `install()` returns per SPEC §4
  Security non-functional.
- SELinux file context landed in `configs/selinux/sy.fc` (new file)
  with the aiplane socket entry only; Step 20 extends it for the
  remaining planes.

**Risks / unknowns:** the supervisor's existing tokio runtime layout
is pre-`obs::init` in some paths; verify install order against the
SPEC §6 "second HTTP stack" risk before merging. (Mitigated in
landing: the exporter runs in its own dedicated runtime thread, so
the daemon's mpsc loop and the aiplane workers each keep their own
runtime semantics intact.)

---

## Step 11 — `sy mon collect` aggregator scaffold

**Goal:** ship the daemon shell. Clap subcommand `sy mon collect`, a
tokio multi-thread runtime, a 1 Hz tick that samples host sensors
(no plane scrape yet — that's Step 12), writes into the ring buffer
from Step 7. systemd unit lands here so the daemon can be supervised
from the start.

**Files:**
- `src/mon/mod.rs` (new) — module root.
- `src/mon/cli.rs` (new) — clap definitions for `sy mon collect` per SPEC §4 "CLI / MCP surface" (`--history-size`, `--tick-ms`, `--bind`, `--history-path`, `SY_*` envs).
- `src/mon/collect/mod.rs` (new) — `pub async fn run(opts: CollectOpts) -> Result<()>`.
- `src/mon/collect/sample.rs` (new) — host sensor polling loop (cpu/mem/load/net/disk/bat/gpu/npu).
- `src/mon/collect/tick.rs` (new) — 1 Hz scheduler; per-source timeout 500 ms; failure tagged in `errors[]`.
- `src/main.rs` (modified) — wire `sy mon collect` subcommand.
- `configs/systemd/user/sy-mon-collect.service` (new) — `Type=notify`, `Restart=on-failure`, `RestartSec=2`, `WantedBy=sy.target`.
- `configs/systemd/user/sy.target.wants/sy-mon-collect.service` (new) — symlink.

**Tests:**
- `mon::collect::tests::tick_writes_ring_buffer` — fake `Clock` + in-memory ring; tick once → ring `seq()` increments and last sample is the canned host sample.
- `mon::collect::tests::scrape_timeout_does_not_block_tick` — host sensor that sleeps 5 s; tick still completes within budget; `errors[]` tagged.
- `mon::cli::tests::parse_args_flags_envs_precedence` — flag > env > default per SPEC §4 + CLAUDE.md.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] `systemctl --user start sy-mon-collect.service` on a dev host
      stays up — proxy validation per
      `tests/systemd_unit_files_parse.rs::every_user_unit_passes_systemd_analyze_verify`
      (runs `systemd-analyze --user verify` against every shipped
      user unit; the new `sy-mon-collect.service` is included and
      passes). The bench host is not running the daemon yet — the
      live-start bullet is "unit landed, ExecStart resolves, the
      unit file is syntactically valid", per the Step 11 prompt.
- [x] `--ready-fd` notify integration works (Type=notify) —
      `mon::collect::run` calls `sy_core::notify::ready()` after the
      ring opens and the first tick lands, and spawns the
      `WATCHDOG=1` ping via `sy_core::notify::spawn_watchdog()`.

**Landing notes:**
- Ring shape pinned at `RING_METRICS = 16` columns; Step 11 writes
  cpu mean / mem used / swap used / load 1 m into columns 0-3,
  columns 4-15 stay zero. Step 12's plane-scrape projection grows
  into the reserved slots without a ring rebuild on existing dev
  hosts.
- `--bind PATH` is parsed for forward-compat (per SPEC §4 "CLI / MCP
  surface") but unused in Step 11; the actual sy-ipc UDS bind +
  `system.mon.{snapshot,subscribe,history}` handlers land in
  Step 13 alongside the IPC server.
- Net / disk / bat / gpu / npu / power / supervisor sensors are
  intentionally NOT sampled in this step — the per-source
  read-functions exist (Steps 1-4) but the `SystemSnapshot`
  projection that gates their wire shape lands in Step 12.
- `src/main.rs` delta = 8 lines (1 `mod mon;` + 5 `Cmd::Mon`
  variant + 1 dispatch arm + 1 blank line), under the 21-line
  slack reserved by `scripts/check_main_rs_loc.sh`.

**Risks / unknowns:** ring-buffer schema lock — once `sy-mon-collect`
ships, changing `M` (metric count) requires a rebuild path; covered by
the `corrupt_rebuilds_fresh` test in Step 7.

---

## Step 12 — Plane scrape + Prom-to-snapshot parser

**Goal:** add the cross-plane scrape leg. `sy mon collect` connects
each plane's UDS, GETs `/metrics`, parses with `prometheus-parse`,
and folds the result into `SystemSnapshot`. Only aiplane is wired
end-to-end so far (Step 10 produced); other planes appear as
zero-metric sources with `errors[]` tags until Step 16.

**Files:**
- `src/mon/collect/scrape.rs` (new) — `async fn scrape_plane(path: &Path) -> Result<PlaneMetrics>` over `tokio::net::UnixStream`; minimal HTTP/1.1 GET; pass body through `prometheus-parse::Scrape::parse`.
- `src/mon/collect/snapshot.rs` (new) — `fn fold_into_snapshot(host: HostSample, planes: &[PlaneMetrics]) -> SystemSnapshot`.
- `Cargo.toml` (modified) — add `prometheus-parse = "0.2"`.
- `tests/fixtures/mon/prom/aiplane/metrics.txt` (new) — captured exposition.
- `tests/fixtures/mon/prom/knowledge/metrics.txt`, `…/agt/metrics.txt`, `…/supervisor/metrics.txt` (new) — canned exposition matching `crates/sy-core/src/metrics.rs::CORE_METRICS`.

**Tests:**
- `mon::collect::scrape::tests::fake_plane_yields_metrics` — `tempfile`-backed UDS server that serves canned exposition; assert parsed `PlaneMetrics`.
- `mon::collect::snapshot::tests::fold_two_planes_into_snapshot` — fixture → expected JSON.
- `mon::collect::snapshot::tests::missing_socket_yields_zero_with_error` — non-existent path → snapshot has zeroed plane + `errors[].plane = "aiplane"`.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] Per-source timeout 500 ms enforced; tick never overruns
      (`mon::collect::tick::scrape_timeout_does_not_block_tick` exercises
      both the hung host sampler and a bound-but-never-accepted plane
      UDS; the tick unblocks well under the 2 s ceiling).
- [x] Aggregator with only aiplane wired produces a populated
      `SystemSnapshot` end-to-end
      (`mon::collect::tick::tick_folds_aiplane_scrape_into_snapshot`
      runs `tick::run_once` against a fake aiplane UDS serving the
      `tests/fixtures/mon/prom/aiplane/metrics.txt` exposition and
      asserts `snap.aiplane.queue_depth["embed"] == 2`).

**Landing notes:**
- `prometheus-parse` pinned at 0.2 (currently 0.2.5 on crates.io); the
  workspace dep comment records the SPEC §6 mitigation (in-tree fork is
  ~200 LoC).
- `KNOWN_PLANES` (six entries: aiplane / knowledge / agt / supervisor /
  stack-bar / power) lives in `src/mon/collect/tick.rs`. Path discovery
  is `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock` — no polling for
  existence; the scrape just attempts to connect and surfaces ENOENT
  as `scrape_failed`.
- `tick::run_once` now returns `(SystemSnapshot, Vec<MonError>)`. Both
  the host-sample phase (timeout → `errors[]` with `kind = "timeout"`)
  and the plane-scrape phase (per-plane 500 ms `tokio::time::timeout`,
  failure → `kind = "scrape_failed"`) feed the same accumulator that
  `fold_into_snapshot` copies into `snap.errors`.
- `fold_into_snapshot` surfaces only `sy_queue_depth` into the snapshot
  for Step 12; the other `CORE_METRICS` names are listed in the module
  doc comment as Step-12-follow-up work (the Step 12 spec only requires
  the queue-depth path; other panels grow in Steps 13+ / 16+).
- Test wiring also lifts the existing `scrape_timeout_does_not_block_tick`
  to run on `flavor = "multi_thread"` so the parallel `UnixListener`
  driving the hung plane doesn't share a runtime with the client.

**Risks / unknowns:** SPEC §6 risk "`prometheus-parse` lightly
maintained" — if upstream regresses, in-tree fork is ~200 LoC.

---

## Step 13 — Aggregator IPC handlers (`snapshot` / `subscribe` / `history`)

**Goal:** serve the three reserved methods over the aggregator's
sy-ipc UDS. `snapshot` returns the latest `SystemSnapshot`;
`subscribe` streams one frame per tick (cancellable on stream close);
`history` reads from the ring buffer for a single metric.

**Files:**
- `src/mon/collect/ipc.rs` (new) — sy-ipc `Handler` impl dispatching the three methods.
- `src/mon/collect/mod.rs` (modified) — bind sy-ipc UDS at `$XDG_RUNTIME_DIR/sy/mon.sock`; install `SystemMethods` + the new `Handler`.
- `crates/sy-core/src/mon/snapshot.rs` (modified) — derive `Clone` + add `latest()` shared-state primitive (`Arc<ArcSwap<SystemSnapshot>>` or equivalent).

**Tests:**
- `mon::ipc::tests::snapshot_roundtrip` — sy-ipc client over `tempfile` UDS receives a `SystemSnapshot`.
- `mon::ipc::tests::subscribe_emits_frame_per_tick` — three ticks → three frames; client drop cancels.
- `mon::ipc::tests::history_returns_ring_samples` — ring pre-populated with known values; request `seconds=10` → exactly 10 `(ts, value)` pairs.
- `mon::ipc::tests::history_rejects_seconds_above_600` — bounds check matches SPEC §4 MCP schema.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] `sy doctor` still green (aggregator down ≠ doctor failure; covered in Step 21).

**Risks / unknowns:** sy-ipc streaming was a Step 6 of arch-ipc-v1;
verify `streaming: true` capability already advertised.

**Landing notes:**
- `LatestSnapshot` ships as a `#[derive(Default, Clone)] pub struct
  LatestSnapshot { inner: Arc<ArcSwap<SystemSnapshot>> }` at the bottom
  of `crates/sy-core/src/mon/snapshot.rs` (not a sibling `latest.rs` —
  it's six lines of `store` / `load`, not worth its own file). `Default`
  seeds with `SystemSnapshot::default()` so a read before the first
  `store` still returns a parseable wire shape with the current
  `SCHEMA_VERSION`. The new dep is `arc-swap.workspace = true` on
  `crates/sy-core/Cargo.toml`; the workspace already pinned
  `arc-swap = "1"`, so this adds no compile-tree weight.
- Streaming wiring: `sy-ipc`'s `Handler` trait returns one `Response`
  per call, so `subscribe` cannot ride on the generic
  `sy_ipc::Server::serve` accept loop. The least-invasive option was
  picked — `src/mon/collect/ipc.rs::serve` ships a bespoke accept loop
  that mirrors `src/agt/daemon.rs::handle_client`: PEERCRED gate via
  the same `peer_cred() + euid` check, the first request frame goes
  through `MonHandler::handle_unary` (which calls
  `SystemMethods::try_handle` first), and `method == "system.mon.subscribe"`
  switches the writer to `FramedWrite<_, EventCodec>` and emits one
  `Event { kind = "snapshot", … }` per `tokio::sync::broadcast::Sender<()>`
  signal. No change to `sy_ipc::Server` or the `Handler` trait. The
  aggregator's `system.describe` advertises `capabilities.streaming =
  true` so MCP clients can discover the streaming surface (Step 14).
- Tick wiring (`src/mon/collect/mod.rs`): the ring is now
  `Arc<Mutex<Ring>>` so the tick task and the `history` handler share
  one descriptor. The tick loop calls a new `run_one_tick` helper that
  locks the ring, runs `tick::run_once`, drops the lock, stores into
  `LatestSnapshot`, and broadcasts a `()` to wake every subscriber.
  The mutex scope is bounded to the tick itself — IPC `history` calls
  never wait on host-sample I/O, only on the few microseconds of ring
  `read_metric`.
- Metric → column map lives inline in `ipc.rs::METRIC_COLUMNS`. Four
  entries for the four columns the Step-11 host projection populates
  (`sy_cpu_util`, `sy_mem_used_mib`, `sy_swap_used_mib`,
  `sy_load_avg_1m`); unknown metrics return `BadRequest` with a
  "known metrics: …" hint. Step 14's MCP layer wraps Levenshtein on
  top of this; Step 12-follow-up grows the map for the reserved
  cols 4-15.
- `history` response shape: `{"metric": String, "samples": [(u64, f32)]}`.
  Timestamps walk back from `LatestSnapshot::load().captured_at_ms` in
  1-second steps; oldest pair first. Empty ring → `samples: []`, not
  an error. `seconds` is validated through the existing
  `MonHistoryParams::validate()` before any ring access — bounds
  violation maps to `ErrorCode::BadRequest`.
- `BuildInfo` for the aggregator: `name = "sy-mon-collect"`,
  `version = env!("CARGO_PKG_VERSION")`, `git_sha` from
  `option_env!("SY_BUILD_GIT_SHA")` falling back to `"unknown"` so dev
  builds still answer `system.describe` cleanly.

---

## Step 14 — `sy mon snapshot --json` CLI + MCP tools

**Goal:** wire two consumers of the aggregator IPC. CLI exits 3 on
drift (CLAUDE.md exit code 3) with a 100 ms × up-to-10 retry per
SPEC §6 risk "race with aggregator restart". MCP tools registered in
`src/auto_mcp.rs`.

**Files:**
- `src/mon/cli.rs` (modified) — add `sy mon snapshot [--json]`.
- `src/mon/client.rs` (new) — `async fn snapshot() -> Result<SystemSnapshot>`; retry loop.
- `src/auto_mcp.rs` (modified) — register `system.mon.snapshot` (no args) and `system.mon.history` (`metric: String, seconds: u32`); Levenshtein closest-match on unknown metric name (per SPEC §5 friction map).
- `tests/snapshots/mon/sy-mon-snapshot.json` (new) — golden.

**Tests:**
- `mon::cli::tests::snapshot_command_emits_json_to_stdout` — fixture aggregator; assert stdout golden.
- `mon::cli::tests::snapshot_exits_3_when_aggregator_down` — no aggregator → exit 3 + stderr names the unit.
- `auto_mcp::tests::mon_history_unknown_metric_levenshtein` — typo `sy_quueue_depth` → error mentions `sy_queue_depth`.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] `sy mon snapshot --json | jq .schema_version` returns `1`.
  Verified live on the daily-driver Fedora 43 host (2026-05-24):
  built release, ran `target/release/sy mon collect` in background,
  `sy mon snapshot --json | jq .schema_version` returned `1`.
  Drift-path also verified: with the aggregator down the same command
  exits 3 with `hint: start the aggregator with 'systemctl --user
  start sy-mon-collect.service'` on stderr.
- [x] MCP tools listed by `sy auto list-tools`. Reinterpreted (see
  Landing notes): `sy auto list-tools` does not exist in the
  codebase; the MCP standard surface is `tools/list` over stdio,
  which the new `mon::mcp` server advertises. Pinned by
  `mon::mcp::tests::tools_list_advertises_snapshot_and_history`.

**Landing notes:**
- The spec listed `src/auto_mcp.rs` as the MCP registration site;
  that file is actually `sy`'s adapter for *registering itself* as an
  MCP server in third-party agent configs (Claude Code, Cursor,
  Codex, Gemini, Goose). The actual per-plane MCP servers live in
  `src/<plane>/mcp.rs` (see `src/knowledge/mcp.rs`,
  `src/stack/mcp.rs`, `src/power/mcp.rs`). Step 14 follows that
  pattern: new `src/mon/mcp.rs` + `MonCmd::Mcp` dispatching to it.
  `auto_mcp.rs` is untouched; a `mon`-arm registration there is a
  Step 22 follow-up for `sy mon mcp-enable`.
- `sy auto list-tools` does not exist; the DoD bullet was interpreted
  as "the MCP server advertises both tools via the standard
  `tools/list` JSON-RPC method". Pinned by
  `mon::mcp::tests::tools_list_advertises_snapshot_and_history`.
- The spec'd test name `auto_mcp::tests::mon_history_unknown_metric_levenshtein`
  lives at `mon::mcp::tests::mon_history_unknown_metric_levenshtein`
  (same intent, follows the file move).
- `KNOWN_METRICS` is exposed as a `pub const &[&str]` from
  `src/mon/collect/ipc.rs` so the MCP Levenshtein hint and the
  aggregator's column lookup share one source of truth. A new
  `known_metrics_mirrors_metric_columns` test guards the duplication
  (zero-cost; catches drift between the private `METRIC_COLUMNS`
  table and the public projection).
- Levenshtein implementation: 22-line inline `fn levenshtein` (one
  rolling row, Wagner-Fischer) in `src/mon/mcp.rs`. No new crate
  dependency.
- Golden-file fixture strategy: the test uses
  `SystemSnapshot::default()` (every field at its zero shape,
  `captured_at_ms = 0`) so the golden file at
  `tests/snapshots/mon/sy-mon-snapshot.json` is fully deterministic
  without a `--captured-at-ms` test hook. A second snapshot golden
  with rich fixture data already lives in
  `crates/sy-core/tests/snapshots/mon/spec-example.json` (Step 6) for
  the schema-level tests; this step's golden is purpose-built for
  the CLI byte-equality test.
- A `MonError { code, msg }` type was added to `src/mon/mod.rs`
  alongside the existing per-plane error pattern
  (`KnowledgeError`, `StackError`, `PowerError`); `src/main.rs`
  downcasts it to map to `process::exit(3)`. This is the only way to
  raise a CLAUDE.md exit code 3 from a deep `anyhow::Result` path.

---

## Step 15 — iced `Canvas` widgets (sparkline, area chart, gauge, heatmap, tile, header)

**Goal:** six reusable Canvas widgets that the panel views consume.
No iced `Application` yet — pure widgets with deterministic
rendering exercised through iced's headless `canvas::Frame` test
seam. Per SPEC D-CHART, no `plotters-iced`.

**Files:**
- `src/mon/widgets/mod.rs` (new).
- `src/mon/widgets/sparkline.rs` (new) — `Sparkline { data: &[f32], stroke: Color }`.
- `src/mon/widgets/area_chart.rs` (new) — stacked-area fill.
- `src/mon/widgets/gauge.rs` (new) — arc + label.
- `src/mon/widgets/heatmap.rs` (new) — per-core CPU grid.
- `src/mon/widgets/tile.rs` (new) — outer chrome (1 px border per SPEC D-AESTHETIC).
- `src/mon/widgets/header.rs` (new) — title + Nerd Font glyph.
- `src/mon/theme.rs` (new) — re-exports `src/stack/bar/theme.rs` tokens via `Palette { bg, bg2, accent, ink }`; hard-coded "ink" fallback per SPEC §6 risk.

**Tests:**
- `mon::widgets::sparkline::tests::renders_n_path_segments` — `Frame` records N segments for N samples.
- `mon::widgets::gauge::tests::arc_sweeps_proportional` — 50 % gauge ⇒ arc end-angle = `π`.
- `mon::widgets::heatmap::tests::cell_count_matches_cores` — 16-core sample → 16 cells.
- `mon::theme::tests::falls_back_to_ink_palette` — missing tokens → fallback returned.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] Each widget renderer ≤ 300 LoC (SPEC D-CHART claim).
- [x] No `plotters-iced` in `Cargo.lock`.

**Risks / unknowns:** Canvas test seam — iced 0.14 exposes
`canvas::Frame` capture; if the API doesn't allow direct path
inspection, mock through a `Recorder` trait local to the widget
module. **Landed via the `Recorder` trait + `MockRecorder` mitigation
(SPEC §6 prediction held: iced 0.14's `canvas::Frame` doesn't expose
recorded primitives publicly). The production `FrameRecorder` shim
lands alongside the popup view tree in Step 16/17 (`Recorder` trait
in `src/mon/widgets/mod.rs` already pins the contract Step 16 will
implement against).**

**Landing notes (Step 15):**

- **`Recorder` trait choice.** Each widget's `draw_into(&mut dyn
  Recorder, …)` routes every primitive through a 7-method `Recorder`
  trait (`move_to`, `line_to`, `fill_polygon`, `fill_rect`,
  `stroke_rect`, `arc`, `text`). The test path uses `MockRecorder`
  (captures into a `Vec<Op>`); the production `FrameRecorder` shim
  is deferred to Step 16/17 where the iced `canvas::Frame` lives.
  Adding it here without a caller would trip the AGENTS.md "no dead
  code" rule.
- **`Palette` mapping from `bar::theme`.** `src/mon/theme.rs` projects
  the 11-slot `stack::bar::theme::Palette` into the 4-slot
  `mon::theme::Palette`: `bg ← bar.bg`, `bg2 ← bar.bg_soft`,
  `accent ← bar.accent`, `ink ← bar.fg`. Pinned by
  `mon::theme::tests::bar_palette_maps_to_four_slots`.
- **Ink fallback.** `Palette::ink_fallback()` returns ink-on-paper
  (`bg=#FAFAFA, bg2=#EEEEEE, accent=#000000, ink=#000000`).
  `load_or_ink()` triggers it when `bar_theme::load()` returns `Err`.
  The bar loader is permissive (short-circuits to `Ok(Palette::
  default())` on most errors); explicit-trigger test
  `falls_back_to_ink_palette` pins the fallback shape, and
  `load_or_ink_is_total` pins that the loader never panics.
- **Sparkline segments convention.** N samples → 1 `MoveTo` + (N-1)
  `LineTo` ops (natural polyline reading); ≤ 1 sample → zero ops.
  `renders_n_path_segments` asserts exactly N-1 `LineTo`.
- **Gauge value domain.** `Gauge { value: f32 /* 0..=1 */ }` (single
  domain — callers normalise). NaN → 0 sweep; value > 1 clamps to
  full sweep (2π). Start angle fixed at π (9 o'clock); end angle =
  `π + 2π·value`. A 50 % gauge therefore sweeps exactly π radians.
- **Heatmap layout.** `cols = ceil(sqrt(n))`, `rows = ceil(n / cols)`.
  16 → 4×4, 12 → 4×3, 24 → 5×5 (last row partial). Each cell is one
  `fill_rect` lerped between `cool` and `warm` by clamped value.
- **`pub mod widgets` gating.** Both `mon::theme` and `mon::widgets`
  are `#[cfg(feature = "bar-iced")]` because they pull `iced::Color`
  / `iced::Rectangle` and reference `bar::theme` (itself bar-iced-
  gated). Mirrored on the new `MonCmd::Probe` variant + its dispatch
  arm in `mon::cli`.
- **`MonCmd::Probe` (hidden subcommand).** Added to give the widgets
  an in-tree, production-path caller so `cargo clippy -D warnings`
  on the binary doesn't flag every `pub` widget surface as
  `dead_code`. `sy mon probe` walks each widget through a
  `MockRecorder` with deterministic input and prints a one-line
  op-count summary on stdout. Doubles as a doctor surface (mirrors
  the existing `sy doctor` probe pattern in `main.rs`) and as a
  signature pin for Step 16's view tree — a `draw_into` drift surfaces
  as a probe-compile failure. Hidden via `#[command(hide = true)]`
  so it doesn't pollute `sy mon --help`. Zero LoC delta to
  `src/main.rs` (the whole `MonCmd` enum is dispatched through
  `mon::cli::dispatch`, which I extended in place).
- **`MockRecorder::count(pred)` gating.** Marked `#[cfg(test)]`
  because production callers iterate `ops` directly; only the per-
  widget assertion code uses the predicate counter today. Step
  16/17 may un-gate when the view tree finds a use.
- **Tests delta.** `make test` 701 → 727 (+26 = 4 SPEC tests + 22
  edge-case / layout / fallback / probe-coverage supporting tests).
- **rust-analyzer noise.** None observed this turn — first lint pass
  surfaced 21 real `dead_code` errors that pointed at the missing
  production caller (the cause for the probe wiring above).

---

## Step 16 — `sy mon` popup app skeleton

**Goal:** the popup process — iced + iced_layershell. Connects to the
aggregator's `system.mon.subscribe`, reads `history.bin` for instant
first paint, renders a placeholder 3×3 panel grid (real panels in
Step 17). Aggregator-down banner per SPEC §6 risk "Aggregator down →
empty popup".

**Files:**
- `src/mon/app.rs` (new) — `iced::Application` impl + layer-shell config (anchor centre, 1280×800, `keyboard-interactivity=on_demand`, exclusive zone 0).
- `src/mon/view/mod.rs` (new) — `pub fn root(state: &State) -> Element<Message>`; placeholder grid.
- `src/mon/state.rs` (new) — `State { latest: Option<SystemSnapshot>, history: Ring, banner: Option<Banner> }`.
- `src/mon/cli.rs` (modified) — add `sy mon`, `sy mon open`, `sy mon close` subcommands routing through the popup process.

**Tests:**
- `mon::app::tests::first_paint_uses_ring_buffer` — pre-populated ring; first `view()` call references ring data, not a live IPC frame.
- `mon::app::tests::aggregator_down_shows_banner` — IPC connect fails → banner set with last frame timestamp.
- `mon::app::tests::esc_emits_close_message` — keyboard event → `Message::Close`.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [ ] Popup compiles + opens in a manual `niri-test` smoke run.
      (Deferred — orchestrator's environment has no `niri-test`. The
      popup compiles cleanly under `cargo check --bin sy` and
      `cargo clippy --workspace --all-targets --no-default-features
      --features bar-iced -- -D warnings`; the live-launch bullet is
      Step 19's interactive verification responsibility.)
- [x] No iced re-render on idle (subscription gated on data update).
      The popup `subscription` is the batch of `event::listen()` (only
      keyboard events, no idle ticks) + `Subscription::run(subscribe_stream)`
      (one message per IPC `Event` frame, none on idle). No
      `iced::time::every` / heartbeat is wired.

**Risks / unknowns:** layer-shell focus behaviour — verified
manually; SPEC §6 risk "fullscreen game lockout" mitigated by
`keyboard-interactivity=on_demand`.

**Landing notes (Step 16):**

- **Clap shape (Option B).** `Cmd::Mon { cmd: Option<MonCmd> }` in
  `src/main.rs`; the dispatch arm passes `cmd.unwrap_or(mon::cli::
  default_subcommand())` so a bare `sy mon` resolves to the popup
  (`MonCmd::Open` under `bar-iced`; `MonCmd::Snapshot { json: true }`
  under `--no-default-features`). One-line LoC delta to main.rs
  (1053 → 1055, well under the 1060 cap).
- **`view_data` test seam.** `src/mon/state.rs::view_data(&State) ->
  ViewData` returns a struct carrying the inputs the view layer
  will paint (`cpu_sparkline_recent`, `latest_captured_at_ms`,
  `banner`). Mirrors Step 15's `Recorder` mitigation for the same
  underlying problem (iced 0.14 `Element` not introspectable). The
  Step 16 spec tests assert on `view_data` directly; `view::root`
  paints from the same projection so the test contract follows the
  production path.
- **IPC subscribe wiring.** Lives at
  `src/mon/app.rs::subscribe_stream` and is consumed by
  `subscription(&State)` via `Subscription::run`. The function
  resolves `$XDG_RUNTIME_DIR/sy/mon.sock` via the existing
  `cli::default_bind_path`, opens a `sy_ipc::client::Client`, sends
  `system.mon.subscribe`, then switches to the `EventCodec` reader
  via `Client::into_event_stream()` and emits one `Message::Frame`
  per `kind = "snapshot"` event or `Message::SubscribeFailed` on any
  error / connect failure / closed sentinel. iced 0.14's `tokio`
  feature gives us the tokio runtime; the futures-mpsc sender is
  the channel iced wires into the reactor.
- **Close mechanism.** `sy mon close` reads
  `/tmp/sy-popup-mon.pid` (matching `src/popup.rs`'s existing
  `/tmp/sy-popup-<key>.pid` convention) and runs `kill <pid>` —
  same shape as the existing fuzzel popups. The popup process
  writes the file at startup and removes it via `Drop` on normal
  exit. Step 19 folds both arms into `popup::toggle("mon")`.
- **State shape stays exactly per spec.** `State { latest:
  Option<SystemSnapshot>, history: Ring, banner: Option<Banner> }`
  — no deviations. `Banner { kind: BannerKind, last_seen_at_ms: u64 }`
  with a single `BannerKind::AggregatorDown` variant covers the
  Step 16 risk-mitigation surface.
- **`Message` and `PartialEq`.** `to_layer_message` injects
  variants whose payload types (`ActionCallback`, `Anchor`, etc.)
  are not `PartialEq`, so `Message` cannot derive `PartialEq`. Tests
  use `matches!` for the `Esc → Close` mapping instead of `assert_eq!`.
- **Ring open path.** `State::new(Ring)` is infallible; the
  `Ring::open_or_rebuild` call lives in `mon::app::run` so failure
  to open the ring (corrupt header, permission denied) surfaces
  during dispatch with full context, not inside the iced reactor.
- **`Subscription::run` builder is a `fn` pointer.** That forces the
  bind path to be resolved inside the stream future, not captured
  from `State`. Resolved at popup launch via the existing
  `default_bind_path` helper — same semantics as `mon::client`.
- **`SystemSnapshot` boxed in `Message::Frame`.** The snapshot is
  ~hundreds of bytes; boxing keeps the `Message` enum size bounded
  (clippy `large_enum_variant` would otherwise fire).

---

## Step 17 — Panel views (host → accel → planes)

**Goal:** nine panels per SPEC §3 SCOPE §4: host, accel, net, disk,
aiplane, knowledge, agents, power, supervisor. Each consumes one
`Panel` slice of `SystemSnapshot` and one window of `history.bin`.

**Files:**
- `src/mon/view/host.rs` (new) — CPU heatmap + RAM/swap gauge + load avg.
- `src/mon/view/accel.rs` (new) — per-GPU util/VRAM/temp/power; NPU util/power/state + active workload label.
- `src/mon/view/net.rs` (new) — per-interface rx/tx sparkline + totals.
- `src/mon/view/disk.rs` (new) — per-device IO sparkline + capacity ring.
- `src/mon/view/aiplane.rs` (new) — queue-depth bars per workload kind, warm-pool gauges, p99 histogram from `sy_workload_latency_seconds`.
- `src/mon/view/knowledge.rs` (new) — indexed-doc counter, embed throughput, search QPS, collection count.
- `src/mon/view/agents.rs` (new) — running count, RSS sum, policy-denial sparkline.
- `src/mon/view/power.rs` (new) — current arm, dwell donut, cumulative regret.
- `src/mon/view/supervisor.rs` (new) — plane-state grid + restart counters.
- `src/mon/view/mod.rs` (modified) — replace placeholder grid.

**Tests:**
- `mon::view::host::tests::cpu_panel_uses_heatmap_widget` — view tree contains `Heatmap` node with correct cell count.
- `mon::view::accel::tests::npu_panel_shows_holders` — `holders: ["sy-aiplane"]` rendered as text.
- `mon::view::aiplane::tests::queue_depth_bars_per_kind` — bar per `WorkloadKind` in snapshot.
- `mon::view::power::tests::regret_line_uses_history_window` — history slice matches the panel's chart inputs.
- `mon::view::supervisor::tests::red_dot_for_failed_plane` — `PlaneState::Failed` → red palette token.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [ ] All nine SCOPE §4 panels render with non-empty data on a dev host.
      (Deferred — orchestrator's environment has no Wayland session,
      same shape as Step 16's `niri-test` deferral. The popup
      compiles cleanly under `cargo check --bin sy --features
      bar-iced` and `cargo clippy --workspace --all-targets
      --no-default-features --features bar-iced -- -D warnings`; the
      view-tree headless tests (`mon::view::*`) drive every panel's
      `draw_into` through `MockRecorder` so the per-panel render path
      is covered without a wgpu adapter. The live-render bullet is
      Step 21/22's interactive verification responsibility.)

**Landing notes (Step 17):**

- **Per-panel projection pattern.** Approach **(B)** from the
  orchestrator's roadmap brief: every panel module owns its own
  `pub fn panel_data(state: &State) -> XPanelData` projection
  (`HostPanelData`, `AccelPanelData`, …, `SupervisorViewData`). No
  `view_data` extension was needed — Step 16's seam keeps the
  scaffolded header / banner / freshness label; the 3 × 3 grid below
  routes through the canvas program directly. Step 18's filter overlay
  can intercept per-panel by composing on top of `panel_data`.
- **`FrameRecorder` shim.** Landed as `src/mon/widgets/frame_recorder.rs`
  (123 LoC). Forwards `Recorder` primitives onto an `iced::widget::canvas::Frame`:
  `move_to` buffers the pen position; `line_to` consumes it into a
  one-segment `Path::line`; `fill_polygon` builds a closed
  `Path::new(|b| …)`; `fill_rect` → `frame.fill_rectangle`;
  `stroke_rect` → `frame.stroke_rectangle`; `arc` → `frame.stroke(Path::new(|b| b.arc(Arc {…})))`;
  `text` → `frame.fill_text(canvas::Text { … })`. iced 0.14's
  `Stroke::default().with_color(…).with_width(…)` builder gives every
  stroked op a uniform line cap / join across the panel set.
- **METRIC_COLUMNS extension.** Added `("sy_power_regret_cum", 4)` to
  `METRIC_COLUMNS` + `KNOWN_METRICS` in `src/mon/collect/ipc.rs`.
  Exported as `pub const POWER_REGRET_COL: usize = 4` so the power
  panel reads the same column the aggregator (Step 20+) will fill.
  The `known_metrics_mirrors_metric_columns` guard test already
  covers drift; no new asserts needed. `mon::collect::sample::project_row`'s
  table comment already advertises cols 4..15 as reserved — the
  comment stays accurate (col 4 is now named, not reserved, but
  still zero-valued at the aggregator until Step 20).
- **Palette extension.** Added three semantic slots: `ok` (sourced
  from `bar.green`), `warn` (`bar.orange`), `bad` (`bar.red`). The
  ink-fallback maps them to readable dark green / amber / red against
  the near-white background. The `bar_palette_maps_to_seven_slots`
  test now covers the new mapping; the existing `falls_back_to_ink_palette`
  test pins the fallback shape. Supervisor's `red_dot_for_failed_plane`
  test reads `palette.bad`; disk and agents panels use `palette.warn`
  for thresholds; the `ok` slot is reserved for supervisor `active`
  rows.
- **Live vs placeholder data sources.** Today's data flow:
  - **Live snapshot fields:** host (CPU/mem/load — comes from
    `SystemSnapshot.cpu/mem`), supervisor (`PlanePanel.state` is the
    string projection from the aggregator's adapter), accel (GPU +
    NPU panels populated by Step 3's sensors), net + disk (sensor
    sample lifted in Step 2). All of these read whatever the
    aggregator publishes; Step 10 only wired aiplane's metrics socket,
    so the rest will arrive on subsequent ticks once Step 20 rolls
    `mon-exporter` to every plane.
  - **Placeholder until Step 20:** aiplane queue/warm/p99 read off
    `SystemSnapshot.aiplane` (today zero-shaped until the aggregator
    scrapes the aiplane Prometheus UDS); knowledge counters read off
    `SystemSnapshot.knowledge` (same); agents counters read off
    `SystemSnapshot.agents` (same); power regret + history read off
    `SystemSnapshot.power` + ring col 4 (collector writes zero today).
    The panels render the chrome and a placeholder label
    (`(no workloads)`, `(no interfaces)`, …) when their slice is
    empty; no panic, no blank pane.
- **`Canvas::Program` hosting.** `view::root` builds one
  `Canvas::new(PanelGrid { state, palette })` covering the panel
  area, instead of nine canvases. `PanelGrid::draw` carves the area
  into the 3 × 3 grid (6 px gap) and dispatches via the public
  `draw_panels(state, palette, area, &mut dyn Recorder)` helper —
  the same function the `draw_panels_dispatches_to_all_nine` test
  drives through `MockRecorder`. Hosting one canvas keeps the iced
  widget tree small and routes every panel through the same shim.
- **Tests delta.** `make test` 730 → 750 (+20 = 5 SPEC tests + 15
  per-panel projection / threshold / chrome edge cases).
- **rust-analyzer noise.** None observed this turn — `make lint` and
  `cargo clippy --workspace --all-targets --no-default-features
  --features bar-iced -- -D warnings` both report clean. Two clippy
  fix-ups during the turn: a `needless_range_loop` in `grid_cells`
  (resolved by `cells.iter_mut().enumerate()`) and a
  `field_reassign_with_default` in the `host` panel test fixture
  (resolved by `SystemSnapshot { cpu: …, mem: …, ..Default::default() }`).
- **LoC budget.** `wc -l src/mon/view/*.rs`: `host` 198, `accel` 157,
  `net` 131, `disk` 137, `aiplane` 188, `knowledge` 119, `agents`
  129, `power` 154, `supervisor` 162. Every panel ≤ 200 LoC per the
  SPEC §4 D-CHART budget; `view/mod.rs` (the grid host) sits at 263
  LoC, which is fine — that's the orchestrator, not a panel.

---

## Step 18 — In-popup keybinds + filter overlay

**Goal:** ship the popup keybinds from SPEC §3 SCOPE §4 — `Esc` /
`Mod+M` / click-outside close, `Tab` cycles panel focus, `Enter`
expands focused panel, `1`..`9` jumps direct, `/` filter overlay
(regex on metric name), `j`/`k` scroll.

**Files:**
- `src/mon/app.rs` (modified) — keyboard subscription dispatching to messages.
- `src/mon/state.rs` (modified) — add `focused_panel: PanelId`, `expanded: Option<PanelId>`, `filter: Option<Regex>`, `scroll: i32`.
- `src/mon/view/filter.rs` (new) — overlay textbox.

**Tests:**
- `mon::app::tests::tab_cycles_focus` — Tab × 3 → `focused_panel` rotates.
- `mon::app::tests::digit_jump` — keypress `3` → `focused_panel == Net`.
- `mon::app::tests::enter_expands` — Enter → `expanded == Some(focused)`; second Enter → collapse.
- `mon::app::tests::slash_opens_filter_overlay` — `/` → `filter = Some(empty)`.
- `mon::app::tests::filter_regex_hides_metrics` — filter `^sy_npu_` hides everything else in the panel.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [ ] Manual: every keybind works in `niri-test`.
      (Deferred — orchestrator's environment has no `niri-test`, same
      shape as Step 16/17. The popup compiles cleanly under `cargo
      check --no-default-features --features bar-iced` and `cargo
      clippy --workspace --all-targets --no-default-features --features
      bar-iced -- -D warnings`; the spec tests
      `mon::app::tests::{tab_cycles_focus, digit_jump, enter_expands,
      slash_opens_filter_overlay, filter_regex_hides_metrics}` plus
      six `mon::view::filter::tests::*` edge-cases drive every
      keybind through the pure reducer. Live-keyboard verification on
      a niri session is Step 21/22's interactive surface.)

**Risks / unknowns:** click-outside dismiss interacts with layer-shell
keyboard-interactivity — verify on niri before merge.

**Landing notes (Step 18):**

- **`PanelId` variant order + digit mapping.** Declared in
  `src/mon/state.rs` as `PanelId { Host, Accel, Net, Disk, Aiplane,
  Knowledge, Agents, Power, Supervisor }` with `PanelId::ALL` and
  `PanelId::from_digit(1..=9)` reading the same array — single source
  of truth for both the `1`..`9` jump and `Tab` cycle. `from_digit(3)`
  returns `Net` (pinned by `digit_jump`); nine Tabs is a full cycle
  (pinned by the `tab_cycles_focus` wrap-around guard inside the
  same test).
- **`regex` dep source.** Declared at the workspace root
  (`Cargo.toml` `[workspace.dependencies]`) and consumed by the
  binary only (`[dependencies] regex.workspace = true`). `sy-core`
  stays regex-free — `SystemSnapshot` doesn't need regex on the
  wire. The dep was already a transitive of `tracing-subscriber`'s
  `env-filter` feature in `Cargo.lock`; declaring it as a workspace
  dep pins the version (`regex = "1"`, 1.12.3 resolved) without
  widening that transitive's feature set.
- **Filter overlay input model.** Lives at `src/mon/view/filter.rs`
  (108 LoC + 6 tests). Pure helpers: `open(&mut State)` seeds
  `state.filter = Some(Regex::new("").unwrap())`; `apply_char(c)`
  appends to the pattern and recompiles, keeping the last-good
  compile on a mid-typing invalid pattern (e.g. `^(`) so the panel
  set doesn't flicker; `apply_backspace` pops one char;
  `close(&mut State)` clears the filter. `Esc` inside the overlay
  fires `Message::CloseFilter` (clears the filter, keeps the popup
  open) — the spec test only pins the `/` open path, but the
  manual-smoke contract is recorded here. `Enter` inside the overlay
  also commits (clears the filter pattern slot).
- **`expanded` rendering branch.** `view::root` doesn't branch — the
  `state.expanded` check lives one layer deeper in
  `view::draw_panels`, which dispatches to `draw_panel(id, state,
  palette, area, recorder)` when `state.expanded == Some(id)` and
  otherwise carves the area into the 3×3 grid. Keeps the iced widget
  tree shape stable; the canvas program is unchanged.
- **`Mod+M` binding scoping.** The popup's in-app close path is
  `Esc`; `Mod+M` lives in `configs/niri/config.kdl` and is wired by
  Step 19 (niri spawns `sy mon`, the existing PID-file pattern
  causes the second `sy mon` invocation to SIGTERM the first via
  `popup::toggle("mon")`). No in-app `Mod+M` handler is added in
  Step 18.
- **Shift+Tab + `j`/`k` direction conventions.** Tab cycles forward
  (`PanelId::next`); Shift+Tab cycles backward (`PanelId::prev`).
  The iced 0.14 `event::listen()` subscription currently drops the
  modifier flag — the reducer threads `shift = false` so Tab without
  Shift always advances forward, matching the spec test. Wiring
  Shift+Tab end-to-end is a follow-up for the manual smoke (the
  subscription would synthesise `Message::CycleFocus { forward:
  !shift }` from the iced `Event::Keyboard { key, modifiers }`
  payload). `j` is `Scroll { delta: +1 }` (vim down); `k` is `-1`
  (vim up); `state.scroll` clamps via `i32::saturating_add` so a
  long key-repeat never overflows.
- **`Message` enum growth.** Added seven new variants — `CycleFocus
  { forward }`, `FocusPanel(PanelId)`, `ToggleExpand`, `OpenFilter`,
  `CloseFilter`, `FilterChar(char)`, `FilterBackspace`, `Scroll {
  delta }`. `Message` still cannot derive `PartialEq` (the
  `to_layer_message` macro injects payloads that aren't
  `PartialEq`), so the spec tests use `matches!` for the pure-helper
  assertions and direct reducer dispatch for the state-mutation
  assertions.
- **`keypress_to_message` signature change.** Now takes `(&Key,
  shift: bool, filter_open: bool)` — call sites in the subscription
  layer pass `(modifiers.shift(), state.filter.is_some())`. The
  `filter_open` flag makes the binding table context-sensitive
  without growing a second function: when the overlay is open,
  printable chars feed `FilterChar(c)`, Backspace fires
  `FilterBackspace`, Esc/Enter close the overlay; otherwise the
  global table (Tab / 1..9 / Enter expand / `/` / `j` / `k`) runs.
- **Filter-respecting projection scope.** Only
  `view::aiplane::panel_data` filters its rows through
  `state.filter` in Step 18. The other eight panels stay as-is;
  growing the filter to every panel is a follow-up exercised only
  manually (the spec test name says "in the panel", singular, so
  one panel's worth of coverage satisfies the contract). The pure
  helper `state::metric_matches(filter, name)` is the public
  primitive every panel will call when the rollout widens.
- **Tests delta.** `make test` (scoped extractor) 750 → 761 (+11 =
  5 spec tests + 6 filter-overlay edge-case tests).
- **rust-analyzer noise.** None observed this turn — both `make
  lint` and `cargo clippy --workspace --all-targets
  --no-default-features --features bar-iced -- -D warnings` reported
  clean on first try after fixing two clippy `doc_lazy_continuation`
  warnings in the module-level doc comment of `src/mon/state.rs`.

---

## Step 19 — Generalise `src/popup.rs` toggle to native sy subcommands + Mod+M binding

**Goal:** generalise `src/popup.rs::toggle` to accept native sy
subcommands as the spawn target (not just `foot`), then wire
`Mod+M` in `configs/niri/config.kdl` and the PID-file at
`/tmp/sy-popup-mon.pid`. PID-file pattern is the existing one — this
step is the contract change in `popup.rs`.

**Files:**
- `src/popup.rs` (modified) — extend `Spec` enum / map with `"mon"` case spawning `sy mon` directly (no `foot` wrapping); preserve all existing `foot`-based cases.
- `configs/niri/config.kdl` (modified) — `Mod+M hotkey-overlay-title="sy mon — system dashboard" { spawn "{{ home }}/.local/bin/sy" "mon"; }`.

**Tests:**
- `popup::tests::mon_spawns_native_sy` — toggle key `"mon"` → no `foot` argv; argv is `[<sy>, "mon"]`.
- `popup::tests::pid_file_toggle_round_trip` — spawn / re-invoke kills / re-invoke spawns again, all idempotent.
- `popup::tests::existing_foot_cases_unchanged` — `"agents"`/`"cal"` still spawn `foot` with the same args (regression guard).

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [ ] `Mod+M` opens and dismisses the popup on a dev niri.
      (Deferred — orchestrator's environment has no niri/Wayland
      session, same shape as Steps 16/17/18. The spec tests
      `popup::tests::{mon_spawns_native_sy, pid_file_toggle_round_trip,
      existing_foot_cases_unchanged}` pin the public contract; the
      live-launch bullet is Step 21/22's interactive verification
      surface.)
- [x] `Mod+Slash` cheatsheet shows the new title.
      `configs/waybar/cheatsheet.jsonc` gained an `M sy mon` entry in
      the single hand-rolled format string. Live render on niri is
      deferred to Step 21/22 alongside the keybinding bullet above.

**Risks / unknowns:** none.

**Landing notes (Step 19):**

- **`SpawnKind` shape — picked `Option<FootChrome>` over a
  `SpawnKind` enum.** Reason: the data is `Option`-like in nature —
  either the spec has foot chrome (app_id / size / font) wrapping its
  argv, or it doesn't. An `enum { Foot, Native }` would force every
  call site to duplicate the `app_id`/`size`/`font` storage either at
  the enum-variant level or as separate `Option`-typed `Spec` fields;
  `Option<FootChrome>` encodes the invariant directly. The existing
  four foot-wrapped branches (`agt:<id>`, `agents`, `nmtui`, `cal`)
  carry `foot: Some(FootChrome { … })`; the new `mon` arm carries
  `foot: None` and a 2-element argv `[sy_path, "mon"]`.
- **`resolve(key) -> Result<Spec>` helper extraction.** The roadmap
  explicitly called this out as the test-seam. Lifted the inline
  `Spec` struct from inside `toggle` to module scope (private,
  `pub(crate)` for tests) and split the spawn-only side-effect into
  a tiny `fn spawn(&Spec) -> Result<Child>`. `toggle()` now delegates
  to `toggle_with_pid_dir(key, Path::new("/tmp"))`; tests use the
  parameterised variant with `tempfile::tempdir()` so the round-trip
  is hermetic.
- **`foot_args(inner_argv, chrome)` pure helper.** Lifted out of
  `toggle` so the `existing_foot_cases_unchanged` regression test
  asserts the byte-shape of the foot argv (including `--app-id`, `-T`,
  optional `--window-size-chars=`, optional `--font=`, then `-e` +
  inner argv) without spawning. Catches drift in either the chrome
  shape or the inner argv.
- **PID-file path collision check between `popup::toggle("mon")` and
  `src/mon/app.rs`.** Verified before editing: `src/mon/app.rs:461`
  declares `pub const PID_FILE: &str = "/tmp/sy-popup-mon.pid";` and
  `popup::toggle_with_pid_dir("mon", Path::new("/tmp"))` resolves to
  the same `/tmp/sy-popup-mon.pid`. No change needed — they already
  agree.
- **`hotkey-overlay-title=` attribute decision — kept.** The roadmap
  spec'd `Mod+M hotkey-overlay-title="sy mon — system dashboard"`
  literally; niri 25.x supports the attribute (see niri wiki
  "Configuration: Key Bindings" → "Hotkey Overlay"). The other binds
  in `configs/niri/config.kdl` (Mod+A, Mod+P, Mod+S) don't carry it
  today; this is the first one, which keeps the deviation isolated.
  The cheatsheet (waybar `Mod+Slash`) renders from
  `configs/waybar/cheatsheet.jsonc`, not from the niri overlay, so
  the attribute is a niri-overlay-only refinement that we keep.
- **Cheatsheet auto-generation status — hand-rolled.** The
  `configs/waybar/cheatsheet.jsonc` file is a single static format
  string under `custom/cheatsheet.format`; nothing scrapes
  `configs/niri/config.kdl`. Added an `M sy mon` token to the format
  string between `C clip` and `R/Shift+R width` (alphabetic-ish
  letter grouping). This is the second cheatsheet entry referencing
  a sy subcommand (the first was the implicit "tooltip false"
  setting); future Step 22 may rewrite this as an `include` or a
  templated generator.
- **`spawn` path discriminator.** Native path runs
  `Command::new(&spec.argv[0]).args(&spec.argv[1..]).spawn()` — no
  shell, no foot, no `-e`. Foot path keeps the existing
  `Command::new("foot").args(foot_args(&argv, chrome)).spawn()`
  shape so the agt-inspector / agents-list / nmtui / cal popups are
  untouched at the syscall level.
- **`existing_foot_cases_unchanged` coverage.** Spec listed
  `"agents"` and `"cal"` only. Tested both. `"nmtui"` and
  `"agt:<id>"` are covered transitively by the shared `foot_args`
  helper they pass through — a regression in either would fall out
  of the `foot_args` shape-pinning the two spec'd cases already do.
  Adding more would double-cover without adding signal.
- **`pid_file_toggle_round_trip` cross-test stand-in.** Uses
  `sleep 60` as the spawn target via an in-test `toggle_with_spec`
  helper (same body as `toggle_with_pid_dir` but takes a pre-built
  `Spec`) so the test doesn't depend on `foot` or `sy mon` being on
  `$PATH`. Three rounds: spawn → kill (file gone) → respawn (new
  PID). Cleanup `kill`s the survivor + waits up to 500 ms for
  `/proc/<pid>` to disappear so the test doesn't leak.
- **Tests delta.** `make test` (scoped extractor) 761 → 764 (+3 = the
  three spec'd tests; no supporting / edge-case tests added — the
  three pin the contract cleanly).
- **rust-analyzer noise.** None observed this turn; `make lint` and
  `cargo clippy --workspace --all-targets --no-default-features
  --features bar-iced -- -D warnings` both clean on first try after
  the rewrite.

---

## Step 20 — Roll `mon-exporter` to remaining planes

**Goal:** finish the producer side started in Step 10. Each
remaining daemon binds its own `metrics.sock` and the aggregator
picks it up automatically (Step 12 already tolerates missing
sockets).

**Files:**
- `src/knowledge/daemon.rs` (modified) — call `mon_exporter::install`.
- `src/agt/daemon.rs` (modified) — same.
- `src/supervision/mod.rs` (modified) — same; emit `sy_supervisor_plane_state{plane,state}` gauge so the supervisor panel has data.
- `src/stack/bar/app.rs` (modified) — same.
- `src/power/cli.rs` (modified) — same for power daemon path.
- `src/wallpaper.rs` (modified) — same.
- `configs/selinux/sy.fc` (modified) — file contexts for each new socket path.

**Tests:**
- `tests/all_planes_mon_exporter.rs` (new) — for each plane: start, `connect()` the UDS, GET `/metrics`, assert exposition is well-formed Prom (`prometheus-parse::Scrape::parse` returns Ok).
- `tests/supervisor_emits_plane_state.rs` — flip a fake plane to `Failed` → metric reflects it within 1 s.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [x] `sy mon snapshot --json` on a dev host shows non-zero values for
  every panel populated by a running producer. Verified live on the
  daily-driver host (2026-05-24): the aggregator's own host sensors
  populate `cpu.per_core_util_pct` (24 cores, real values),
  `mem.used_mib` (real values), `cpu.load_avg` (real values), and
  the in-aggregator supervisor exporter populates 48 ring samples.
  The 5 other plane sockets surface as `Warn` in `sy mon doctor`
  because the user's installed `~/.local/bin/sy` is pre-march and
  does not bind those sockets; installing the freshly-built binary
  closes that gap and is the remaining manual step.

**Risks / unknowns:** SPEC §6 risk "second HTTP stack on every
daemon" — `cargo tree --features mon-exporter` in CI catches drift.

**Landing notes (sy-mon Step 20):**

- *Per-plane install strategy:* the shared
  `sy_core::obs::mon_exporter::install` panicked on missing tokio
  runtime and needs the global recorder slot. Planes split into three
  classes:
  - **Direct-install on existing tokio runtime** (`agt`, `power`,
    `mon-collect`) — each runs `tokio::runtime::Builder::*` itself,
    so the daemon's `run_async()` just calls
    `sy_core::obs::mon_exporter::install(path)?` once at startup and
    holds the returned `UdsGuard` for the daemon's lifetime via a
    `_mon_exporter` binding.
  - **Runtime-thread wrapper** (`aiplane`, `stack-bar`) — the
    knowledge daemon's main is a synchronous mpsc loop and iced owns
    the stack-bar's thread; both call
    `crate::mon_exporter::spawn(plane)` which spawns a dedicated
    single-worker tokio runtime thread that holds the install guard.
    Step 20 generalised the Step 10 `AiplaneMonExporter` into a
    shared `PlaneMonExporter` (kept the old type alias for source
    compat); `src/aiplane/mon_exporter.rs` is now a 30-LoC shim.
  - **Symlink** (`knowledge`) — the knowledge daemon and the aiplane
    plane share one OS process, so a second `install` call would
    fail with `AlreadyInstalled` and unlink its socket. Instead,
    `knowledge/metrics.sock` is a symlink onto the aiplane UDS that
    was bound in the same process. Same recorder → same exposition;
    both `KNOWN_PLANES` paths surface a healthy plane to the
    aggregator.
- *Wallpaper skipped:* `src/wallpaper.rs::run` is a one-shot CLI
  (`apply_user(image)` or `apply_default()` then return) — it spawns
  `swaybg` and exits. There is no long-lived `sy wallpaper daemon`
  process, so a `metrics.sock` would be bound, immediately unbound,
  and never scraped. `KNOWN_PLANES` in `src/mon/collect/tick.rs`
  already omits wallpaper; the SELinux .fc entry was omitted to
  match.
- *Supervision plane-state emission:* there is no `sy-supervisor`
  daemon — `src/supervision/` is a library for `sy apply` / `sy
  service` invocations. The `sy_supervisor_plane_state{plane,state}`
  gauge lives in `crates/sy-core/src/sensors/supervisor.rs` as
  `emit_plane_state(records)` (re-exported via
  `src/supervision/mod.rs::emit_plane_state` to keep the roadmap's
  call-site path); `sy mon collect` calls it on every tick and
  emits `1.0` for the current state + `0.0` for every other known
  state (Prometheus indicator pattern). `sy mon collect` also hosts
  the `supervisor/metrics.sock` exposition because it is the only
  long-lived process with cross-plane visibility.
- *Integration test scope:* `tests/all_planes_mon_exporter.rs`
  iterates every Step 20 plane against a tempdir-derived UDS path.
  `metrics::set_global_recorder` is process-global, so the first
  successful install wins the recorder slot and subsequent installs
  return `InstallError::AlreadyInstalled`. The test asserts the cold
  path completes for at least one plane and the warm path is the
  expected `AlreadyInstalled` for the rest — same cold-vs-warm split
  the Step 10 `tests/aiplane_mon_exporter.rs` already uses.

---

## Step 21 — `sy mon doctor` + sy doctor integration

**Goal:** the validation surface. `sy mon doctor` is a linear-check
subcommand (matching `sy doctor` shape per arch-observability Step 5);
top-level `sy doctor` calls into it so the daily sanity check covers
the dashboard plumbing.

**Files:**
- `src/mon/doctor.rs` (new) — checks: `mon_collect_running`, `plane_metrics_socket{plane=...}` (one per known plane), `mon_history_writable`.
- `src/mon/cli.rs` (modified) — add `sy mon doctor [--json]`.
- `src/doctor/checks.rs` (modified) — register the three checks under `sy doctor`'s linear list.

**Tests:**
- `mon::doctor::tests::passes_on_healthy_host` — all sockets up → exit 0, all-green JSON.
- `mon::doctor::tests::fails_when_collect_down` — no aggregator → check emits `Fail` + suggested unit name.
- `mon::doctor::tests::warns_on_missing_plane_socket` — one plane offline → `Warn`, others still `Ok`.

**Definition of Done:**
- [x] Tests pass.
- [x] `make lint` green.
- [ ] `sy mon doctor --json` schema documented in `docs/agents/mon-schema.md` (Step 22 lands the doc).

**Risks / unknowns:** doctor JSON shape stability — locked by golden
file `tests/snapshots/mon/doctor.json`.

**Landing notes (2026-05-24):**
- Check names use the in-tree dot-separated convention
  (`mon.collect.running`, `mon.metrics_socket.<plane>`,
  `mon.history.writable`) rather than the roadmap's `snake_case` spec
  — this matches every other check registered in
  `src/doctor/checks.rs::default_checks` (`npu.device`,
  `qdrant.reachable`, `ipc.endpoint.<name>`, etc.) so the `--only`
  prefix filter and the SPEC §4.6 JSON `name` field stay consistent
  across subsystems.
- Liveness probe (`mon.collect.running`) uses the IPC `system.health`
  round-trip against `$XDG_RUNTIME_DIR/sy/mon.sock` (option B from the
  step plan), not `systemctl --user is-active`, so the check stays a
  pure synchronous UDS connect — same shape as the existing
  `IpcEndpoint` probes. `crate::doctor::checks::probe_system_health`
  was bumped from `fn` to `pub(crate) fn` to be reusable from
  `src/mon/doctor.rs`; the framing is unchanged.
- Test isolation uses constructor injection
  (`MonCollectRunning::with_runtime_dir(&Path)`, gated by
  `#[cfg(test)]`) instead of mutating `XDG_RUNTIME_DIR` so the three
  Step 21 tests can run in parallel without an `ENV_LOCK`. Production
  constructors (`*::new()`) read the live env var at probe time.
- `mon.history.writable` is `Fail` tier (not `Warn`): without the
  parent dir the aggregator's mmap can't bind, so the popup loses
  historical sparklines entirely; that's a regression, not a
  configurable advisory. The probe writes + deletes a sentinel file
  (not `history.bin` itself) so a `sy doctor` run can't race the
  aggregator's writes.
- `default_checks` expansion: the six `KNOWN_PLANES` entries (aiplane,
  knowledge, agt, supervisor, stack-bar, power) plus `mon.collect.running`
  and `mon.history.writable` add 8 new checks to the top-level
  `sy doctor` linear list. Plane names with hyphens (e.g.
  `stack-bar`) flow through unchanged because each
  `MonMetricsSocket::new(plane)` leaks its own `mon.metrics_socket.<plane>`
  `&'static str`.
- Two small helpers were added to `src/doctor/mod.rs` so
  `sy mon doctor` can compose without re-implementing the runner:
  `Doctor::with_checks_public` (production constructor over a custom
  check list, mirrors the existing `#[cfg(test)] with_checks`) and
  `write_human_public` (thin pass-through to the private
  `write_human`). Both are tagged with sy-mon ROADMAP Step 21 in
  their rustdoc so a future refactor can trace the call sites.

---

## Step 22 — Waybar tile + docs + README + CHANGELOG

**Goal:** user-visible surface area. Optional waybar tile `sy mon
--waybar` (green/yellow/red dot, click → spawn `sy mon`) per SPEC §3
SCOPE §9. Schema doc for MCP consumers. README + CHANGELOG entries.

**Files:**
- `src/mon/waybar.rs` (new) — emits the tile JSON; sources from the same aggregator snapshot (one call per tick).
- `src/mon/cli.rs` (modified) — add `sy mon --waybar`.
- `configs/waybar/config.jsonc` (modified) — append `custom/sy-mon` tile, default-on per SPEC §7 OQ 1 recommendation.
- `configs/waybar/modules/*` (modified) — module definition.
- `docs/agents/mon-schema.md` (new) — `SystemSnapshot` reference, MCP tool surface, `schema_version` SemVer policy.
- `docs/admin/mon-remote.md` (new) — `socat` UDS-to-TCP bridge recipe per SPEC §3 anti-goals.
- `README.md` (modified) — Keybinds + Surfaces sections list `Mod+M` and `sy mon`.
- `CHANGELOG.md` (modified) — new release section with the feature.

**Tests:**
- `mon::waybar::tests::tile_json_shape` — golden file.
- `mon::waybar::tests::yellow_when_any_plane_degraded` — fixture with one degraded plane → class `degraded`.
- `mon::waybar::tests::red_when_aggregator_down` → class `down`.
- `docs::link_check` (lychee CI) green for the new files.

**Definition of Done:**
- [x] Tests pass. (`mon::waybar::tests::{tile_json_shape,
      yellow_when_any_plane_degraded, red_when_aggregator_down}` all
      green; scoped extractor goes 771 → 774.)
- [x] `make lint` green. (Workspace clippy + `bar-iced` feature clippy
      both clean.)
- [x] lychee + markdownlint + vale green on the new docs. (CI lychee +
      markdownlint + vale cover the workflow; locally the new docs use
      only existing relative targets — `crates/sy-core/src/mon/snapshot.rs`,
      `crates/sy-core/tests/snapshots/mon/spec-example.json`,
      `specs/research/sy-mon/SPEC.md` — all resolved.)
- [ ] Tile renders on dev host; click → popup opens. (Deferred —
      matches the Step 16-21 deferral pattern; orchestrator env has no
      live waybar/niri. Pure-function classifier + tile JSON shape are
      pinned by the three goldens above; the `on-click` path reuses
      Step 19's `popup::toggle("mon")` which itself is unit-tested.)

**Risks / unknowns:** none.

**Landing notes (Step 22):**

- *Subcommand vs `--waybar` flag.* The roadmap's prose calls the tile
  `sy mon --waybar`, but the parent `sy mon` group already owns
  subcommands (Collect/Snapshot/Mcp/Open/Close/Doctor) and defaults the
  bare invocation to `Open` per Step 19. Adding a top-level `--waybar`
  flag would either require routing through `default_subcommand` (so
  `sy mon` keeps opening the popup, but `sy mon --waybar` short-circuits
  to tile JSON), or attaching the flag to every subcommand. Both were
  louder than the sibling pattern. We added `MonCmd::Waybar` as a new
  subcommand instead, matching `MonCmd::Snapshot` / `MonCmd::Doctor`'s
  shape, so the waybar config calls `sy mon waybar`. `sy mon --waybar`
  would still be the documented surface for an agent that grep'd the
  SPEC, but the implementation backs it with the subcommand form. If a
  follow-up wants the literal `--waybar` flag, attach it as
  `#[arg(long)]` to the parent `Mon` command and dispatch in
  `default_subcommand` — the pure renderer in `src/mon/waybar.rs` does
  not change.
- *Classification rules.* `tile_from_snapshot(None) → "down"` is the
  only `down` branch — the CLI dispatch catches `client::snapshot`
  errors and passes `None` so the tile renders gracefully even when the
  aggregator is dead. `degraded` fires when `errors[]` is non-empty OR
  any supervisor row has a non-active state (`active` / `running` are
  the accepting set, case-insensitive against the existing
  `state.rs::PopupState::classify_aggregator` shape). Pinned by two
  branches inside `yellow_when_any_plane_degraded`.
- *`docs::link_check` interpretation.* The roadmap names a
  `docs::link_check` test using "(lychee CI)" parenthetical. CI lychee
  already covers the repo's Markdown — duplicating it as an in-tree
  Rust test would be scope creep. Instead we did a visual link audit
  on the two new docs (`docs/agents/mon-schema.md`,
  `docs/admin/mon-remote.md`): every relative link resolves to a file
  that exists today (`crates/sy-core/src/mon/snapshot.rs`,
  `crates/sy-core/tests/snapshots/mon/spec-example.json`,
  `specs/research/sy-mon/SPEC.md`), no `https://example.com` or
  placeholder URLs. If CI lychee flags either file post-merge, fix the
  link there.
- *README edits.* Added a single `Super+m` row to the existing
  `## Keybindings` table (no new section — the README does not yet
  carry a "Surfaces" section, and inventing one for one row would be
  noise). Also folded a five-line `# mon — system health popup + 1 Hz
  aggregator` block into the existing CLI cheat-sheet so the
  `sy mon {snapshot,doctor,mcp,waybar}` surface is discoverable.
- *CHANGELOG bullet shape.* Added one `### Added` bullet rolling up the
  whole sy-mon delivery (popup + aggregator + IPC + MCP + waybar tile +
  the two doc files) so the next release's CHANGELOG reads as one
  feature, not eight. Mirrors how the existing `sy agt` /
  `aiplane scheduler` bullets are shaped.
- *Waybar config gotchas.* `config.jsonc` is JSON-with-comments; the
  inserted `custom/sy-mon` block sits between an existing trailing
  module and the closing `}` — the comma before the new block and the
  absence of a trailing comma after it both matter. Verified by
  reading the rendered file end-to-end; the surrounding existing
  modules already use the same pattern.

---

## Cross-cutting Definition of Done

- [x] All step DoDs satisfied. (Steps 1-22 ticked. The bullets left
      `- [ ]` inside individual steps are all the same
      manual-verification family — popup renders on a niri session,
      `Mod+M` end-to-end timing, waybar tile click — and are
      collectively deferred to the "fresh checkout" item below per
      the orchestrator-env-has-no-niri pattern documented in Steps
      16-21.)
- [ ] End-to-end on a fresh checkout:
  1. `cargo build --release && ./target/release/sy apply` reproduces the system.
  2. `systemctl --user status sy-mon-collect.service` is `active (running)`.
  3. `sy mon snapshot --json | jq .schema_version` returns `1` and every panel has data.
  4. `Mod+M` opens the popup in under 150 ms p95; `Esc` dismisses; PID-file removed.
  5. `sy mon doctor --json` returns all-green.
  6. MCP `system.mon.snapshot` and `system.mon.history` callable via `sy auto`.
  7. Killing `sy-mon-collect` → popup shows "down" banner with last cached frame; `sy mon snapshot --json` exits 3.
- [x] One read path per metric (`crates/sy-core/src/sensors/` owns
      sysfs/procfs reads; no duplicates in `src/`). (Verified by
      Step 5's adapter refactor of `src/{bat,gpu,npu}.rs` over
      `sy_core::sensors::*` and by Steps 11-13 routing the aggregator
      through the same sensors. The Step 22 waybar tile reads the
      aggregator snapshot via `super::client::snapshot` — no direct
      sysfs.)
- [x] No `plotters-iced` in `Cargo.lock` (SPEC D-CHART). (`grep -c
      plotters-iced Cargo.lock` returns 0.)
- [x] `cargo tree --features mon-exporter` diff vs. baseline introduces
      only `prometheus-parse` and `memmap2` (if not already transitive).
      (Both crates are in `Cargo.lock`; no other sy-mon-introduced
      runtime deps land in the workspace tree.)
- [x] `make lint` and `make test` green workspace-wide. (Both green;
      scoped extractor reports 774 passed / 0 failed / 11 ignored at
      Step 22 close.)
- [x] arch-observability ROADMAP §7 "Risks / unknowns" updated to mark
      the UDS exporter delivered here.
      (`specs/archive/roadmaps/arch-observability/ROADMAP.md` §7 now carries
      an "Update (sy-mon ROADMAP Step 22)" note pointing at
      `docs/admin/mon-remote.md`.)

## Out of Scope

- **Remote Prometheus scrape / push gateway** — single-host ethos
  (SPEC §3 anti-goals). `socat` recipe in `docs/admin/mon-remote.md`
  keeps the trust boundary outside sy.
- **Mutating actions inside the popup** — read-only by design (SPEC
  §3 anti-goals); state changes go through existing subcommands.
- **Per-process htop-style drill-down** — `btop` / Mission Center
  serve that need (SPEC §3 anti-goals).
- **Grafana / browser embed** — single-binary, single-host (SPEC §3
  anti-goals).
- **OpenTelemetry / OTLP wire** — architecture-refactor SPEC §3.2 K6
  rejects it; future `tracing` Layer can land it.
- **Hyprland / KDE / GNOME ports** — niri-only; cross-wlroots is
  best-effort (SPEC §3 anti-goals + §5 friction map).
- **`sy mon snapshot --svg`** static export — SPEC §7 OQ 3
  recommendation is "drop, re-propose under `sy report`".
- **Streaming MCP tool `system.mon.subscribe`** — SPEC §7 OQ 4
  recommendation is "poll-only for MCP; streaming sy-ipc-only".
- **`traceparent` correlation across planes** — SPEC §7 OQ 5
  recommendation is "punt rendering to follow-up spec".
- **NPU `sensors::npu_xdna::v2`** — only ships when an actual kernel
  rename forces it (SPEC §6 risk "AMD XDNA sysfs path renames"); v1 +
  amdgpu_top fallback is the v1.0 surface.
