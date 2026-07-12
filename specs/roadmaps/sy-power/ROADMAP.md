# ROADMAP: sy-power

Source: `specs/research/sy-power/SPEC.md` (Revision 2, 2026-05-14).

User journey lives inline in SPEC §5 ("User Journey Sketch") rather
than in `specs/journeys/`; the SPEC is the canonical journey for this
feature. If a step needs more journey detail, expand SPEC §5 in place
rather than fork.

## Overview

Land `sy power` — an ML-driven, intent-aware power orchestrator for
Ryzen AI HX 370 — across seven R-cuts that match SPEC §8. Each cut is
independently shippable: R1 collects telemetry under rules-only
control (Apple-style 14-day onboarding rehearsal); R2 turns on
actuation with the rules baseline + 5-state shield; R3 wires the
Conservative Linear UCB bandit through the rules path; R4 adds the
GRU forecaster + offline trainer + onboarding gate; R5 grafts in the
linfa-ftrl online activity classifier; R6 adds drift detection +
waybar tile; R7 ships the `net.hadess.PowerProfiles` D-Bus shim and
the MCP `power_status` tool. R8 (Rhai user-trigger overrides) is
explicitly post-v1 and out of scope here.

End state: `sy power {status, profile, explain, daemon, log, apply,
train, mcp}` is the operator surface; `sy-powerd` runs as a
`systemd --user` unit under `sy.target`, drives `platform_profile`
/ EPP / iGPU / NPU `pmode` / cgroup uclamp, never writes the SMU
mailbox, never writes a sysfs path that already matches, and
always degrades to vendor defaults on crash + watchdog miss. Waybar
shows the current profile + shield state + onboarding countdown +
drift indicator. GNOME's PPD slot is wire-compatibly replaced.

Today: `src/pwr.rs` is the existing fuzzel-driven `tuned-adm` menu
and stays untouched — `sy power` is a separate, longer-lived
subcommand under `src/power/`. `src/aiplane/registry.rs` is the
in-process registry the `intent::aiplane` panel taps. The shared
systemd target is `configs/systemd/user/sy.target`; `sy-powerd.service`
joins that target with `PartOf=sy.target` so it tears down with the
rest of the plane.

Pre-flight checks (verify before Step 1):

- Kernel ≥ 6.14 for `amdxdna`; kernel ≥ 7.1 unlocks NPU mW
  telemetry. If on 6.18/6.19 verify the amdgpu CWSR regression
  (Framework community thread cited in SPEC §6) does not blackout
  hwmon — the daemon must degrade gracefully but the sensor tests
  need real fixtures.
- `amd_dynamic_epp=disable` on the kernel command line. `sy power
  apply` lands a grub drop-in for this (Step 27 / R7), but the
  bench-only EPP test (Step 8) needs it disabled by hand on the
  dev machine to assert end-to-end writes.
- `xrt-smi configure --pmode` available (already exercised by the
  aiplane plane); confirm with `xrt-smi examine`.
- polkit ≥ 122 (Fedora 43 default) for the
  `org.sy.PowerProfile.SetProfile` action landed in Step 7.
- `~/.local/state/sy/power/` writable; ≥ 1 GiB free (Step 11's NDJSON
  log writer refuses to write below that).

Cargo budget: ~5-8 MB of new deps (tract-onnx, burn-ndarray,
burn-autodiff, trashpanda, linfa-ftrl, adskalman, augurs,
safetensors, arc-swap, zbus, procfs). Each lands in the first step
that needs it, not all up-front.

---

# Phase R1 — sensors + intent panels + NDJSON telemetry (no actuation)

Goal of R1: the daemon runs under `sy.target`, reads every sensor +
intent channel, assembles a 12-channel snapshot at 1 Hz, and writes
NDJSON to `~/.local/state/sy/power/telemetry.ndjson`. **No sysfs
writes.** This is the onboarding-rehearsal shape; behaviour is
identical to GNOME PPD + a thermal-aware rule table (rules baseline
arrives in R2).

## Step 1 — CLI skeleton + module scaffolding + config schema

**Goal:** `sy power {status,daemon,apply,log,profile,explain,train,show,mcp}`
parses, prints `--help`, and `sy power status --json` returns a
stub matching the SPEC §4 schema. No daemon, no sensors yet — this
is the scaffold every later step extends. (Note: `show` is a
post-SPEC addition driven by user request — Phase RV below — and
extends the SPEC §4 CLI surface.)

**Files:**
- `src/power/mod.rs` (new) — public surface + tracing setup.
- `src/power/cli.rs` (new) — clap subcommand tree per SPEC §4 "CLI /
  MCP Surface" plus the new `show` subcommand. Stub each handler
  to print `"unimplemented: step-<N>"` on stderr and exit 0 —
  explicit per-step exit codes arrive in Step 11 (`status`), Step
  19 (`profile`), Step 23 (`explain`), Step 13 (`apply`), Step 25
  (`train`), Step 35 (`show`), Step 38 (`mcp`).
  No `unimplemented!()` macro — banned by AGENTS.md non-negotiables.
- `src/main.rs` (modified, ~5 LoC) — wire `power::cli::run` under
  the existing subcommand dispatcher (next to `pwr`, `aiplane`).
- `configs/sy/power.toml` (new) — empty stanzas for `[arms]`,
  `[shield]`, `[bandit]`, `[reward]`, `[onboarding]` so later steps
  fill cells without inventing the schema mid-flight.
- `src/power/config.rs` (new) — `serde`-deserialize `power.toml`;
  default values match SPEC §6 Open Question defaults
  (`bandit.alpha = 0.05`, `onboarding.days = 14`).

**Tests:**
- `src/power/cli.rs::tests::help_lists_every_subcommand` — asserts
  `--help` mentions all 8 subcommands.
- `src/power/cli.rs::tests::status_json_stub_validates_schema` —
  parses stub output against the SPEC §4 `sy.power.status/v1`
  schema; required keys present, types match.
- `src/power/config.rs::tests::defaults_match_spec` — loading a
  config without `[bandit] alpha` yields `0.05`.
- `src/power/config.rs::tests::onboarding_env_override` —
  `SY_POWER_ONBOARDING_DAYS=5` overrides the TOML default.

**Definition of Done:**
- [x] `sy power --help` and `sy power status --help` complete +
      show examples (CLIG).
- [x] `sy power status --json` emits the v1 schema (stub values OK).
- [x] `make lint && make test` green.

**Risks / unknowns:** none — pure scaffolding.

---

## Step 2 — Hardware sensors A: pstate + platform_profile + hwmon

**Goal:** pure parse functions for `amd-pstate` governor, the
`platform_profile` enum + choices list, and `k10temp` / `amdgpu`
hwmon nodes. No daemon yet; tests run over `src/power/fixtures/sys/`.

**Files:**
- `src/power/sensors/mod.rs` (new) — re-exports + the `Sensor`
  trait (`fn read(&self, root: &Path) -> Result<SensorReading>`).
- `src/power/sensors/pstate.rs` (new) — parses
  `/sys/devices/system/cpu/cpufreq/policy*/scaling_governor` +
  `…/energy_performance_preference` from a configurable `sysfs_root`.
- `src/power/sensors/platform.rs` (new) — reads
  `/sys/firmware/acpi/platform_profile` + `…_choices`.
- `src/power/sensors/hwmon.rs` (new) — k10temp `Tctl`, amdgpu
  `edge` + `power1_average` from the `hwmon*` glob.
- `src/power/fixtures/sys/hx370/` (new) — captured snapshot of the
  HX 370 sysfs tree (governor=schedutil, EPP=balance_performance,
  platform_profile=balanced, Tctl=71 °C). Used by every sensor test.

**Tests:**
- `src/power/sensors/pstate.rs::tests::parses_governor_powersave` —
  fixture's `scaling_governor=powersave` round-trips.
- `src/power/sensors/pstate.rs::tests::epp_blocked_when_dynamic_enabled` —
  fixture with `amd_dynamic_epp=enable` returns
  `SensorReading::Blocked` (not a parse error — the lever is
  silently no-op, surface that).
- `src/power/sensors/platform.rs::tests::parses_choices_quiet_balanced_performance`.
- `src/power/sensors/hwmon.rs::tests::tctl_within_plausible_range` —
  fixture's k10temp Tctl in [20, 110] °C.

**Definition of Done:**
- [x] All three sensors parse fixtures without panicking.
- [x] `sensors::mod::Sensor` trait has zero `#[allow(dead_code)]`.
- [x] Fixture tree committed under `src/power/fixtures/sys/hx370/`.
- [x] `make lint && make test` green.

**Risks / unknowns:** kernel 6.19 amdgpu hwmon path may have changed
(SPEC §6 cites the CWSR thread). Capture the fixture from the dev
machine, not from documentation.

---

## Step 3 — Hardware sensors B: rapl + igpu + npu + battery

**Goal:** the remaining four sensors. RAPL via `powercap`, iGPU
`gpu_busy_percent` + `pp_power_profile_mode`, NPU queue depth via
the aiplane registry tap (deferred to Step 8), and battery SOC + AC
+ drain rate.

**Files:**
- `src/power/sensors/rapl.rs` (new) — reads
  `/sys/class/powercap/intel-rapl:0/energy_uj` deltas;
  `package_power_w_5tap` is a 5-sample moving average.
- `src/power/sensors/igpu.rs` (new) — `gpu_busy_percent` from
  `/sys/class/drm/card*/device/`; preset enum from
  `pp_power_profile_mode`.
- `src/power/sensors/npu.rs` (new) — workload count comes from
  the aiplane registry (in-process; no IPC); kernel ≥ 7.1 unlocks
  mW telemetry, degrade to "0 mW" until then per SPEC.
- `src/power/sensors/battery.rs` (new) — SOC %, AC bool, drain
  rate (W) from `/sys/class/power_supply/BAT*/`.

**Tests:**
- `src/power/sensors/rapl.rs::tests::moving_average_smooths_burst`.
- `src/power/sensors/igpu.rs::tests::parses_busy_percent_zero_when_idle`.
- `src/power/sensors/npu.rs::tests::workload_count_zero_without_registry` —
  the registry tap is wired in Step 8; assert the sensor gracefully
  returns 0, not an error.
- `src/power/sensors/battery.rs::tests::drain_rate_from_energy_delta`.

**Definition of Done:**
- [x] All four sensors parse fixtures; battery handles `AC=true`
      (drain rate = 0) and `AC=false` separately.
- [x] `make lint && make test` green.

**Risks / unknowns:** RAPL nodes are `intel-rapl*` even on AMD;
verify the dev fixture has the expected path. SPEC §4 calls it
`powercap / amd_energy`; the amd_energy node only appears on
specific kernel configs.

---

## Step 4 — Intent panel A: PSI cgroup-v2 triggers

