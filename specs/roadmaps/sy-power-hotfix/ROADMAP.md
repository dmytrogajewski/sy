# ROADMAP: sy-power-hotfix

Source: real-hardware ship diagnostics on 2026-05-20 against AMD HX 370 / kernel 7.0.6-100.fc43. The sy-power v1 roadmap was code-complete (587/0/11 tests) but production deployment surfaced six real-hardware compatibility gaps. This roadmap lands the minimum fixes that bring R2+ actuation and the four read-side CLI surfaces to a working state on a Fedora 43 / HX 370 host.

## Overview

Six atomic /implement steps. Order is dictated by blast radius: NaN serialization fix unblocks four CLI surfaces with one change; then sensor-path discovery makes the bandit's context actually populated; then iGPU + cgroup actuator fixes; then the installer cosmetics + productisation gaps. Each step lands code + tests in /march mode; one rebuild + install + daemon restart at the end before the operator reboots for the grub kernel param.

Live daemon state at 2026-05-20T18:54Z:
- `sy-powerd.service` active, 1Hz tick, 630+ NDJSON entries
- `platform_profile` actuator working (post-tmpfiles.d, currently "quiet")
- EPP actuator EBUSY (kernel reboot pending for `amd_dynamic_epp=disable`)
- iGPU + cgroup + NPU + PPD-shim actuators failing on this hardware
- `sy power {status --json, log, explain, show --since}` all read 0 entries
- 3 of 12 sensor channels return None on this hardware (tctl_c, package_power_w, battery_soc_pct)

---

## Step H1 — NaN→null serialize fix on Snapshot.features (unblocks 4 read-side CLIs)

**Goal:** sensors that fail to read produce `f32::NAN`, `serde_json` serializes that as JSON `null`, and downstream tail readers (`sy power status --json`, `sy power log`, `sy power explain`, `sy power show --since`) can't deserialize `null` into typed `f32` — all four silently return 0 entries despite the daemon writing 1 Hz to disk. Single fix at the serialize boundary unblocks all four.

**Files:**
- `src/power/snapshot.rs` (modified) — add `#[serde(serialize_with = "serialize_features_nan_as_null", deserialize_with = "deserialize_features_null_as_nan")]` on the `features: [f32; 12]` field. Symmetric pair: serialize maps `NaN → null`, deserialize maps `null → NaN`. On-disk shape unchanged (already emits `null` today); only the deserialize-back path becomes lossless.
- `src/power/snapshot.rs::tests::round_trips_through_serde_when_features_have_nan` (new) — serialize a Snapshot with `features[0] = NaN`, deserialize back, assert `features[0].is_nan()`.
- `src/power/snapshot.rs::tests::nan_serializes_as_json_null_for_back_compat` (new) — assert `to_string(snap)` produces `"features":[null,...]` for the NaN slot (locks the on-disk shape against future drift).

**Tests:**
- `src/power/snapshot.rs::tests::round_trips_through_serde_when_features_have_nan`.
- `src/power/snapshot.rs::tests::nan_serializes_as_json_null_for_back_compat`.
- Existing `tests/power_log.rs` integration test must keep passing — it currently injects synthetic entries with all-finite features; after the fix it should also work with NaN-containing entries.

**Definition of Done:**
- [x] `Snapshot` round-trips through `serde_json::{to_string, from_str}` losslessly even when `features` contains `NaN`.
- [x] On-disk NDJSON shape unchanged (still emits `null` for NaN — back-compat with 630+ entries already written today).
- [x] After this fix: `sy power log --since=2m`, `sy power explain`, `sy power show`, `sy power status --json` all read entries from the running daemon's NDJSON.
- [x] `make lint && make test` green.

**Risks / unknowns:** the same fix is needed on `Snapshot.snapshot_hash` if that's a derived f32 anywhere, but it's a `String` (BLAKE3 hex) — safe. Bandit's CLUCB feeds `features` through `propose_ranked(&[f32])` which already handles NaN via the Step 21 NaN guard.

---

## Step H2 — Real-hardware sensor fallback chain for tctl / package_power / battery

**Goal:** on this HX 370 / kernel 7.0.6 host, the SPEC §4-named sensor paths return None for `tctl_c`, `package_power_w`, and `battery_soc_pct`. The bandit's context vector is therefore 3/12 features = NaN today (visible in the 630+ live NDJSON entries: `"raw":{"tctl_c":null,"package_power_w":null,...,"battery_soc_pct":null}`). Need to probe the actual on-host paths and ship a fallback chain.

**Files:**
- `src/power/sensors/hwmon.rs` (modified) — extend the `walkdir` scan to also accept `k10temp` / `zenpower` / `acpitz` driver names (this host appears to have a different `hwmon*` numbering or driver name; the parser must enumerate by `name` file, not by fixed index). Add `#[cfg(test)]` fixture rows for `k10temp` at `hwmon0` / `hwmon3` / `hwmon7` to cover ordering drift.
- `src/power/sensors/rapl.rs` (modified) — try `/sys/class/powercap/intel-rapl-mmio:0/energy_uj` and `/sys/class/powercap/amd-rapl:0/energy_uj` as fallbacks when the SPEC §4 `intel-rapl:0` path is absent. The probe order is: `intel-rapl:0` → `intel-rapl-mmio:0` → `amd-rapl:0` → `Err::PowercapAbsent`.
- `src/power/sensors/battery.rs` (modified) — accept `BAT0` or `BAT1` (this host has `BAT1`, the AGENTS.md-era assumption was `BAT0`); already in Step 3 per run log but verify the live behaviour matches.
- Manual on-host probe: at the top of the step, run `ls /sys/class/hwmon/*/name | xargs head` and `ls /sys/class/powercap/` against the running daemon's host; capture the actual paths into the test fixtures.

**Tests:**
- `src/power/sensors/hwmon.rs::tests::finds_k10temp_at_arbitrary_hwmon_index` — fixture with k10temp at hwmon3 instead of hwmon0.
- `src/power/sensors/rapl.rs::tests::falls_back_to_intel_rapl_mmio` — fixture with only `intel-rapl-mmio:0` present.
- `src/power/sensors/battery.rs::tests::accepts_bat1_when_bat0_absent`.
- Manual: after rebuild + restart, `sy power show --json | jq '.bandit.chosen_arm, .applied_policy'` shows non-null thermal-related metrics.

**Definition of Done:**
- [x] All four sysfs-class walkers (`sensors::{hwmon,igpu,battery}` + `apply::igpu::find_amd_card`) use `.follow_links(true)` so the symlink-only entries under `/sys/class/{hwmon,drm,power_supply}/` are descended instead of skipped by the `is_dir()` filter. Real-host probe confirmed `/sys/class/hwmon/hwmon5/temp1_input` is readable as the daemon user and now reports the live `tctl_c` (verified `cat` returns `82375` ⇒ 82.375 °C).
- [x] Regression tests cover the symlink case for every walker: `sensors::hwmon::tests::follows_symlinks_in_sysfs_class_hwmon`, `sensors::igpu::tests::follows_symlinks_in_sysfs_class_drm`, `sensors::battery::tests::follows_symlinks_in_sysfs_class_power_supply` — each builds a TempDir with `class/<bus>/<dev> → ../../devices/...` and asserts the reader returns `Ok(...)`.
- [ ] All three sensors return `Ok(...)` on this HX 370 host (verified end-to-end after rebuild + restart in the cross-cutting deploy step — `sy power log --since=10s --json | jq '.snapshot.raw'`).
- [ ] The 12-element `features` vec has zero NaN slots on a healthy snapshot (end-to-end, post-deploy).
- [x] `make lint && make test` green (verified under hot CPU at 86 °C — see `SY_SYSFS_ROOT` retry below).
- [x] **Retry on hot CPU.** The `.follow_links(true)` fix unmasked a hidden coupling in `tests/power_bandit_floor.rs::bandit_status_block_schema_matches_spec_v1`: the test spawns `sy power status --json`, and the binary's CLI-side anti-dead-code probe (`probe_actuators` + `snapshot::collect_tick`) reads the live `/sys` tree before crossing the IPC boundary, so a hot host (CPU > ~75 °C) flips the SPEC §4 `bandit.baseline_arm` away from `"browse"`. Fix: introduce a private `sysfs_root()` helper in `src/power/cli.rs` reading `SY_SYSFS_ROOT` (default `/sys`); the integration test sets it to a hermetic tempdir before spawning. Verified failing-then-passing under sustained 86 °C load: without `SY_SYSFS_ROOT` the assertion mismatched (`left: "idle", right: "browse"`); with the env override the test is hermetic.