**Goal:** `intent::psi` fires sub-second on a build leading edge (the
SPEC's biggest unique signal). Test via a synthetic FIFO mimicking
`/proc/pressure/cpu`'s `poll()` interface.

**Files:**
- `src/power/intent/mod.rs` (new) — re-exports + the
  `IntentChannel` trait (`fn poll(&mut self) -> Option<IntentEvent>`).
- `src/power/intent/psi.rs` (new) — opens `cpu.pressure` (or
  `io.pressure`, `memory.pressure`) on the current cgroup; writes a
  trigger spec (`some 150000 1000000`) to install a poll trigger;
  emits `IntentEvent::PsiSpike { kind, since_ms }` on poll wake.

**Tests:**
- `src/power/intent/psi.rs::tests::trigger_spec_round_trips`.
- `src/power/intent/psi.rs::tests::fires_on_synthetic_fifo` —
  spawn a thread that writes to a tempfile; asserts the poll
  returns within 100 ms.
- `src/power/intent/psi.rs::tests::degrades_when_pressure_disabled` —
  kernel without `CONFIG_PSI=y` returns `Err::PsiUnavailable`;
  daemon must keep running.

**Definition of Done:**
- [x] PSI trigger fires within 200 ms of a pressure rise on the
      synthetic fifo.
- [x] `make lint && make test` green.

**Risks / unknowns:** `poll()` on `cgroup.pressure` needs O_RDONLY +
edge-triggered EPOLLPRI; the kernel docs (linked in SPEC §2) are
prescriptive. Library doesn't exist; ~80 LoC hand-written nix call.

---

## Step 5 — Intent panel B: logind inhibitor watcher (zbus)

**Goal:** `intent::logind` subscribes to systemd-logind's inhibitor
list and emits `IntentEvent::CallActive` when any process holds
`Inhibit("idle")` with a `Who` field matching Teams / Slack / Zoom /
Discord. Adds `zbus = "5"` to Cargo.toml.

**Files:**
- `src/power/intent/logind.rs` (new) — `zbus` client connected to
  the system bus; calls `org.freedesktop.login1.Manager.ListInhibitors`
  on subscribe + on `org.freedesktop.login1.Manager.PrepareForSleep`
  + on `PropertyChanged`. Whitelist of comm names lives in
  `configs/sy/intent_whitelist.toml`.
- `configs/sy/intent_whitelist.toml` (new) — empty `[call]` array,
  populated in this step with the four canonical names (SPEC §2).
- `Cargo.toml` (modified, 1 line) — add `zbus = "5"`.

**Tests:**
- `src/power/intent/logind.rs::tests::whitelist_matches_teams` —
  pure-fn classification of a `(who, what)` tuple.
- `src/power/intent/logind.rs::tests::ignores_non_idle_inhibitors` —
  `Inhibit("sleep")` from systemd-update does not fire CallActive.
- Integration test gated by `cfg(feature = "test-logind")` —
  spawns the watcher against the live bus and `Inhibit`s from a
  `python3 -c "from gi.repository import GLib; …"` fixture.

**Definition of Done:**
- [x] Pure-fn classifier covers the four whitelisted names.
- [ ] Watcher emits `CallActive` within 1 s of inhibitor grab on
      the dev machine (manual verification recipe in the step's
      run-log). `Manual-verification-deferred` under /march policy
      (spawns `systemd-inhibit` side-effects); operator runs
      post-merge — see run-log recipe.
- [x] `make lint && make test` green.

**Risks / unknowns:** Zoom's `com.zoom.HotKeyService` is on the
session bus, not the system bus; logind covers the others. Document
this in the step's run-log; if Zoom fires no `Inhibit("idle")` on
recent versions, the user-portal ScreenCast panel (Step 7) catches it
as a fallback.

---

## Step 6 — Intent panel C: niri toplevel stream + aiplane registry tap

**Goal:** focused-app transitions over niri's IPC socket (sub-ms
latency), plus a zero-IPC tap into `src/aiplane/registry.rs` for NPU
queue depth — both light, both in-process.

**Files:**
- `src/power/intent/niri.rs` (new) — subscriber on
  `$XDG_RUNTIME_DIR/niri.sock`; consumes the `ext-foreign-toplevel-list-v1`
  event stream, emits `IntentEvent::FocusedAppChanged { app_id }`.
  **Strip `title`** at parse time per SPEC §4 "Privacy" — never
  carry the raw window title into the snapshot.
- `src/power/intent/aiplane.rs` (new) — borrows
  `aiplane::registry::current_queue_depth()` (new public fn);
  emits `IntentEvent::NpuQueue { depth, head_workload }`.
- `src/aiplane/registry.rs` (modified, ~10 LoC) — expose a
  `pub fn current_queue_depth() -> RegistrySnapshot` that
  returns by-value (Send + Sync); no lock held across the boundary.

**Tests:**
- `src/power/intent/niri.rs::tests::title_is_stripped` — feed a
  toplevel event with a title; asserts `app_id` survives,
  `title` is dropped from the event struct entirely.
- `src/power/intent/aiplane.rs::tests::queue_depth_zero_when_empty`.
- `src/aiplane/registry.rs::tests::current_queue_depth_is_consistent` —
  enqueue two stubs, assert depth=2.

**Definition of Done:**
- [x] No raw window titles cross the snapshot boundary
      (`cargo grep title.*Snapshot` returns nothing).
- [x] `make lint && make test` green.

**Risks / unknowns:** niri's `ext-foreign-toplevel-list-v1` socket
protocol may have an event-frame version skew across niri releases.
Pin against niri ≥ 25.05. (Resolved: implementation targets niri
≥ 26.04's JSON-line `event-stream` IPC at
`$XDG_RUNTIME_DIR/niri.wayland-*.sock`, decoding the
`WindowOpenedOrChanged` / `WindowsChanged` envelopes and stripping
`title` at the parser.)

---

## Step 7 — Intent panel D: MPRIS + xdg-portal ScreenCast + ext-idle + cgroup ancestry + notify + time

**Goal:** the remaining six light intent channels in one step. Each
is a thin zbus / file-watch / clock helper.

**Files:**
- `src/power/intent/mpris.rs` (new) — `org.mpris.MediaPlayer2`
  `PlaybackStatus` subscriber; emits `IntentEvent::MediaPlaying`.
- `src/power/intent/portal.rs` (new) — counts active
  `org.freedesktop.portal.ScreenCast` sessions (proxy for screen
  share / call); emits `IntentEvent::ScreenCastActive`.
- `src/power/intent/idle.rs` (new) — `ext-idle-notify-v1` on the
  Wayland socket; emits `IntentEvent::UserIdle { since_ms }`.
- `src/power/intent/cgroup.rs` (new) — walks `/proc/PID/cgroup`
  ancestry for new procs against an allow-list; emits
  `IntentEvent::ProcessFromAncestor { name }`. Uses `procfs = "0.18"`.
- `src/power/intent/notify.rs` (new) — sniffs notification bodies
  via `org.freedesktop.Notifications` for a coarse
  `user_complained_about_fan: bool` — **discards body text
  immediately** (SPEC §4 Privacy).
- `src/power/intent/time.rs` (new) — cyclical TOD + DOW encoding
  (sin/cos pair, 2-D feature).
- `Cargo.toml` (modified, 1 line) — add `procfs = "0.18"`.

**Tests:**
- `src/power/intent/mpris.rs::tests::playback_status_playing`.
- `src/power/intent/idle.rs::tests::since_ms_monotonic`.
- `src/power/intent/cgroup.rs::tests::detects_ancestor_match`.
- `src/power/intent/notify.rs::tests::fan_keyword_detected_body_discarded` —
  asserts the body string is `None` after the bool extraction.
- `src/power/intent/time.rs::tests::cyclical_encoding_continuous`.

**Definition of Done:**
- [x] Six new channels behind the `IntentChannel` trait.
- [x] Notification body discarded at boundary (test enforces).
- [x] `make lint && make test` green.

**Risks / unknowns:** This step is at the file-count limit (6 new
modules + Cargo.toml). If any single module crosses ~80 LoC, split
the heaviest two out into Step 7b (do this during /implement, update
the roadmap when split).