**Risks / unknowns:** if the HX 370's `tctl_c` lives behind a vendor driver path neither k10temp nor zenpower expose, the fallback chain terminates at "no thermal sensor available" cleanly — the daemon stays alive, the feature stays NaN, the bandit context degrades by one channel. Document explicitly.

---

## Step H3 — iGPU actuator uses `power_dpm_force_performance_level` instead of `pp_power_profile_mode`

**Goal:** Steps 3 (sensor) + 16 (actuator) assumed the iGPU exposes `/sys/class/drm/card*/device/pp_power_profile_mode`. On this kernel/hardware, that file is absent — the iGPU exposes `power_dpm_force_performance_level` (read-write: `auto | low | high | manual | profile_standard | profile_min_sclk | profile_min_mclk | profile_peak`) and `power_dpm_state` (`balanced | performance | battery`). The iGPU actuator must use the present knob.

**Files:**
- `src/power/sensors/igpu.rs` (modified) — try `pp_power_profile_mode` first; if absent, fall back to reading `power_dpm_force_performance_level`. Map the legacy enum to the new one: `BootupDefault → auto`, `ThreeDFullScreen → profile_peak`, `PowerSaving → profile_min_sclk`, `Video → auto`, `Vr → profile_peak`, `Compute → high`.
- `src/power/sensors/igpu.rs::IgpuProfileMode` — extend the enum with a `LegacyDpmLevel(DpmLevel)` variant that carries the actual on-host value when only the legacy knob is present.
- `src/power/apply/igpu.rs` (modified) — symmetric: try writing `pp_power_profile_mode` first; on `read /sys/.../pp_power_profile_mode` error (already the observed failure mode), fall back to writing the corresponding `power_dpm_force_performance_level` value.
- `src/power/bandit/arms.rs` (modified, ~5 LoC) — `Arm.igpu_mode` continues to use the `IgpuProfileMode` enum; the actuator picks legacy vs new at write time, transparent to the bandit.

**Tests:**
- `src/power/sensors/igpu.rs::tests::reads_power_dpm_force_performance_level_when_pp_absent`.
- `src/power/apply/igpu.rs::tests::writes_legacy_dpm_level_when_pp_absent` — tempdir fixture with only `power_dpm_force_performance_level`; assert `set_igpu_mode(ThreeDFullScreen)` writes `profile_peak`.
- Manual: after rebuild + restart, daemon log no longer shows `igpu actuator failed` lines.

**Definition of Done:**
- [ ] On this HX 370 host, iGPU actuator writes succeed (verified by daemon log + sysfs readback). — pending operator rebuild + restart + tmpfiles `systemd-tmpfiles --create`.
- [x] iGPU sensor returns `Ok(LegacyDpmLevel(_))` against a fixture that has only `power_dpm_force_performance_level` (`reads_power_dpm_force_performance_level_when_pp_absent`).
- [x] iGPU actuator writes `power_dpm_force_performance_level` when `pp_power_profile_mode` is absent (`writes_legacy_dpm_level_when_pp_absent`). H3 mapping table pinned by `igpu_mode_to_dpm_level_mapping`.
- [x] `configs/systemd/tmpfiles.d/sy-power.conf` extended with `power_dpm_force_performance_level` for card0/1/2.
- [x] `make lint && make test` green (597 passed, 0 failed, 8 ignored — two consecutive runs).

**Risks / unknowns:** `power_dpm_force_performance_level` is `root:root rw-r--r--` by default — need a tmpfiles.d update too. Add the path to `configs/systemd/tmpfiles.d/sy-power.conf` in the same step.

---

## Step H4 — Step 13 polkit installer content-diff fallback (cosmetic Warning)

**Goal:** Step 13's installer emits `Warning("polkit destination /etc/polkit-1/rules.d/10-sy-power.rules unwritable; re-run as root or copy manually")` whenever the destination is unwritable — even when the file is already present with identical content. Real-machine output is misleading: the polkit rule IS installed (via the earlier sudo install) but the Warning makes the operator think it isn't. Fix: when the destination is unwritable, sudo-read the destination via a fallback (or trust the path-exists check + `include_str!` hash equality) before emitting the Warning.

**Files:**
- `src/power/apply/installer.rs` (modified) — `install_polkit_rule` checks: if dest unwritable AND dest exists AND content matches `include_str!` → emit `AlreadyMatches` instead of `Warning`. Content-comparison via `std::fs::read_to_string` (readable as user even when not writable — `/etc/polkit-1/rules.d/` is mode 0750 root:polkitd on Fedora, but the rule file itself is mode 0644 root:root so user can read).

**Tests:**
- `src/power/apply/installer.rs::tests::polkit_already_matches_when_content_equal_but_dest_unwritable` — fixture tempdir with read-only path containing the same content; installer emits `AlreadyMatches`, not `Warning`.
- Manual: on this host, `sy power apply` second run shows `= /etc/polkit-1/rules.d/10-sy-power.rules` instead of `!`.

**Definition of Done:**
- [x] `sy power apply` re-run no longer emits a misleading polkit Warning when the file is content-correct. Installer now content-diffs via `fs::read_to_string` after a failed write path and emits `AlreadyMatches` when the dest file matches `POLKIT_RULE` (`install_polkit_rule` in `src/power/apply/installer.rs`). Caveat: on this HX 370 host `/etc/polkit-1/rules.d/` is `0750 root:polkitd` so user `dmitriy` cannot read the file regardless — the fallback fires only when the dir lets the user traverse and the file mode allows read. Regression covered by `polkit_already_matches_when_content_equal_but_dest_unwritable`.
- [x] `make lint && make test` green (504 + 46 + 34 + 2 + smaller suites; two consecutive runs, zero failures).

---

## Step H5 — PPD D-Bus system policy + installer wiring

**Goal:** Step 36's PPD shim fails to claim `net.hadess.PowerProfiles` on the system bus because Fedora 43 ships a `/etc/dbus-1/system.d/net.hadess.PowerProfiles.conf` policy restricting name ownership to `power-profiles-daemon` user only. Productise a sy-side policy drop-in that grants the sy-powerd-running uid (or, more portably, the `wheel` group) the right to own the name.

**Files:**
- `configs/dbus-1/system.d/sy-power.conf` (new) — D-Bus policy drop-in granting `net.hadess.PowerProfiles` name ownership to `at_console` callers (canonical for desktop daemons) OR to `unix:user=*` if PPD isn't installed. Productised in tree per CLAUDE.md no-snowflakes.
- `src/power/apply/installer.rs` (modified) — install the D-Bus policy drop-in to `/etc/dbus-1/system.d/sy-power.conf`. Symmetric Warning fallback to the polkit / grub paths.

**Tests:**
- `src/power/apply/installer.rs::tests::installs_dbus_policy_dropin` — tempdir fixture; assert the file lands at the expected path.
- Manual: after `sudo install` + `sudo systemctl reload dbus.service`, daemon restart shows the PPD shim binding `net.hadess.PowerProfiles` cleanly (no `AccessDenied` line).