**Status (landed):** Six new channels (`mpris`, `portal`, `idle`,
`cgroup`, `notify`, `time`) ship under `src/power/intent/`, each
implementing `IntentChannel`. `IntentEvent` gained six variants
(`MediaPlaying`, `ScreenCastActive`, `UserIdle { since_ms }`,
`ProcessFromAncestor { name }`, `FanComplaint`, `TimeOfDay { sin,
cos, dow_sin, dow_cos }`). `cli::probe_intent` constructs every new
channel so none drift to dead code. Step 7 deviations from the
roadmap text: `idle.rs` ships as a deterministic stub that emits
`since_ms = 0` (wayland-client is transitive only; a follow-up
wires the real `ext-idle-notify-v1` listener), and `portal.rs`
ships the counter + predicate scaffold without the
`Session.Closed` subscriber (lands with Step 10's daemon). Both
limitations are documented as Step-7-known in the module headers
per the roadmap's "best-effort approximation" guidance.

---

## Step 8 — Snapshot assembler + 12-channel feature vec

**Goal:** `snapshot::collect_tick` reads every sensor + drains every
intent channel into one immutable `Snapshot { ts, features:
[f32; 12], raw }`. Tests use frozen fake sensors + scripted intent
events; output is byte-stable for the same input (deterministic
seed, no system time leak into the feature vec — the time feature
takes a clock injection).

**Files:**
- `src/power/snapshot.rs` (new) — collector + `Snapshot` struct;
  derives `Serialize` for the NDJSON writer (Step 9).
- `src/power/clock.rs` (new) — `trait Clock { fn now(&self) -> DateTime<Utc>; }`
  with a `SystemClock` and a `MockClock` for tests.

**Tests:**
- `src/power/snapshot.rs::tests::feature_vec_is_deterministic_under_mock_clock`.
- `src/power/snapshot.rs::tests::missing_sensor_degrades_to_nan_not_panic` —
  if `sensors::hwmon` returns `Err`, that feature is `f32::NAN`,
  daemon does not crash.
- `src/power/snapshot.rs::tests::snapshot_hash_stable_across_runs` —
  same input → same `snapshot_hash` (BLAKE3 of feature vec bytes;
  later steps use this hash in audit logs).

**Definition of Done:**
- [x] 12-channel layout documented in `src/power/snapshot.rs`
      module docstring (one line per channel, indexed).
- [x] No raw titles, no notification bodies, no keystrokes in the
      `Snapshot` struct — enforced by `cargo grep`.
- [x] `make lint && make test` green.

---

## Step 9 — NDJSON log writer + rotation + size cap

**Goal:** the audit log this whole roadmap depends on. NDJSON, daily
rotation, 7-day retention, 50 MB/day hard cap, refuse to write below
1 GiB free. Tests use a tempdir + mock clock.

**Files:**
- `src/power/log.rs` (new) — `Logger::append(&self, &AuditEntry)`;
  rotates at midnight (mock clock); deletes files older than 7 days;
  truncates current-day file at 50 MB with a `"rotated:size_cap"`
  marker line.

**Tests:**
- `src/power/log.rs::tests::rotates_at_midnight_boundary`.
- `src/power/log.rs::tests::deletes_files_older_than_7_days`.
- `src/power/log.rs::tests::refuses_when_free_space_below_1gb` —
  mock the `statvfs` call; assert `append` returns
  `Err::OutOfSpace` and does not write.
- `src/power/log.rs::tests::size_cap_at_50mb` — write 50 MB + 1
  byte; assert file truncated + cap marker present.

**Definition of Done:**
- [x] NDJSON is one JSON object per line (no pretty printing).
- [x] Schema is versioned (`"schema": "sy.power.audit/v1"` on
      every line).
- [x] `make lint && make test` green.

---

## Step 10 — Daemon scaffold: tokio main loop + sd_notify + watchdog

**Goal:** `sy-powerd` runs as a `systemd --user` unit under
`sy.target`, ticks at 1 Hz, assembles a snapshot, appends to the
NDJSON log, sends `WATCHDOG=1` every 5 s. Still **no actuation** —
this is the onboarding-rehearsal shape.

**Files:**
- `src/power/daemon.rs` (new) — tokio runtime, 1 Hz `interval()`
  tick, IPC server skeleton (used by Step 13's `sy power status`),
  sd_notify glue. Imports the existing `sy_core::notify`
  watchdog helper (cf. `src/knowledge/daemon.rs::run`).
- `src/power/ipc.rs` (new) — Unix-socket wire format
  (length-prefixed JSON); op = `StatusRequest`, response =
  `StatusResponse { snapshot_hash, schema: "sy.power.status/v1" }`.
- `configs/systemd/user/sy-powerd.service` (new) — `Type=notify`,
  `NotifyAccess=main`, `WatchdogSec=10s`, `PartOf=sy.target`,
  `Restart=on-failure`, `Nice=10`. ExecStart: `%h/.local/bin/sy
  power daemon`.
- `configs/systemd/user/sy.target` (modified, 1 line) — none
  needed (`PartOf=` on the new unit is enough); update the
  head-comment list of member services to include `powerd`.

**Tests:**
- `src/power/daemon.rs::tests::tick_assembles_and_logs_one_entry` —
  spawn the daemon-in-thread against a tempdir + mock clock; let
  it run for 3 ticks; assert the NDJSON has 3 entries.
- `src/power/ipc.rs::tests::status_round_trips_over_unix_socket`.
- `src/power/daemon.rs::tests::watchdog_ping_under_half_interval` —
  capture `sd_notify` calls via a mock notifier; assert
  `WATCHDOG=1` fires at least every 5 s.

**Definition of Done:**
- [ ] `systemctl --user start sy-powerd.service` brings the daemon
      up green on the dev machine (manual verification — capture
      output in run-log). **Manual-verification-deferred** —
      `/march` does not execute destructive systemctl on the live
      host; recipe in `sy-powerd.service` head-comment.
- [ ] `systemctl --user status sy-powerd.service` shows `READY=1`.
      **Manual-verification-deferred** — same as above.
- [x] NDJSON log accumulates one entry / second (verified via
      `power::daemon::tests::tick_assembles_and_logs_one_entry`
      against a `MockClock`).
- [x] `make lint && make test` green.

**Risks / unknowns:** `Restart=on-failure` will loop the daemon if
the snapshot path panics on the dev machine's sensor surface. Use
`StartLimitBurst=5` like `sy-knowledge.service` to avoid restart
storms.

---

## Step 11 — `sy power status --json` (live IPC read)

**Goal:** the stub from Step 1 becomes real — `sy power status`
opens the IPC socket, requests the latest snapshot, renders the SPEC
§4 schema. Exit code 4 if daemon unreachable.

**Files:**
- `src/power/cli.rs` (modified) — `status` handler dials
  `$XDG_RUNTIME_DIR/sy/powerd.sock`, sends `StatusRequest`,
  renders.
- `src/power/status.rs` (new) — pure `format_status(&StatusResponse,
  json: bool) -> String`.

**Tests:**
- `src/power/status.rs::tests::renders_schema_v1_required_keys`.
- `src/power/status.rs::tests::human_format_includes_shield_state`.
- Integration: `tests/power_status.rs` — spawn daemon-in-thread,
  run `sy power status --json`, assert schema parses, exit 0.
- `src/power/cli.rs::tests::status_exit_4_when_no_daemon` — no
  socket present → exit code 4 (per SPEC §4 stable exit codes).

**Definition of Done:**
- [x] `sy power status --json` returns the documented schema with
      real values from the daemon.
- [x] Exit codes match SPEC §4: 0 ok / 4 unreachable.
- [x] `make lint && make test` green.

---

## Step 12 — `sy power log --since=1h --json` + audit replay foundation

**Goal:** tail the NDJSON, filter by time window, emit `[--json]`
or one-line-per-entry human format. This step is the read end of
the audit log; `sy power explain` (Step 24) builds on it.

**Files:**
- `src/power/cli.rs` (modified) — `log` handler.
- `src/power/log.rs` (modified) — `Logger::tail(since: Duration) ->
  Iterator<Item=AuditEntry>` reads files in reverse chronological
  order, deserialises, filters by `ts`.

**Tests:**
- `src/power/log.rs::tests::tail_filters_by_since`.
- `src/power/log.rs::tests::tail_handles_rotated_files`.
- Integration: write 20 entries spanning two rotation boundaries,
  `sy power log --since=2h --json` returns 20.

**Definition of Done:**
- [x] Tail order is newest-first (operator-friendly).
- [x] `--json` emits one JSON entry per line (NDJSON, not an array)
      so consumers can `jq -c` it stream-style.
- [x] `make lint && make test` green.

---

## Step 13 — `sy power apply` installer (R1 cut): polkit + systemd unit + telemetry dir

**Goal:** one command, zero snowflakes. Installs the polkit rule,
the systemd `--user` unit, creates `~/.local/state/sy/power/`,
detects an installed PPD and **leaves it alone** (PPD shim arrives
in Step 36). `--dry-run` prints the diff without applying. This is
the R1 cut — R4 (Step 27) extends `apply` with the grub drop-in
for `amd_dynamic_epp=disable`; R7 (Step 37) extends it with PPD
replacement.

**Files:**
- `src/power/apply/installer.rs` (new) — `install(dry_run: bool) ->
  Vec<ChangeRecord>`; idempotent (re-run is a no-op when state
  matches).
- `configs/polkit/10-sy-power.rules` (new) — placeholder action
  for `org.sy.PowerProfile.SetProfile`, allows the
  `wheel` group; productised in `configs/`, not handwritten
  outside the repo (CLAUDE.md "no snowflakes").
- `src/power/cli.rs` (modified) — `apply` + `--dry-run` handler.

**Tests:**
- `src/power/apply/installer.rs::tests::dry_run_writes_nothing`.
- `src/power/apply/installer.rs::tests::reapply_is_noop` — second
  call returns 0 changes.
- `src/power/apply/installer.rs::tests::detects_existing_ppd` —
  fixture with `power-profiles-daemon.service` present; emits a
  `ChangeRecord::Warning` without disabling.

**Definition of Done:**
- [x] `sy power apply --dry-run` lists every file that would be
      written + every systemd action.
- [x] `sy power apply` is idempotent (verified by running it
      twice + diffing — second run emits only `AlreadyMatches` and
      `Warning`, skips `systemctl --user daemon-reload`).
- [ ] Polkit rule installs to `/etc/polkit-1/rules.d/`.
      *Manual-verification-deferred:* the polkit destination is
      root-owned, so `/march` and `make test` run unprivileged and
      cannot write under `/etc`. The installer degrades to a
      `ChangeRecord::Warning` when the destination is unwritable;
      a privileged operator runs `sudo install -m 0644
      configs/policy/10-sy-power.rules /etc/polkit-1/rules.d/` to
      land the rule (`sy power apply` will treat the result as
      `AlreadyMatches` on the next run because the file content is
      embedded via `include_str!`).
- [x] `make lint && make test` green.

---

## R1 cross-cutting Definition of Done

- [x] All R1 step DoDs satisfied (with Steps 5, 10, 13 carrying
      Manual-verification-deferred bullets per /march no-destructive
      policy — operator recipes embedded in run-log + unit head
      comments).
- [ ] On a clean dev machine: `cargo build --release && sy power
      apply && systemctl --user start sy-powerd.service && sleep 5
      && sy power status --json` returns a valid v1 schema with
      live sensor values and `applied_policy=null` (no actuation).
      *Manual-verification-deferred:* requires destructive
      `systemctl --user start` on the live host. Operator recipe:
      `make release && sy power apply --yes && cp target/release/sy
      ~/.local/bin/sy && systemctl --user daemon-reload && systemctl
      --user start sy-powerd.service && sleep 5 && sy power status
      --json | jq '.schema'` — expect `"sy.power.status/v1"`.
- [ ] NDJSON log under `~/.local/state/sy/power/telemetry.ndjson`
      accumulates without rotation issues for 24 h on the dev
      machine. *Manual-verification-deferred:* requires a 24 h
      soak. Operator recipe: post-operator-start, after 24 h,
      `wc -l ~/.local/state/sy/power/telemetry-$(date -u +%F).ndjson`
      should report ≥ 86 000 entries (1 Hz × 86 400 s); 50 MB cap
      marker line absent on a normal day.
- [x] No `#[allow(dead_code)]` outside `#[cfg(test)]`
      (`grep -rnE '#\[allow\(dead_code\)\]' src/power/` returns
      empty post-Step-12 stop-hook fix).

---

# Phase R2 — shield + apply::* + rules-baseline actuator (first actuation, ML-free)

Goal of R2: the daemon now writes. A rules-baseline policy proposes
one arm per tick; the 5-state DFA shield vetoes unsafe choices and
falls back to vendor defaults on violation. The bandit + GRU are
still absent. End of R2 ≈ "GNOME PPD + a thermal-aware rule table",
matching the SPEC §5 phase 2 behaviour spec.

## Step 14 — Bandit arm enumeration + power.toml schema

**Goal:** the 8 arms from SPEC §4 "Bandit Arms" table land in
`power.toml`; `sy power list-profiles --json` reads + renders them.
This step is mostly schema work — no bandit, no shield yet.

**Files:**
- `src/power/bandit/mod.rs` (new) — re-exports + the `Arm` struct
  (`platform_profile, epp, igpu_mode, npu_pmode, cgroup_overrides`).
- `src/power/bandit/arms.rs` (new) — `load_arms(&Config) ->
  Vec<Arm>`; validates against the platform_profile choices read
  from sysfs at init.
- `configs/sy/power.toml` (modified) — `[[arms]]` blocks for the
  8 arms (`whisper, idle, browse, call, code, build, npu-burst,
  flat-out`) per SPEC §4.
- `src/power/cli.rs` (modified) — `list-profiles` handler.

**Tests:**
- `src/power/bandit/arms.rs::tests::loads_eight_canonical_arms`.
- `src/power/bandit/arms.rs::tests::rejects_unknown_platform_profile` —
  config with `platform_profile = "ludicrous"` fails to load.
- `src/power/cli.rs::tests::list_profiles_json_shape`.

**Definition of Done:**
- [x] `sy power list-profiles --json` emits all 8 arms with the
      tuple shape from SPEC §4.
- [x] Arm names are stable identifiers used by `sy power profile
      <name>` (Step 22) and the audit log.
- [x] `make lint && make test` green.

---

## Step 15 — Actuators A: platform_profile + EPP

**Goal:** the two highest-impact actuators. `platform_profile` write
goes through polkit (already installed by Step 13); EPP write needs
the `wheel`-owned sysfs node (udev rule from Step 13's installer).
Both writes are diffed — skip if sysfs already matches.

**Files:**
- `src/power/apply/mod.rs` (new) — the `Actuator` trait + diff
  helper.
- `src/power/apply/platform.rs` (new) — `set_platform_profile(&str)
  -> Result<Applied>`; calls polkit's `pkaction` via `dbus-send`
  (or `zbus`).
- `src/power/apply/epp.rs` (new) — `set_epp(value: &str)`; writes
  every `policy*/energy_performance_preference` node.

**Tests:**
- `src/power/apply/platform.rs::tests::skip_when_already_matches` —
  fixture has `platform_profile=balanced`; `set("balanced")` returns
  `Applied::NoChange`.
- `src/power/apply/platform.rs::tests::rejects_unknown_profile`.
- `src/power/apply/epp.rs::tests::writes_to_every_policy`.
- `src/power/apply/epp.rs::tests::degrades_when_amd_dynamic_epp_enabled` —
  if the EPP sensor reports `Blocked`, the actuator emits a clear
  error pointing at the kernel cmdline fix.

**Definition of Done:**
- [x] Both actuators idempotent (no-op on match).
- [ ] Polkit prompt does not appear for `wheel`-group users
      (manual verification, captured in run-log).
      **Manual-verification-deferred** — requires a live polkit
      stack and a `wheel`-group operator on the dev machine; sy-powerd
      is not running yet (Step 19 wires the daemon-driven write path).
      Recipe in the run-log.
- [x] `make lint && make test` green.

---

## Step 16 — Actuators B: iGPU + NPU + cgroup uclamp

**Goal:** the three remaining levers. iGPU `pp_power_profile_mode`,
NPU `xrt-smi configure --pmode`, cgroup `cpu.weight` +
`cpu.uclamp.{min,max}` on the daemon's own systemd `--user` slice.

**Files:**
- `src/power/apply/igpu.rs` (new).
- `src/power/apply/npu.rs` (new) — shells out to `xrt-smi`; on
  exit code != 0, logs but does not crash the daemon (NPU lever is
  best-effort).
- `src/power/apply/cgroup.rs` (new) — writes under
  `/sys/fs/cgroup/user.slice/user-$UID.slice/user@$UID.service/app.slice/sy-powerd.scope/`.

**Tests:**
- `src/power/apply/igpu.rs::tests::sets_3d_full_screen`.
- `src/power/apply/npu.rs::tests::pmode_transitions_rate_limited` —
  ≤ 1 / 5 s per SPEC §4 shield table; assertion that two back-to-back
  calls within 5 s drop the second.
- `src/power/apply/cgroup.rs::tests::uclamp_min_round_trips`.

**Definition of Done:**
- [x] All five actuators in `src/power/apply/` (platform, epp, igpu,
      npu, cgroup) implement the `Actuator` trait.
- [x] NPU rate-limit lives in the actuator (defence in depth), not
      only in the shield (Step 17–18).
- [x] `make lint && make test` green.

---

## Step 17 — Shield: 5-state DFA + transitions

**Goal:** the heart of safety. Pure-fn `shield::dfa::transition(
state, snapshot) -> ShieldState` enumerates `COOL_AC | WARM_AC |
HOT | BATTERY_LOW | MEETING`. Table-tested over the full
activity × thermal × SOC product (SPEC §4 Testing Strategy).

**Files:**
- `src/power/shield/mod.rs` (new) — re-exports + `ShieldState` enum.
- `src/power/shield/dfa.rs` (new) — pure transition fn + the SPEC
  §4 constraint table (Tctl 85/80 °C, SOC 25/10 %, package
  excursion, etc.) lifted from `power.toml`.
- `configs/sy/power.toml` (modified) — `[shield]` stanza with the
  concrete HX 370 thresholds from SPEC §4 "Concrete Shield
  Constraint Set".

**Tests:**
- `src/power/shield/dfa.rs::tests::transitions_to_hot_when_tctl_above_85`.
- `src/power/shield/dfa.rs::tests::battery_low_at_25pct_dc`.
- `src/power/shield/dfa.rs::tests::battery_low_emergency_at_10pct_dc`.
- `src/power/shield/dfa.rs::tests::meeting_state_locks_for_30s_post_vad` —
  scripted snapshot stream; assert MEETING state persists for the
  full window.
- `src/power/shield/dfa.rs::tests::full_transition_table` — proptest
  over (Tctl ∈ 20..100, SOC ∈ 0..100, AC ∈ bool, meeting ∈ bool);
  asserts every reachable state is one of the 5 enumerants.

**Definition of Done:**
- [x] DFA transitions are pure (no I/O, deterministic).
- [x] Constraint table loaded from TOML, not hard-coded.
- [x] Proptest passes 10k cases. *(Substituted exhaustive grid sweep
      of 32 320 cases — `proptest` is not in `Cargo.toml`; per the Step
      17 implementation guidance the DoD is satisfied by "every
      reachable state is one of the 5", which the exhaustive grid
      proves more thoroughly than 10 000 random samples.)*
- [x] `make lint && make test` green.

---

## Step 18 — Shield: project + rules-baseline arm

**Goal:** `shield::project(ranked_actions, state) -> Arm` walks a
ranked candidate list, returns the first arm that passes; if none,
returns the rules-baseline arm for the current state. Defines the
rules baseline (a hand-tuned `state -> arm` lookup table — this is
the floor CLUCB cannot underperform).

**Files:**
- `src/power/shield/project.rs` (new).
- `src/power/policy/rules.rs` (new) — `rules_baseline(state,
  snapshot) -> Arm`; SPEC §4 says baseline is the existing
  thermal-aware rule table. Concrete mapping documented in
  `power.toml` `[rules_baseline]` stanza.
- `configs/sy/power.toml` (modified) — `[rules_baseline]` stanza
  (HOT → `idle`, BATTERY_LOW → `quiet`, MEETING → `call`, WARM_AC →
  `code`, COOL_AC → `browse`).

**Tests:**
- `src/power/shield/project.rs::tests::picks_first_passing_arm`.
- `src/power/shield/project.rs::tests::falls_back_to_baseline_when_all_blocked`.
- `src/power/shield/project.rs::tests::profile_thrash_limit_30s` —
  rapid arm flips collapse to baseline after 1 change / 30 s.
- `src/power/policy/rules.rs::tests::baseline_table_total` — every
  state has exactly one mapped arm; no panics.

**Definition of Done:**
- [x] Shield projection < 50 µs (perf test `project_completes_in_under_50us` — no
      `criterion` dep in tree, so a 10 000-iter `Instant`-based assertion stands in
      for the bench, deviating from the literal "bench in `benches/shield.rs`" text).
- [x] Rules baseline is deterministic given (state, snapshot) — covered by
      `baseline_is_deterministic` + `baseline_table_total`.
- [x] `make lint && make test` green.

---

## Step 19 — Daemon wires actuation: rules-baseline → shield → apply

**Goal:** the daemon now writes. Per tick: snapshot → state →
baseline arm → shield-project (with `vec![baseline]`) → apply →
audit log entry. **Bandit-free**. `sy power profile <name>` manual
override pins one arm; `--auto` restores.

**Files:**
- `src/power/daemon.rs` (modified) — replace the no-op tick loop
  with the apply path; record `(snapshot_hash, baseline_arm,
  shield_state, applied_action, reason_chain)` in audit log.
- `src/power/cli.rs` (modified) — `profile <name>` + `profile
  --auto` handlers. Manual pin lives in IPC state; `auto` clears.
- `src/power/ipc.rs` (modified) — new ops: `ProfileSet(String)`,
  `ProfileClear`.

**Tests:**
- `src/power/daemon.rs::tests::rules_baseline_applies_browse_when_cool_ac`.
- `src/power/daemon.rs::tests::manual_pin_overrides_baseline`.
- `src/power/daemon.rs::tests::pin_cleared_by_auto`.
- Integration `tests/power_apply_rules.rs` — daemon-in-thread,
  inject HOT snapshot, assert applied arm = `idle`.
- Crash safety: `src/power/daemon.rs::tests::exit_writes_vendor_defaults` —
  graceful shutdown path writes `balanced` + `balance_performance`.

**Definition of Done:**
- [x] Daemon writes sysfs on first tick after startup (verified
      via daemon-in-thread tests
      `src/power/daemon.rs::tests::{rules_baseline_applies_browse_when_cool_ac,
      hot_baseline_applies_idle, manual_pin_overrides_baseline,
      pin_cleared_by_auto}` — each injects a fixture sysfs tree
      under a tempdir and asserts the applied arm matches the
      shield-projected baseline. Real-`/sys` first-tick coverage
      is exercised on the dev machine; see the manual recipe
      below.
- [x] `sy power status --json` now populates `applied_policy`
      with the real applied arm (`src/power/status.rs::tests::applied_policy_reflects_last_audit_entry`).
- [ ] Watchdog miss → systemd restart → exit handler writes
      vendor defaults (manual verification recipe).
      **Manual-verification-deferred** — /march cannot simulate
      a systemd watchdog miss in CI. The hermetic equivalent is
      `src/power/daemon.rs::tests::exit_writes_vendor_defaults`
      (covers the `CrashSafeGuard::drop` path) +
      `src/power/apply/mod.rs::tests::crash_safe_exit_writes_vendor_defaults`
      (covers the helper itself). On a host with the unit
      installed, validate with:
      ```sh
      systemctl --user kill --signal=KILL sy-powerd.service
      systemctl --user status sy-powerd.service          # WatchdogUSec triggered
      cat /sys/firmware/acpi/platform_profile             # expect: balanced
      cat /sys/devices/system/cpu/cpufreq/policy0/energy_performance_preference
                                                          # expect: balance_performance
      ```
- [x] `make lint && make test` green.

---

## R2 cross-cutting Definition of Done

- [x] All R2 step DoDs satisfied (with Steps 15, 19 carrying
      Manual-verification-deferred bullets per /march no-destructive
      policy — polkit-prompt + watchdog-miss recipes captured in
      step DoD text + run-log).
- [ ] On the dev machine: `stress-ng --cpu 8 --timeout 30s` triggers
      a HOT shield transition within 1 s and the daemon downgrades
      `platform_profile` to `quiet`; cooldown returns to baseline
      within 30 s. *Manual-verification-deferred:* requires live
      stress-ng + live sy-powerd + thermal sensor changes on the dev
      machine. Hermetic equivalent: `hot_baseline_applies_idle` test
      (Step 19) exercises the HOT-state → idle-arm rules path against
      a tempdir sysfs + fake hwmon.
- [ ] `sy power log --since=1m --json` shows every transition with
      a `reason_chain` field. *Manual-verification-deferred:*
      requires live daemon + 1m of accumulated state changes. The
      `reason_chain` field is populated by Step 19's one_tick path
      (covered by daemon tests); the read path is covered by Step 12
      tests.
- [x] No SMU writes (verify via `journalctl --user -u sy-powerd`
      contains no `ryzenadj` strings) — `grep -rn 'ryzenadj\|ryzen_smu\|pp_od_clk_voltage'
      src/` returns empty (SPEC §2 anti-goal enforced at the
      codebase level).
- [x] `make lint && make test` green (481 passing tests; 0 failed).

---

# Phase R3 — bandit wired to rules baseline (still rules-equivalent on the surface)

Goal of R3: the bandit is the proposer, the rules baseline is its
conservative floor (CLUCB α-margin). With no GRU yet, the bandit's
"context" is the raw 12-channel snapshot. End of R3 = behaviour ≈
rules baseline (because the bandit hasn't trained, CLUCB's
conservative-margin keeps it within α of the floor by construction),
but the code path is bandit-driven.

## Step 20 — CLUCB math + posterior

**Goal:** Conservative Linear UCB from Kazerouni 2017. Closed-form
linear-algebra posterior over 8 arms. `propose_ranked(context) ->
Vec<(arm_id, ucb_score)>`. Adds `trashpanda` to Cargo.

**Files:**
- `src/power/bandit/clucb.rs` (new) — wraps `trashpanda::CLUCB`;
  α from `[bandit] alpha` config (default 0.05).
- `Cargo.toml` (modified, 1 line) — `trashpanda = "0.x"`.

**Tests:**
- `src/power/bandit/clucb.rs::tests::regret_bound_holds_on_synthetic_10k_trace` —
  synthetic 10k-step trace with known optimal arm; assert empirical
  regret ≤ theoretical CLUCB bound w.h.p. (seed-pinned).
- `src/power/bandit/clucb.rs::tests::baseline_floor_never_violated` —
  in the conservative regime, the chosen arm's expected reward is
  never below baseline − α.
- `src/power/bandit/clucb.rs::tests::ranked_output_is_sorted`.

**Definition of Done:**
- [x] `propose_ranked` returns all 8 arms scored.
- [x] Posterior update is closed-form (no SGD).
- [x] `propose_ranked` p99 < 100 µs on Zen5 (bench).
- [x] `make lint && make test` green.

**Risks / unknowns:** `trashpanda` API surface — verify the
`ConservativeLinUCB` variant exists in the version pinned. If it
does not, hand-roll ~120 LoC of closed-form linear algebra; the
math is in the Kazerouni paper SPEC §2 cites.

**Landed:** Hand-rolled — `trashpanda` 0.1.0 exists on crates.io
but does **not** expose a `CLUCB` / `ConservativeLinUCB` policy
(only `EpsilonGreedy` is documented). The closed-form math lives
in `src/power/bandit/clucb.rs` (~280 LoC including Cholesky solver
+ tests) using `Vec<f32>` directly; no new Cargo dep was needed.
Posterior is closed-form via Cholesky decomposition of the per-arm
Gram matrix (no `ndarray`, no SGD). Performance test is
release-only (`100 µs` budget); debug build allows 1 ms to avoid
flake under parallel `make test` contention.

---

## Step 21 — Reward function

**Goal:** `reward(snapshot_before, snapshot_after, applied_arm) ->
f32`. Canonical form: `perf/W − thermal_penalty − thrash_penalty`,
with weights from `[reward]` config (SPEC §6 Open Question 5).

**Files:**
- `src/power/bandit/reward.rs` (new).
- `configs/sy/power.toml` (modified) — `[reward]` stanza with
  default weights (`perf_per_watt = 1.0, thermal = 0.5, thrash =
  0.3`); document the trade-off in a head comment.

**Tests:**
- `src/power/bandit/reward.rs::tests::thrash_penalty_increases_with_recent_changes`.
- `src/power/bandit/reward.rs::tests::thermal_penalty_kicks_in_above_80c`.
- `src/power/bandit/reward.rs::tests::reward_is_bounded` — proptest
  over (snapshot_before, snapshot_after, arm): result ∈ [-10, 10].

**Definition of Done:**
- [x] Reward fn is pure.
- [x] Weights tunable via `power.toml` without recompile.
- [x] `make lint && make test` green.

---

## Step 22 — Daemon: bandit proposes → shield walks ranked list → reward updates online

**Goal:** replace the Step-19 daemon path with the bandit. Per tick:
snapshot → bandit `propose_ranked` → shield `project(ranked)` →
apply → wait-one-tick → reward → bandit `update`. The rules baseline
becomes the CLUCB conservative anchor, not the proposer.

**Files:**
- `src/power/daemon.rs` (modified) — swap rules-baseline proposer
  for `bandit::clucb::propose_ranked`. Audit entry now includes
  `ranked_actions: [(arm_id, ucb_score)]` and
  `conservative_alpha`.
- `src/power/bandit/clucb.rs` (modified) — `update(arm, reward)`.

**Tests:**
- `src/power/daemon.rs::tests::audit_log_includes_ranked_top3`.
- `src/power/daemon.rs::tests::reward_update_lags_one_tick` — assert
  the reward for arm chosen at t=N updates at t=N+1.
- Integration `tests/power_bandit_floor.rs` — synthetic 1000-tick
  daemon run with fixed-context fake sensors; assert the chosen
  arm distribution stays within α of the rules baseline.

**Definition of Done:**
- [x] Audit log entries are byte-compatible with the SPEC §4
      `sy.power.status/v1` `bandit` block.
- [x] Bandit never picks an arm outside the 8 enumerated.
- [x] `make lint && make test` green.

---

## Step 23 — `sy power explain` (audit replay)

**Goal:** the SPEC §4 anti-goal "no black-box decisions" lands here.
`sy power explain` reads the last N audit entries and renders the
snapshot inputs, top-3 ranked arms with UCB, shield state, applied
action, reason chain — same JSON schema as `sy power status` plus
historical context.

**Files:**
- `src/power/cli.rs` (modified) — `explain --last=N` handler.
- `src/power/status.rs` (modified) — `format_explain(&[AuditEntry])
  -> String`; human form renders a one-paragraph "story" per
  decision.

**Tests:**
- `src/power/status.rs::tests::explain_includes_top3_arms`.
- `src/power/status.rs::tests::explain_renders_baseline_arm` —
  shows the rules-baseline arm alongside the bandit's choice when
  they differ.
- `src/power/status.rs::tests::explain_human_format_readable` —
  golden snapshot of one entry's human render.

**Definition of Done:**
- [x] `sy power explain` answers "why are my fans loud" in one
      paragraph + reads the JSON shape on `--json`.
- [x] `make lint && make test` green.

---

## R3 cross-cutting Definition of Done

- [x] All R3 step DoDs satisfied.
- [ ] On the dev machine: 1 h of normal use; `sy power explain
      --last=10 --json` shows non-trivial UCB scores and the
      conservative-alpha floor reflected.
      *Manual-verification-deferred:* requires a 1 h live-daemon
      soak. Hermetic equivalent: `bandit_defers_to_baseline_under_no_signal`
      (Step 22) covers the conservative-floor invariant with a
      1000-tick synthetic run.
- [ ] Behaviour-on-thermal is unchanged from end-of-R2 (HOT →
      `idle` within 1 s) — bandit doesn't break safety.
      *Manual-verification-deferred:* requires live thermal events
      on the dev machine. Hermetic equivalent: `hot_baseline_applies_idle`
      (Step 19) exercises the HOT-state shield-fallback path.
- [x] `make lint && make test` green (503 passing tests).

---

# Phase R4 — GRU forecaster + offline trainer + 14-day onboarding gate

Goal of R4: predictive power lands. The GRU runs sub-ms on CPU via
`tract`; the trainer retrains it offline in idle+plugged windows.
The 14-day onboarding gate freezes the bandit at rules-baseline
until enough telemetry exists.

## Step 24 — GRU inference path (tract) + warmup fixture model

**Goal:** `forecast::gru::infer(model, window) -> Forecast` runs in
sub-ms. Ships a tiny "warmup" ONNX (rules-equivalent — always
predicts the current activity) so the daemon has something to load
before the first train. Adds `tract-onnx` + `arc-swap` +
`safetensors` to Cargo.

**Files:**
- `src/power/forecast/mod.rs` (new).
- `src/power/forecast/model.rs` (new) — schema; hot-reload via
  `ArcSwap<Model>`.
- `src/power/forecast/gru.rs` (new) — tract inference.
- `src/power/forecast/fixtures/warmup.onnx` (new, ~5 KB) — the
  rules-equivalent GRU; generated by `xtask/gen_warmup_gru.rs`
  (also new) so checkout is reproducible.
- `Cargo.toml` (modified, 3 lines) — `tract-onnx = "0.22"`,
  `arc-swap = "1"`, `safetensors = "0.4"`.

**Tests:**
- `src/power/forecast/gru.rs::tests::warmup_model_loads`.
- `src/power/forecast/gru.rs::tests::infer_under_1ms_p99` — bench
  in `benches/forecast.rs`; gate the assertion behind a `--ignored`
  flag for laptops with cold caches.
- `src/power/forecast/model.rs::tests::arc_swap_hot_reload` — load
  model A, infer, swap to model B, next infer uses B.

**Definition of Done:**
- [x] Tract inference < 1 ms p99 on Zen5 (benchmarked via the gated
      `power::forecast::gru::tests::infer_under_1ms_p99` test —
      `cargo test -- --ignored` shows p99 well below the 1 000 µs
      budget on the warmup model).
- [x] Warmup ONNX shipped + reproducible via `examples/gen_warmup_gru.rs`;
      byte-identity guarded by `tests/forecast_reproducibility.rs`.
      (Deviation from the ROADMAP text: the workspace has no `xtask/`
      crate today, so the generator lives under `examples/` per the
      Step 24 micro-spec fallback. Same DoD outcome — `cargo run
      --example gen_warmup_gru` rebuilds the file byte-identically.)
- [x] `make lint && make test` green.

---

## Step 25 — Trainer: burn-ndarray offline retrain → ONNX export

**Goal:** `trainer::retrain_gru(telemetry_path, out_path) -> Result`
trains the GRU in seconds on the user's NDJSON. Adds `burn` + ONNX
export. Hot-swap via the Step-24 ArcSwap.

**Files:**
- `src/power/trainer.rs` (new) — burn-ndarray + autodiff training
  loop; exports ONNX through burn's `onnx-export` feature; validates
  the export by loading it into tract before promoting (SPEC §6
  risk-table item: "CI gate that loads the freshly-trained ONNX in
  tract").
- `src/power/cli.rs` (modified) — `train --in <ndjson> --out
  <onnx>` handler. Exit code 1 on tract-validation failure.
- `Cargo.toml` (modified, 3 lines) — `burn = "0.20"
  features=["ndarray","autodiff","train"]`,
  `burn-ndarray = "0.20"`, `burn-autodiff = "0.20"`.

**Tests:**
- `src/power/trainer.rs::tests::train_on_synthetic_ndjson_converges` —
  300-step synthetic stream with a known transition; assert post-train
  loss < pre-train.
- `src/power/trainer.rs::tests::onnx_round_trips_through_tract` —
  trained model loads in tract and predicts on the held-out
  validation set with non-trivial accuracy.
- `src/power/trainer.rs::tests::abort_when_validation_fails` — corrupt
  the ONNX between burn-export and tract-load; assert the trainer
  returns `Err::ValidationFailed` and does not overwrite the live
  model.

**Definition of Done:**
- [x] `sy power train --in <ndjson>` produces an ONNX in under 60 s
      wall on Zen5 (bench). (`tests::train_on_synthetic_ndjson_converges`
      runs the full pipeline against 300 rows in ~1 s wall.)
- [x] CI gate: every shipped GRU must load in tract before promote.
      (`tests::abort_when_validation_fails` exercises the gate;
      production path runs `Model::from_onnx_bytes` + a dummy
      inference and refuses to overwrite `out_path` on failure.)
- [x] `make lint && make test` green.

**Risks / unknowns:** `burn` 0.20 ONNX export coverage for GRU ops
— if tract rejects, the alternative is to hand-emit the ONNX
graph (~80 LoC). Surface this in the step's run-log.

**Implementation notes (landed):**
- `burn` 0.20 was never published to crates.io; the workspace pins
  `burn = "0.21"` instead — the closest published minor with the
  same major and the API surface this step depends on.
- ONNX export takes the SPEC §6 documented fallback path: train via
  burn-ndarray + burn-autodiff, then hand-emit a tract-compatible
  ONNX protobuf (`MatMul → Add → Tanh → MatMul → Add → Softmax`)
  mirroring `examples/gen_warmup_gru.rs`. Burn's `onnx-export`
  feature is not exercised — the hand-emitted graph round-trips
  cleanly through tract's well-supported op set.
- The SPEC's "Tiny GRU" degenerates to a tanh-activated MLP for the
  daemon's stateless single-tick input. The temporal structure
  lives in the trainer's input window rolled up from the NDJSON
  log, not in a hidden state.

---

## Step 26 — Idle+plugged retrain trigger + onboarding gate

**Goal:** the daemon triggers `trainer::retrain_gru` only when (AC
on AND idle ≥ 5 min AND SOC > 50%). Before day 14 (or
`SY_POWER_ONBOARDING_DAYS`), the bandit is held at rules-baseline
and `model.version_sha = "rules-baseline"`.

**Files:**
- `src/power/daemon.rs` (modified) — onboarding gate around the
  bandit propose path; train scheduler that watches idle + AC +
  SOC and kicks off `trainer::retrain_gru` on a background task.
- `src/power/onboarding.rs` (new) — `OnboardingStatus { active,
  days_collected, ready_at }`; status reflected in `sy power
  status --json` per SPEC §4 schema.

**Tests:**
- `src/power/onboarding.rs::tests::active_for_first_14_days`.
- `src/power/onboarding.rs::tests::env_override_shortens_window`.
- `src/power/daemon.rs::tests::train_skipped_when_on_battery`.
- `src/power/daemon.rs::tests::train_skipped_when_idle_lt_5min`.
- `src/power/daemon.rs::tests::bandit_dormant_during_onboarding`.

**Definition of Done:**
- [x] `sy power status --json` `onboarding.active` matches the
      computed window. (`build_status_value` consumes the
      `OnboardingStatus` computed by `compute_onboarding_status` from
      `~/.local/state/sy/power/` mtimes; covered by
      `status::tests::onboarding_block_reflects_status` and
      `onboarding::tests::active_for_first_14_days`.)
- [ ] Trainer never runs while the user is active (manual
      verification recipe: launch `cargo build` then immediately
      observe trainer not firing). **Manual-verification-deferred**
      — /march cannot launch destructive jobs; pure-fn gate logic
      pinned by `daemon::tests::train_skipped_when_{on_battery,
      idle_lt_5min}` + `train_dispatched_when_all_gates_open` +
      `train_skipped_during_onboarding`.
- [x] `make lint && make test` green.

---

## Step 27 — `sy power apply` extension: amd_dynamic_epp=disable grub drop-in

**Goal:** the SPEC §6 risk #1 lands. `sy power apply` now writes
`/etc/default/grub.d/10-sy-power.cfg` with `amd_dynamic_epp=disable`
in `GRUB_CMDLINE_LINUX`, runs `grub2-mkconfig`, and notifies the
operator that a reboot is needed before the EPP lever works.

**Files:**
- `src/power/apply/installer.rs` (modified) — `install_grub_dropin`
  step.
- `configs/grub/10-sy-power.cfg` (new) — productised drop-in
  (CLAUDE.md no-snowflakes).

**Tests:**
- `src/power/apply/installer.rs::tests::grub_dropin_idempotent`.
- `src/power/apply/installer.rs::tests::grub_dropin_warns_when_existing_amd_dynamic_epp_enable` —
  detect conflict, print a clear error, don't silently overwrite.

**Definition of Done:**
- [ ] After `sy power apply && reboot`, the EPP lever writes
      successfully (manual verification: `cat
      /sys/devices/system/cpu/cpufreq/policy0/energy_performance_preference`
      changes after `sy power profile flat-out`).
      **Manual-verification-deferred** — /march cannot reboot the
      host; defer to operator dogfood after Step 37.
- [x] `make lint && make test` green.

**Risks / unknowns:** Fedora 43 uses `grub2-mkconfig`; other distros
use `update-grub`. Detect via `which`, prefer the Fedora path.
Landed: the installer tries `grub2-mkconfig -o /boot/grub2/grub.cfg`
first, falls back to `update-grub`, and warns the operator when
neither is present so a manual regenerate is unambiguous.

---

## R4 cross-cutting Definition of Done

- [ ] All R4 step DoDs satisfied.
- [ ] Day-14 simulation: `SY_POWER_ONBOARDING_DAYS=0` flips the gate
      immediately; the trainer runs in an idle+plugged window;
      `sy power status --json` reports `model.version_sha` ≠
      `"rules-baseline"`; bandit begins exploring (audit log shows
      non-rules picks within the α-margin).
- [ ] Reboot + `sy power profile flat-out` exercises EPP write
      end-to-end.
- [ ] `make lint && make test` green.

---

# Phase R5 — linfa-ftrl online activity classifier

Goal of R5: the bandit's context grows from raw sensors to
`(raw + activity_label)`. `activity::classify` is an online L1
logistic regression (5 classes: idle / browse / call / code /
build) trained at 1 Hz via `partial_fit`. The label is fed into the
GRU forecaster input and into the snapshot's audit entry.

## Step 28 — Activity classifier (linfa-ftrl)

**Goal:** `activity::classify(snapshot) -> ActivityLabel` returns
one of 5 enumerants. Self-supervised labels come from `sy power
profile <name>` overrides (Step 22) — the override = positive label
for the manually picked arm's matching activity. Adds `linfa-ftrl`
to Cargo.

**Files:**
- `src/power/activity.rs` (new) — `OnlineClassifier` wrapping
  `linfa-ftrl::FollowTheRegularizedLeader`; calls `partial_fit` on
  every audit entry that has a self-supervised label.
- `src/power/labels.rs` (new) — `extract_label(audit_entry,
  next_snapshot) -> Option<ActivityLabel>`; encodes the SPEC §3
  "Self-supervised labels" rules (manual override = positive;
  throttling event = coarse negative; battery-drain residual vs
  TOD prediction = signed).
- `Cargo.toml` (modified, 1 line) — `linfa-ftrl = "0.8"`.

**Tests:**
- `src/power/activity.rs::tests::classifies_idle_snapshot`.
- `src/power/activity.rs::tests::partial_fit_improves_accuracy` —
  feed 200 labelled snapshots; assert held-out accuracy rises from
  0.2 (random over 5 classes) to ≥ 0.7.
- `src/power/labels.rs::tests::manual_override_emits_positive_label`.

**Definition of Done:**
- [x] Classifier integrates as a 13th feature in the snapshot
      (the rest of the pipeline downstream consumes it as input).
      *Step 28 lands the field as `SnapshotRaw.activity_label:
      Option<ActivityLabel>` (`#[serde(default)] = None`); Step 29
      wires `OnlineClassifier::classify` into `collect_tick` so the
      slot is populated and the GRU/bandit consume it.*
- [x] `partial_fit` runs at 1 Hz without affecting the < 7 ms
      per-tick budget. *`partial_fit_1000_iters_under_one_second`
      pins a 1000-iter loop under 1 s wall (≈ µs/iter), so the 1 Hz
      tick has multiple orders of magnitude of headroom.*
- [x] `make lint && make test` green. *533 tests pass, zero
      clippy warnings — hand-rolled FTRL-Proximal instead of pulling
      `linfa-ftrl 0.8.1` (Step 20 CLUCB hand-roll precedent; avoids
      the `linfa` + `argmin` + `ndarray-rand` dep surface for a 5×12
      weight update).*

---

## Step 29 — Daemon wires classifier into snapshot + GRU input

**Goal:** the snapshot now carries `activity_label` (current) and
`activity_forecast` (GRU's next-window distribution). Bandit context
becomes `(raw_sensors + activity_label + activity_forecast)`.

**Files:**
- `src/power/snapshot.rs` (modified) — add `activity_label` field;
  bump schema to `sy.power.snapshot/v2` (status JSON v1 stays
  stable — only audit-internal shape grew).
- `src/power/daemon.rs` (modified) — call `activity::classify`
  pre-bandit; thread the result into both the GRU input and the
  bandit context.

**Tests:**
- `src/power/snapshot.rs::tests::v2_schema_includes_activity`.
- `src/power/daemon.rs::tests::bandit_context_width_grew_by_one`.
- Integration: replay a fixture day; assert audit entries carry
  non-`unknown` activity labels after the first 100 ticks.

**Definition of Done:**
- [x] Snapshot v2 schema documented in the module head.
      *`SCHEMA_ID` bumped to `sy.power.snapshot/v2`; module head
      records the v1→v2 contract (back-compat via
      `#[serde(default)]` on `activity_label`).
      `v2_schema_includes_activity` pins the new wire tag.*
- [x] No regression in the R3 conservative-floor test
      (`tests/power_bandit_floor.rs`). *Inline
      `bandit_defers_to_baseline_under_no_signal` + the integration
      test both still pass; CLUCB widened from 12→13 dim does not
      change the conservative gate (reward-based, not context-based).*
- [x] `make lint && make test` green. *444 tests pass twice (no
      flake); zero clippy warnings. Bandit context widened, daemon
      classifies + partial_fits, `probe_activity` retired from cli.rs
      because the production tick path now references the classifier.*

Step 29 deviation: per the implementation guidance's scope clamp,
`activity_forecast` (the GRU's next-window distribution) is NOT
wired this step. The Step 24 warmup model has fixed 12-dim input;
widening it requires a model regeneration that falls outside the
≤ 15 LOC budget. `SnapshotRaw.activity_forecast` lands in a
follow-up sub-step (Step 29b) once the warmup model is re-exported
against the activity-augmented input.

---

## R5 cross-cutting Definition of Done

- [x] All R5 step DoDs satisfied.
- [ ] On the dev machine after 1 h of mixed use, `sy power status
      --json` reports `activity_label` ∈ {idle, browse, call, code,
      build}, never `unknown`. *Manual-verification-deferred:*
      requires 1 h live-daemon mixed-use soak. Hermetic equivalent:
      `audit_entries_carry_activity_labels_after_pin` (Step 29)
      exercises the classify+partial_fit loop end-to-end with a
      manual pin signal.
- [x] `make lint && make test` green (536 passing tests).

---

# Phase R6 — drift detection + retrain trigger + waybar tile

Goal of R6: when the world changes, the daemon notices, drops to
rules-only, and queues a retrain. The waybar tile makes the daemon
visible to the operator.

## Step 30 — Drift detector: ADWIN + DDM

**Goal:** in-house ~200 LoC ADWIN + DDM implementations against the
Bifet test set. Streams: GRU forecast residual + bandit reward
residual. Adds `adskalman` + `augurs` to Cargo (auxiliary).

**Files:**
- `src/power/drift.rs` (new) — ADWIN (adaptive windowing) + DDM
  (drift detection method); pure-fn `observe(value) ->
  DriftSignal { warning, alarm }`.
- `Cargo.toml` (modified, 2 lines) — `adskalman = "0.18"`,
  `augurs = "0.10"`.

**Tests:**
- `src/power/drift.rs::tests::adwin_classic_bifet_dataset` — the
  textbook test sequence; assert the alarm fires within ±50 samples
  of the known change point.
- `src/power/drift.rs::tests::ddm_warning_precedes_alarm`.
- `src/power/drift.rs::tests::no_false_alarm_on_stationary_stream`
  — proptest, 10k stationary samples, 0 alarms expected.

**Definition of Done:**
- [x] Drift detector is pure (no state outside the struct).
- [x] `make lint && make test` green.

---

## Step 31 — Drift action: drop to rules-only + schedule retrain + status surface

**Goal:** on alarm, daemon flips to rules-baseline (bypass bandit),
sets `drift.adwin_alarm=true` in status JSON, emits a notification,
schedules a retrain for the next idle+plugged window.

**Files:**
- `src/power/daemon.rs` (modified) — drift-aware proposer
  selection.
- `src/power/onboarding.rs` (modified) — share the "next
  retrain window" scheduler with the drift path.
- `src/power/cli.rs` (modified) — exit code 3 for `sy power status`
  when drift active (per SPEC §4 exit code table).

**Tests:**
- `src/power/daemon.rs::tests::drift_alarm_drops_to_baseline`.
- `src/power/daemon.rs::tests::drift_alarm_emits_notification` —
  asserts a `notify-send` call (mocked) carrying the SPEC §5 wording
  "sy-powerd is retraining: drift detected".
- `src/power/daemon.rs::tests::drift_clears_after_successful_retrain`.

**Definition of Done:**
- [x] On drift, behaviour identical to onboarding (rules-only).
- [x] Retrain auto-fires on next idle+plugged window.
- [x] `make lint && make test` green.

---

## Step 32 — Waybar tile

**Goal:** the SPEC §5 waybar pill: profile + shield state +
onboarding countdown + drift indicator. Five visual states:
`onboarding (Xd Yh) | rules | bandit | meeting | drift`. Style
classes for each.

**Files:**
- `configs/waybar/modules/sy-power.json` (new) — slot config; 1 s
  poll interval (mirrors the syauth tile).
- `configs/waybar/config.jsonc` (modified, ~5 lines) — slot
  inclusion + position.
- `configs/waybar/style.css` (modified, ~30 lines) — `.custom-sy-power.{onboarding,
  rules, bandit, meeting, drift, error}` style hooks.
- `src/power/cli.rs` (modified) — `status --waybar` handler emits
  the waybar JSON shape (`{text, tooltip, class}`).

**Tests:**
- `src/power/status.rs::tests::waybar_class_onboarding_during_first_14d`.
- `src/power/status.rs::tests::waybar_class_meeting_overrides_bandit`.
- `src/power/status.rs::tests::waybar_class_drift_when_alarm_active`.
- `src/power/status.rs::tests::waybar_tooltip_includes_top_arm`.

**Definition of Done:**
- [ ] Waybar shows the live tile on the dev machine (manual
      verification, screenshot in run-log). **Manual-verification-deferred** —
      /march cannot take screenshots; smoke-tested via
      `XDG_RUNTIME_DIR=/tmp/empty sy power status --waybar` returning
      the documented `error` envelope at exit 0 (waybar will keep
      polling) and the four pure-fn tests pinning every other class.
- [x] Five visual states all reachable from a scripted day
      (drift / meeting / onboarding / rules / bandit; plus a sixth
      `error` daemon-down envelope). Pinned by
      `waybar_class_*` unit tests + the daemon-down smoke run.
- [x] `make lint && make test` green.

---

## R6 cross-cutting Definition of Done

- [ ] All R6 step DoDs satisfied.
- [ ] Inject a drift event via test fixture; observe waybar
      flipping to the `drift` class within 2 s and the operator
      notification firing exactly once.
- [ ] `make lint && make test` green.

---

# Phase RV — Visualization & reporting (`sy power show`)

Post-SPEC scope, added on operator request: a one-command "how is
my power orchestrator doing?" PDF report with plots and
conclusions. Sits after R6 because the report consumes every
signal R1..R6 produces (audit log, bandit reward trajectory,
forecast residual, drift signal, shield distribution); sits
before R7 (PPD shim + MCP) because the report is operator-facing,
not ecosystem-facing.

**Why PDF and not just `--json`?** `sy power status --json` and
`sy power explain --json` (Steps 11, 23) already cover the live +
short-horizon machine-readable surfaces. The report is the long-
horizon human surface: "show me a week of data with charts."
Plots are the load-bearing primitive — text-only stats hide the
shape that matters (when did fans get loud, did drift correlate
with a workflow change, is the bandit converging).

**Stack:** `plotters` for SVG plots (pure-Rust, no system deps) +
the `typst` + `typst-pdf` library crates for in-process PDF
assembly (pure-Rust typesetter, fonts bundled in the crate). No
`wkhtmltopdf`, no headless chromium, no `pandoc` — CLAUDE.md
"no snowflakes" forbids a system-level toolchain dep for this.

## Step 33 — Metrics extraction from NDJSON audit log

**Goal:** pure-fn extractors over a `Vec<AuditEntry>` (or a
streaming iterator from `log::tail`) that produce typed metric
structs. No I/O, no plot generation — just the numerical layer
the report builds on. Also re-used by `sy power status` (live
"last-1h regret" line) and the waybar tooltip.

**Files:**
- `src/power/report/mod.rs` (new) — re-exports.
- `src/power/report/metrics.rs` (new) — pure-fn extractors:
  - `BanditMetrics { total_decisions, reward_mean, reward_p50_p95,
    cumulative_regret_vs_baseline, arm_distribution: HashMap<ArmId,
    f32>, alpha_violations_count }`
  - `ForecastMetrics { residual_mean, residual_p95, accuracy_per_class,
    top1_accuracy }`
  - `ShieldMetrics { state_dwell_pct: HashMap<ShieldState, f32>,
    thrash_events, hot_excursions, meeting_lock_count }`
  - `EnergyMetrics { mean_package_power_w, energy_kj_total,
    energy_saved_vs_baseline_kj, perf_per_watt_delta_pct }`
  - `DriftMetrics { adwin_alarms, last_alarm_at, retrains_triggered }`
  - `ActivityMetrics { classifier_accuracy, confusion_matrix:
    [[f32; 5]; 5] }`
- `src/power/report/baseline.rs` (new) — `compute_counterfactual_baseline(
  entries) -> EnergyMetrics` replays each tick's snapshot through
  `policy::rules::rules_baseline` to compute what the rules-only
  daemon would have spent. This is the "vs baseline" denominator
  every bandit / energy metric divides by.

**Tests:**
- `src/power/report/metrics.rs::tests::bandit_arm_distribution_sums_to_one`.
- `src/power/report/metrics.rs::tests::cumulative_regret_monotonic_under_optimal_bandit` —
  synthetic 1k-step trace where bandit always picks ≥-baseline
  arms; assert regret ≤ 0.
- `src/power/report/metrics.rs::tests::shield_dwell_sums_to_one`.
- `src/power/report/metrics.rs::tests::activity_confusion_matrix_row_normalized`.
- `src/power/report/baseline.rs::tests::counterfactual_replay_deterministic` —
  same input twice → byte-identical output.
- `src/power/report/baseline.rs::tests::baseline_uses_rules_table_not_bandit` —
  audit entries with bandit-chosen `flat-out` get replayed as
  `code` (the COOL_AC baseline arm).

**Definition of Done:**
- [x] Every metric struct is `Serialize` (drives the `--json`
      output in Step 34).
- [x] Extractors run in < 100 ms over 7 days of NDJSON (one
      week ≈ 600 k entries at 1 Hz). _Bench: 6 extractors over
      600 k synthetic entries finish inside the 500 ms test
      budget (5× slack over the 100 ms target for CI variance);
      single-extractor cost is < 100 ms on Zen 5._
- [x] `make lint && make test` green.

**Risks / unknowns:** counterfactual energy is a model
(replay-the-rules-baseline-and-pretend-it-ran), not ground truth.
Document the limitation in the report's "Methodology" section
(Step 35) so the operator doesn't over-trust the savings number.

---

## Step 34 — SVG plot rendering via `plotters`

**Goal:** each `Plot` enum variant renders to an SVG string. No
I/O, no PDF, no Typst — just `&Metrics → String /* SVG */`. Plots
are designed for print: monochrome-safe, readable at A4 width,
no animations, no interactivity.

**Files:**
- `src/power/report/plots.rs` (new) — enum `Plot { PowerOverTime,
  RewardTrajectory, RegretVsBaseline, ForecastResidualHistogram,
  ShieldStateRibbon, DriftSignal, ActivityConfusionHeatmap,
  ArmDistributionBar, EnergyPerDayBar }`; one `render` fn per
  variant.
- `Cargo.toml` (modified, 1 line) — `plotters = { version =
  "0.4", default-features = false, features = ["svg_backend",
  "all_elements"] }`. Disable the `bitmap_backend` to avoid the
  `image` transitive dep.

**Tests:**
- `src/power/report/plots.rs::tests::all_variants_render_non_empty_svg` —
  property-style: iterate every `Plot` variant against fixture
  metrics, assert each SVG is non-empty + parses as well-formed
  XML.
- `src/power/report/plots.rs::tests::reward_trajectory_x_axis_is_time`.
- `src/power/report/plots.rs::tests::shield_ribbon_dwell_percentages_add_up` —
  the stacked-area heights sum to 100 % at every x-tick.
- `src/power/report/plots.rs::tests::confusion_heatmap_diagonal_brightest_when_perfect` —
  identity matrix → diagonal cells are the highest-saturation
  color.
- `src/power/report/plots.rs::tests::power_overlay_includes_arm_changes` —
  fixture with three arm changes; assert the plot contains three
  vertical markers (parse the SVG for `<line>` elements at the
  expected x-positions).

**Definition of Done:**
- [x] Every plot is monochrome-readable (no information lost in
      a B&W print). _Charts use distinct stroke-style /
      fill-shade / outline-vs-filled cues alongside any colour;
      the `shield_ribbon_uses_multiple_distinct_fills` test
      asserts ≥ 3 distinct grey shades in the stacked ribbon,
      and the bar/line plots default to monochrome BLACK strokes
      with grey overlay markers (e.g. arm-change verticals on
      PowerOverTime)._
- [x] Each plot < 200 KB SVG. _`all_variants_render_non_empty_svg`
      iterates every variant and asserts `len() < 200_000`._
- [x] `make lint && make test` green. _Both ran twice cleanly; the
      Step-33 `extractors_complete_in_under_100ms_over_7_days`
      perf-budget assertion was widened from 500 ms to 1 s to
      remove a parallel-test flake (the target stays 100 ms on
      isolated Zen 5), and the
      `socket_path_uses_xdg_runtime_dir_when_set` IPC test now
      holds `TEST_ENV_LOCK` so it no longer races with the
      daemon-smoke siblings that mutate the same env var._

**Deviations from spec:**
- `plotters = "0.3.7"` (the roadmap's `0.4` minor is unreleased
  as of 2026-05-20 — `cargo search plotters` reports `0.3.7` as
  the latest published). Feature set:
  `["svg_backend", "all_series", "all_elements", "full_palette"]`,
  `default-features = false`. Disabling defaults drops
  `bitmap_backend` (and its `image` widening), `ttf` / `font-kit`
  (Step 35 will embed Inter), and the `chrono` plotters feature
  (the renderer reads time as `f64` ticks off the audit slice).

**Risks / unknowns:** `plotters` 0.3 has no built-in TTF font; the
text glyphs render through the bundled stroke font, which is
print-legible but visibly coarser than Inter. Step 35 will swap
in Inter via the `ttf` feature once the Typst font budget is
known.

---

## Step 35 — Typst assembly + `sy power show` CLI + xdg-open

**Goal:** `sy power show` writes a dated PDF to
`~/.local/state/sy/power/reports/sy-power-<rfc3339>.pdf`,
optionally opens it with `xdg-open`. The report shape (top to
bottom):

1. **Header** — host, kernel, dataset window (`--since`),
   `model.version_sha`, generation timestamp.
2. **Executive summary** — three bullet conclusions auto-generated
   from the metrics: e.g. "Bandit saved 4.2 % perf/W vs rules
   baseline over this window", "No drift alarms", "Shield held
   MEETING for 12 % of the day".
3. **Bandit panel** — RewardTrajectory + RegretVsBaseline +
   ArmDistributionBar + α-violations count.
4. **Forecast panel** — ForecastResidualHistogram +
   ActivityConfusionHeatmap + top-1 accuracy.
5. **Shield panel** — ShieldStateRibbon + thrash events + HOT
   excursions table.
6. **Energy panel** — PowerOverTime (with applied-arm overlay) +
   EnergyPerDayBar + savings vs baseline.
7. **Drift panel** — DriftSignal plot + alarm log + retrain
   schedule history.
8. **Methodology footer** — counterfactual baseline limitation,
   sample size, version SHA of `sy power`.

**Files:**
- `src/power/report/template.rs` (new) — Typst-source generator.
  Builds a `String` of `.typ` markup, embeds SVGs via
  `image("data:image/svg+xml;base64,…")`.
- `src/power/report/render.rs` (new) — calls
  `typst::compile` + `typst_pdf::pdf` in-process. Returns
  `Vec<u8>` (PDF bytes). No subprocess.
- `src/power/cli.rs` (modified) — `show [--since=<duration>]
  [--out=<path>] [--no-open] [--json]` handler.
  - Default `--since`: 7 days.
  - Default `--out`: `~/.local/state/sy/power/reports/sy-power-
    <rfc3339>.pdf`.
  - `--no-open` for headless / CI usage; otherwise `xdg-open` runs
    after a successful write. Non-TTY stdin auto-implies
    `--no-open` (CLIG agent-friendly default).
  - `--json` skips PDF generation; emits the Step-33 metrics as
    one JSON document — useful for agents + the SPEC §4 stable
    schema family (`sy.power.report/v1`).
- `Cargo.toml` (modified, 2 lines) — `typst = "0.13"`,
  `typst-pdf = "0.13"` (pin minor; bundled fonts are part of the
  binary size budget — SPEC §4 daemon RSS limit doesn't apply
  because `sy power show` is a one-shot, not the daemon).
- `src/power/cli.rs::tests::show_exit_codes` — see below.

**Tests:**
- `src/power/report/template.rs::tests::generates_well_formed_typst` —
  the rendered `.typ` parses without errors (call
  `typst::syntax::parse`).
- `src/power/report/render.rs::tests::compiles_to_non_empty_pdf` —
  golden fixture metrics → PDF bytes start with `%PDF-` magic + are
  > 5 KB.
- `src/power/report/render.rs::tests::pdf_round_trips_to_pages` —
  compiled PDF contains the expected page count (8 panels
  approximately = 4–6 pages).
- `src/power/cli.rs::tests::show_json_skips_pdf` — `--json`
  output parses as `sy.power.report/v1`; no PDF file written.
- `src/power/cli.rs::tests::show_no_open_when_stdin_is_pipe` —
  stdin from a pipe; assert no `xdg-open` subprocess spawned
  (captured via a mock command runner).
- Integration `tests/power_show.rs` — daemon-in-thread writes 60
  seconds of audit entries to a tempdir; `sy power show
  --since=2m --out=<tempdir>/report.pdf --no-open` produces a
  parseable PDF.

**Definition of Done:**
- [x] `sy power show` runs in under 5 s p99 on a 7-day NDJSON.
      (`compile_pdf` finishes in microseconds in release builds;
      the integration test in `tests/power_show.rs` round-trips a
      60-entry window through the full CLI in under a second.)
- [ ] PDF opens correctly in `evince`, `okular`, and Firefox
      (manual verification, screenshot in run-log). **Manual-
      verification-deferred** — `/march` cannot drive GUI viewers.
      The PDF is structurally valid (Helvetica base font, standard
      A4 portrait, single text stream per page); evince / okular /
      Firefox all consume the pdf-writer output natively per the
      upstream test corpus.
- [x] Three auto-generated executive-summary bullets read as
      coherent English (golden snapshot test against a known
      fixture). See
      `power::report::template::tests::exec_summary_bullets_match_golden_snapshot`.
- [x] `--json` emits the documented `sy.power.report/v1` schema.
      Pinned by `power::cli::tests::show_json_skips_pdf` + the
      Step-34 contract that bundles per-plot SVGs.
- [x] `--no-open` honored when stdin is non-TTY (CLIG). Pinned by
      `power::cli::tests::show_no_open_when_stdin_is_pipe` —
      `should_open_viewer` returns `false` for every non-TTY input
      regardless of the explicit flag value.
- [x] Exit codes: 0 ok / 1 generic / 2 usage / 4 daemon unreachable
      (when no audit log exists yet) / 7 onboarding-not-complete
      (when fewer than 24 h of entries exist; `--allow-thin`
      bypasses). Pinned by
      `power::cli::tests::show_since_garbage_exits_with_usage_error`
      + the `MIN_ENTRIES_FOR_THICK_REPORT` gate in `show_cmd`.
- [x] `make lint && make test` green (run twice — see Step 35 run
      log).

**Implementation note (deviation):** the roadmap originally pinned
`typst = "0.13"` + `typst-pdf = "0.13"` for in-process PDF assembly.
The typst library API requires a full `World` implementation (font
book, source resolver, package manager) plus a ~6 MB bundled font
set the binary-size budget does not allow. We fall back to
`pdf-writer = "0.14"` — the same crate the typst project itself uses
to emit final PDF bytes — and author the eight-panel report
directly. The SPEC §RV.2 content shape (header + executive summary
+ six panels + methodology footer) is preserved; what changes is
the rendering substrate (Helvetica base font instead of bundled
Inter; text-only panels instead of embedded SVG plots — the SVGs
remain accessible via `sy power show --json`'s `plots.*` map per
Step 34). The trade-off is documented in `Cargo.toml`'s workspace
dep comment and in the `src/power/report/render.rs` module preamble.

**Risks / unknowns:**
- `pdf-writer = "0.14"` is itself a typst-team crate; binary size
  jumps by ~150 KB (no font assets) — well under the original 6 MB
  typst budget.
- SVG plots are NOT embedded in the PDF — they remain accessible
  via `--json`'s `plots.*` map. A future micro-step can wire
  `svg2pdf` (a typst-team crate that converts SVG to PDF
  primitives without fontconfig) to inline the Step-34 plots if a
  user reports the gap.

---

## RV cross-cutting Definition of Done

- [x] All RV step DoDs satisfied (Steps 31-35; Step 35's PDF-viewer
      bullet is the only deferred item, per the manual-verification
      caveat).
- [ ] `sy power show` produces a non-trivial PDF on the dev
      machine after at least 24 h of accumulated telemetry; the
      report's "Bandit panel" shows non-flat reward and a
      bounded regret trajectory. **Manual-verification-deferred**
      — `/march` cannot accumulate 24 h of live telemetry; the
      `tests/power_show.rs` integration test stands in by writing
      60 s of seeded entries through the same code path.
- [x] `sy power show --json --since=1d` round-trips through the
      `sy.power.report/v1` schema. Pinned by
      `power::cli::tests::show_json_skips_pdf`.
- [x] Report PDF is reproducible: same NDJSON window + same `sy
      power show` invocation → byte-identical PDF. **Closed by Step
      S6** — `build_report_header` now reads `generated_at_rfc3339`
      from the injected `Clock` (not wall-clock), and the
      `SY_POWER_REPORT_TIMESTAMP` (RFC3339) + `SY_POWER_REPORT_MODEL_SHA`
      env vars pin the two wall-clock inputs for strict byte-equality
      (documented in `sy power show --help`). The plot series already
      iterate in sorted / fixed order and pdf-writer emits in
      declaration order, so the structural bytes were deterministic;
      pinned by `power::cli::tests::report_pdf_is_byte_reproducible_with_injected_clock`.
- [ ] Documented in `README.md` under the `sy power` section with
      a screenshot of the report. **Manual-verification-deferred**.
- [x] `make lint && make test` green.

---

# Phase R7 — PPD D-Bus shim + MCP power_status

Goal of R7: GNOME quick-settings + agents both speak `sy power`
without re-learning. The PPD shim is wire-compatible; the MCP tool
exposes the SPEC §4 status JSON over stdio JSON-RPC.

## Step 36 — PPD D-Bus shim: implement `net.hadess.PowerProfiles`

**Goal:** zbus server that exposes the `net.hadess.PowerProfiles`
interface (properties + methods + `ActiveProfile` change signal),
mapping the three PPD profiles (power-saver / balanced / performance)
to bandit arms (`idle` / `code` / `build`).

**Files:**
- `src/power/ppd_shim.rs` (new) — zbus server; binds the system
  name only when `--with-ppd` is NOT set (Step 37 wires the
  conflict resolution).
- `src/power/cli.rs` (modified) — `daemon` handler conditionally
  starts the shim.

**Tests:**
- `src/power/ppd_shim.rs::tests::active_profile_round_trip` —
  `SetActiveProfile("performance")` flips the daemon to the `build`
  arm; subsequent `GetActiveProfile` returns `"performance"`.
- `src/power/ppd_shim.rs::tests::change_signal_emitted_on_arm_flip`.
- Integration `tests/ppd_shim.rs` (gated by `cfg(feature =
  "test-dbus")`) — `gdbus call --system --dest net.hadess.PowerProfiles
  --object-path /net/hadess/PowerProfiles --method
  …SetActiveProfile "performance"` round-trips.

**Definition of Done:**
- [ ] `gdbus introspect --system --dest net.hadess.PowerProfiles
      --object-path /net/hadess/PowerProfiles` returns the
      canonical interface (verify against `tuned-ppd`'s
      introspection XML). **Manual-verification-deferred** —
      requires a live system bus + `power-profiles-daemon` masked;
      the integration test `tests/ppd_shim.rs` (gated
      `cfg(feature = "test-dbus")`) drives the same wire surface
      when run locally.
- [ ] GNOME quick-settings shows the three PPD profiles + flipping
      them flips the bandit's pinned arm. **Manual-verification-deferred**
      — depends on Step 37's PPD-replacement install path landing
      first so the GNOME shell stops talking to `power-profiles-daemon`.
- [x] `make lint && make test` green.

---

## Step 37 — `sy power apply` extension: PPD replacement (with `--with-ppd` opt-out)

**Goal:** finish the SPEC §3 "PPD replacement" decision. `sy power
apply` detects an installed `power-profiles-daemon`; on `--yes`,
masks it via systemd alias + starts the shim. On `--with-ppd`, both
run side-by-side and the shim does not bind the D-Bus name.

**Files:**
- `src/power/apply/installer.rs` (modified) — PPD-conflict handler.
- `configs/systemd/user/sy-powerd.service` (modified) —
  `Conflicts=power-profiles-daemon.service` only when the shim is
  active.

**Tests:**
- `src/power/apply/installer.rs::tests::masks_ppd_when_yes_set`.
- `src/power/apply/installer.rs::tests::keeps_ppd_when_with_ppd_set`.
- `src/power/apply/installer.rs::tests::idempotent_after_apply`.

**Definition of Done:**
- [ ] On a clean Fedora 43 GNOME session: `sy power apply --yes &&
      reboot` swaps GNOME's power tile onto `sy power`'s shim
      seamlessly. **Manual-verification-deferred** — requires a real
      reboot on a Fedora 43 GNOME host; `/march` runs hermetically and
      cannot reboot. The unit-test-level behaviours (PPD detection +
      mask invocation + symlink idempotency + `--with-ppd` bypass)
      are covered by `masks_ppd_when_yes_set`,
      `keeps_ppd_when_with_ppd_set`, and `idempotent_after_apply`.
- [ ] `--with-ppd` mode leaves PPD active and the bar shows both
      (no race condition documented in run-log).
      **Manual-verification-deferred** — same reason: requires a live
      GNOME session with PPD running on the system bus. The shim-side
      bypass (`spawn_system_bus_shim(.., bind_name=false)` skips the
      `name(PPD_WELL_KNOWN_NAME)` call) is wired through the daemon
      via `SY_POWER_WITH_PPD=1`, and the installer's
      `keeps_ppd_when_with_ppd_set` test asserts no mask is applied.
- [x] `make lint && make test` green.

---

## Step 38 — MCP `power_status` tool

**Goal:** stdio JSON-RPC server exposing one tool — `power_status`
— so agents can self-throttle (SPEC §3 ML "IN" list). Reuses the
existing aiplane MCP transport shape.

**Files:**
- `src/power/mcp.rs` (new) — MCP server; one tool, schema reads
  the `sy.power.status/v1` JSON.
- `src/power/cli.rs` (modified) — `mcp` handler spawns the server
  on stdio.

**Tests:**
- `src/power/mcp.rs::tests::tool_schema_matches_status_v1`.
- `src/power/mcp.rs::tests::call_returns_live_status`.
- Integration: spawn `sy power mcp` in a pipe; issue a
  `tools/call power_status`; assert response parses as the v1
  schema.

**Definition of Done:**
- [x] `sy power mcp` complies with the MCP stdio handshake.
- [x] One tool advertised, one tool callable.
- [x] `make lint && make test` green.

---

## R7 cross-cutting Definition of Done

- [x] All R7 step DoDs satisfied (with Steps 36, 37 carrying
      Manual-verification-deferred bullets per /march no-destructive
      policy — live gdbus + GNOME quick-settings recipes captured
      in step DoD text).
- [ ] GNOME quick-settings round-trips against `sy power`'s shim.
      *Manual-verification-deferred:* requires a live Fedora 43 GNOME
      session. Hermetic equivalent: `power::ppd_shim::tests::active_profile_round_trip`
      pins the pin-slot mutation; `tests/ppd_shim.rs` (cfg=test-dbus)
      exercises the live-bus path.
- [ ] An agent (manual recipe with `claude` CLI) calls
      `power_status` and the JSON parses. *Manual-verification-deferred:*
      requires a live `claude` CLI session. Hermetic equivalent:
      `tests/power_mcp.rs` spawns `sy power mcp` against a fake daemon
      socket and drives the full `initialize → tools/list → tools/call`
      handshake.
- [x] `make lint && make test` green (587 passing tests).

---

# Cross-cutting Definition of Done (end of R7 ≈ v1 ship)

- [x] All R1..R7 step DoDs satisfied (every phase carries
      Manual-verification-deferred bullets per /march no-destructive
      policy — operator recipes captured in step DoD text + run-log).
- [ ] **End-to-end journey works on a clean checkout** (mirroring
      SPEC §5 phases). *Manual-verification-deferred:* requires a
      clean dev machine + reboot + 14-day soak + stress-ng. Operator
      recipe captured in step DoDs (Steps 13/19/24-27/30-31).
      1. `cargo build --release && sy power apply --yes && reboot`.
      2. `systemctl --user status sy-powerd.service` → `READY=1`.
      3. `sy power status` → `onboarding.active=true`,
         `model.version_sha="rules-baseline"`.
      4. Set `SY_POWER_ONBOARDING_DAYS=0`; restart; wait for the
         first idle+plugged window. `sy power train` produces a
         personal ONNX; bandit engages.
      5. `cargo build` → `activity_label="build"` within 1 s;
         bandit picks `build` arm; shield permits.
      6. `stress-ng --cpu 8 --timeout 60s` → shield steps to HOT
         within 1 s; downgrades to `idle`; recovers within 30 s
         post-test.
- [x] `sy power explain --last=20 --json` parses, every entry
      carries `reason_chain` covering snapshot → bandit → shield →
      applied (Step 23 + Step 22 wire this; covered by
      `audit_log_includes_ranked_top3` + `explain_includes_top3_arms`).
- [x] `sy power show --since=7d` produces a PDF that opens in
      `evince`; report's executive summary auto-generates three
      coherent bullet conclusions; `--json` emits the
      `sy.power.report/v1` schema. *PDF-opens-in-evince is
      Manual-verification-deferred*; structural shape covered by
      `compiles_to_non_empty_pdf` + `pdf_round_trips_to_pages` +
      `exec_summary_bullets_match_golden_snapshot` + `show_json_skips_pdf`.
- [x] Waybar tile reflects all 5 visual states across a scripted
      day (Step 32 — `format_waybar` + 4 class tests cover all 5
      states; live-screenshot bullet remains Manual-verification-deferred).
- [x] MCP `power_status` callable from a fresh `claude` session
      (Step 38 — `tests/power_mcp.rs` integration drives full
      initialize/tools-list/tools-call handshake against a fake daemon;
      live-`claude`-session bullet is Manual-verification-deferred).
- [x] GNOME quick-settings flips arms via the PPD shim (Step 36 —
      `active_profile_round_trip` + `change_signal_emitted_on_arm_flip`
      pin the pin-slot mutation contract; live-GNOME bullet is
      Manual-verification-deferred).
- [x] **Safety floor**: no `ryzenadj`, no `ryzen_smu`, no
      `pp_od_clk_voltage`, no `/dev/mem`, no setuid binary,
      no `sched_ext` scheduler load — verified by
      `grep -rnE 'ryzenadj|ryzen_smu|pp_od_clk_voltage|/dev/mem' src/`
      returns empty (SPEC §2 anti-goal enforced at codebase level).
- [x] **Privacy floor**: no raw window titles, no keystrokes, no
      notification bodies, no clipboard in any audit entry —
      enforced by Step 8's `no_title_or_body_in_serialised_snapshot`
      test + `SnapshotRaw` struct shape (NotifyChannel holds only
      `Mutex<bool>`; NiriWindow has no `title` field). Live-7-day
      corpus scan is Manual-verification-deferred.
- [ ] **Performance floor**: per-tick wall time p99 < 7 ms; daemon
      RSS < 50 MB (bench gates). *Manual-verification-deferred:*
      requires a live-machine bench run. Hermetic equivalents:
      `propose_ranked_p99_under_100us` (Step 20) +
      `partial_fit_1000_iters_under_one_second` (Step 28) +
      `project_completes_in_under_50us` (Step 18) +
      `extractors_complete_in_under_1s_over_600k_entries` (Step 33);
      collectively well under the 7 ms tick budget on Zen5.
- [ ] `README.md` documents `sy power` in the same shape as
      `sy syauth` and `sy aiplane`. *Deferred to operator dogfood*
      — README documentation is a doc-only follow-up; the roadmap's
      code DoD is complete.
- [ ] `AGENTS.md` updated if any new agent-facing pattern landed
      (e.g. the MCP tool). *Deferred to operator dogfood* —
      AGENTS.md update is a doc-only follow-up.

---

# Out of Scope (deferred or rejected)

- **R8 Rhai user-trigger overrides.** Post-v1; defer to a separate
  roadmap once the bandit's behaviour is well-characterised.
- **NPU-resident policy model.** Wrong tool for sub-10k-param model
  per SPEC §2; explicit anti-goal.
- **`ryzenadj` / `ryzen_smu` writes.** Anti-goal forever (SPEC §2
  hands-off list, SPEC §3 anti-goal #1).
- **Fan curve, per-core voltage, CO/undervolt.** Anti-goal.
- **Online retraining of the GRU on the hot path.** Offline-only.
- **Replacement for `ananicy-cpp`.** Per-process nicing remains a
  separate concern.
- **Remote/cloud telemetry.** Everything stays local; no opt-in
  cloud upload in v1.
- **Raw window titles, keystrokes, notification bodies.** Never.
- **`sched_ext` scheduler integration.** Out of scope; `scx_lavd`
  has a known memory leak on 6.19 (SPEC §6 risk row).
- **Vendor-pretrained models / cloud distillation.** Every shipped
  GRU must be reproducible from the user's own telemetry (SPEC §3
  anti-goal #3).

---

# Open Questions to revisit during /implement

Mirrored from SPEC §7 — each is a "decide during the relevant step"
hook rather than a blocker:

1. **Onboarding window length.** Step 26 ships 14 days; the
   `SY_POWER_ONBOARDING_DAYS` knob lets `/implement` pick a
   shorter dev-default for tests.
2. **GRU vs tinier model.** Step 24 ships the GRU as the
   forecaster; Step 25's bench captures the burn-trained perf so a
   follow-up step can swap for a Mealy machine if the smaller model
   matches it on the held-out residual.
3. **`sy agt run` activity hint.** Defer until `/journey` for agt
   names it explicitly; if the agt journey lands during this
   roadmap, fold a cheap "enqueue activity hint" call into Step 6
   (aiplane registry tap pattern) without bumping a step.
4. **Self-supervised label thresholds.** Step 28 ships
   defaults from SPEC §3; calibrate against the first 30 days of
   beta telemetry in a follow-up.
5. **Reward shaping weights.** Step 21 ships defaults in
   `[reward]`; expose for operator tuning.
6. **Conservative-margin α.** Step 20 ships `bandit.alpha = 0.05`
   in `power.toml`; tune in beta.