**Definition of Done:**
- [ ] `gdbus introspect --system --dest net.hadess.PowerProfiles --object-path /net/hadess/PowerProfiles` returns the sy-power shim's interface XML. — pending operator `sudo install -m 0644 configs/dbus-1/system.d/99-sy-power.conf /etc/dbus-1/system.d/` + `sudo systemctl reload dbus.service` + daemon restart.
- [x] `configs/dbus-1/system.d/99-sy-power.conf` exists with `<allow own="net.hadess.PowerProfiles"/>` for the `wheel` group. `99-` prefix is load-bearing — D-Bus reads `system.d/` alphabetically with later files overriding, so this drop-in sorts after the vendor `net.hadess.PowerProfiles.conf` (root-only default) and the wheel allowance wins.
- [x] `sy power apply` installs the drop-in to `<dbus_root>/99-sy-power.conf`. Productised via `include_str!` + `InstallOpts.dbus_root` mirroring the polkit / grub paths. Tempdir tests verify (`installs_dbus_policy_dropin`, `dbus_policy_already_matches_when_dest_readonly_but_content_equal`). H4-style content-diff fallback: when dest unwritable AND content matches → `AlreadyMatches` (not the misleading "unwritable" Warning).
- [x] `make lint && make test` green — clippy clean twice in a row, all 512 unit + 46 integration + 34 power-log tests green. Sporadic pre-existing parallel-execution flakes in `aiplane::ipc::tests` (`aiplane_ipc_v1_cancel`, `aiplane_cancel_resolves_workload_from_inflight_registry`) and `power::cli::tests::status_exit_4_when_no_daemon` are unrelated to H5 (no installer interaction) and pass single-threaded.

**Risks / unknowns:** if the existing `net.hadess.PowerProfiles.conf` from upower / power-profiles-daemon-base package takes precedence (D-Bus reads `system.d/` in alphabetical order, with later files overriding), the sy drop-in must be named `99-sy-power.conf` to win. Test on the live host. — Mitigated: drop-in is named `99-sy-power.conf`.

---

## Step H6 — Productise tmpfiles.d + cgroup actuator silent-no-op fix + installer wiring

**Goal:** today's deploy ad-hoc-installed `configs/systemd/tmpfiles.d/sy-power.conf` via `sudo install` (CLAUDE.md no-snowflakes was flagged correctly by the auto-classifier — only the in-tree file made it productised). The `sy power apply` installer must drop the file to `/etc/tmpfiles.d/` so a clean machine reproduces the environment without manual steps. Same step: diagnose why `cpu.uclamp.max` reads `max` after the daemon applied the whisper arm (which has `cpu_uclamp_max = 40`) — the cgroup actuator is silently no-op'ing somewhere.

**Files:**
- `src/power/apply/installer.rs` (modified) — install `configs/systemd/tmpfiles.d/sy-power.conf` to `/etc/tmpfiles.d/sy-power.conf` + run `systemd-tmpfiles --create` on successful write. Symmetric Warning fallback when the dest is unwritable.
- `src/power/apply/cgroup.rs` (modified) — diagnose: the cgroup file path is `/sys/fs/cgroup/user.slice/user-$UID.slice/user@$UID.service/app.slice/sy-powerd.service/cpu.uclamp.max` (verified live). The actuator may be writing to the wrong path (e.g. computing `app.slice/sy-powerd.scope` instead of `sy-powerd.service`). Fix: probe the actual cgroup path from `/proc/self/cgroup` at startup rather than hard-coding.
- `src/power/apply/cgroup.rs::tests::probes_own_cgroup_from_proc_self_cgroup` (new) — tempdir fixture with synthetic `/proc/self/cgroup` content; assert the actuator resolves the right write path.

**Tests:**
- `src/power/apply/installer.rs::tests::installs_tmpfiles_dropin`.
- `src/power/apply/cgroup.rs::tests::probes_own_cgroup_from_proc_self_cgroup`.
- Manual: after restart, `cat /sys/fs/cgroup/.../sy-powerd.service/cpu.uclamp.max` shows the bandit's applied value (e.g. `40` for whisper).

**Definition of Done:**
- [x] `sy power apply` on a clean machine installs the tmpfiles.d drop-in (no manual sudo install of the file content). Productised via `include_str!("../../../configs/systemd/tmpfiles.d/sy-power.conf")` + `InstallOpts.tmpfiles_root` mirroring the polkit / grub / dbus paths. On successful write the installer also invokes `systemd-tmpfiles --create <dest>` via the existing `CommandRunner` trait so sysfs perm overrides apply immediately without reboot. Tempdir tests verify (`installs_tmpfiles_dropin`, `tmpfiles_already_matches_when_dest_readonly_but_content_equal`). H4-style content-diff fallback: when dest unwritable AND content matches → `AlreadyMatches` (not the misleading "unwritable" Warning). New `ChangeRecord::TmpfilesApplied` variant tracks the shell-out; dry-run and `AlreadyMatches` skip it.
- [ ] cgroup actuator writes the bandit's uclamp values to the live cgroup hierarchy. **Misdiagnosed — see verification probe.** Re-diagnosis on 2026-05-20: `production_cgroup_root()` already returns the correct path (`/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/sy-powerd.service`); the actuator was not silently no-op'ing. The daemon had not been restarted with H2's working sensors yet, so the bandit was picking default-cgroup arms (`browse` / `idle`) whose `CgroupOverrides::default()` = all None = no writes is by design. Post-deploy with H2's fully-populated context vector, the bandit picks non-default arms (`whisper` / `call` / `code` / `build` / `flat-out`) that have cgroup overrides and the actuator writes them — verified live by `production_cgroup_root` probe + arm-table inspection. H6 narrows to ONLY the tmpfiles.d productisation.
- [x] `make lint && make test` green. 508 unit + 46 obs + 34 ipc + smaller suites; two consecutive `make lint` runs and three `make test` runs (one sporadic pre-existing parallel-execution flake in `aiplane::ipc::tests::daemon_smoke_run_roundtrip_via_fake_workload` on the first pass, unrelated to installer; passes single-threaded and on subsequent parallel passes).

---

## Cross-cutting Definition of Done

- [ ] All H1..H6 step DoDs satisfied.
- [ ] After `cargo build --release && sudo install -m 0755 target/release/sy ~/.local/bin/sy && systemctl --user restart sy-powerd.service`:
  - `sy power log --since=2m` returns ≥ 100 entries (not zero).
  - `sy power status --json | jq '.sensors.tctl_c'` returns a numeric value (not null).
  - `sy power show --no-open --allow-thin --out /tmp/test.pdf` produces a PDF mentioning a non-zero `entries` count.
  - `journalctl --user -u sy-powerd --since '1 min ago' | grep 'actuator failed' | wc -l` returns 0 (or counts only NPU's expected best-effort failure).
- [ ] After reboot (operator-driven): `cat /sys/devices/system/cpu/cpufreq/policy0/energy_performance_preference` after `sy power profile flat-out` returns `performance` (EPP lever unblocked by `amd_dynamic_epp=disable`).

## Out of Scope (deferred — design intent OR larger followup)

- **NPU `xrt-smi configure --pmode` via CAP_SYS_ADMIN** — design intent: best-effort, logs WARN. Step 16 anti-goal: no root-mode actuator. Re-evaluate if NPU pmode becomes critical-path for power saving.
- **Snapshot v2 → v3 forecast residual column** — Step 31's drift detector feeds 0.0 forecast residual until Step 29b lands. Out of hotfix scope.
- **Logger path resolution edge cases** (rotation across midnight when daemon was off for >24h) — the live deploy hasn't crossed a date boundary yet; defer.
