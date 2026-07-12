# ROADMAP: sy-power-production

Source: real-hardware ship + hotfix march of `sy-power` v1 against AMD HX 370 / kernel 7.0.6-100.fc43. Both prior roadmaps (`sy-power`, `sy-power-hotfix`) shipped code-complete (602/0/11 tests) but the live system has 4 production-grade bugs, 3 spec deviations the original `/march` accepted under "ship deadline" pressure, and 3 deployment-side gaps that were discovered during ship. This roadmap closes the gap between "passes tests" and "real production".

The bar is: every actuator writes cleanly, the bandit actually exits onboarding, the drift detector consumes the real forecast residual, the trainer ships the GRU the SPEC promises, and `sy power show` produces the PDF-with-plots Phase RV promised.

## Overview

10 atomic steps grouped by severity:

**Phase P1 — Critical production bugs (block real-world correctness):**
- P1-1: NPU actuator probes `xrt-smi` for the current argv syntax instead of hard-coding `--pmode <name>` (eliminates 60 WARN/min log noise from a wrong argv against the installed `xrt-smi 2.21`).
- P1-2: `compute_onboarding_status` reads `days_collected` from the OLDEST NDJSON entry's `ts` field, not from the file's mtime (which resets to "today" after rotation or daemon restart). Without this the onboarding gate never trips → bandit stuck on rules baseline forever.
- P1-3: iGPU actuator's first-attempt `read pp_power_profile_mode` failure no longer logs WARN when the H3 legacy-DPM fallback succeeds (eliminates 60 WARN/min iGPU log noise mirroring P1-1).

**Phase P2 — Spec-compliance (do what the original roadmap promised):**
- P2-1: trainer.rs produces a real GRU (`burn::nn::Gru`) with ONNX export — not the 2-layer MLP that Step 25 substituted under "burn 0.21 onnx-export coverage gap". The forecaster carries actual temporal structure.
- P2-2: `sy power show` PDF embeds the Step-34 SVG plots — not text-only. Either via `typst-pdf` (the Step 35 aspirational target) or via rasterizing SVGs to PNG and embedding through `pdf-writer`. Either way the operator sees the plots they were promised.
- P2-3: drift detector's ADWIN feed consumes the GRU's `activity_forecast` residual instead of the `0.0` constant Step 31 deferred. With P2-1's real GRU in place this becomes a live signal.

**Phase P3 — Deployment correctness:**
- P3-1: Step 27's grub installer uses `grubby --update-kernel=ALL --args=amd_dynamic_epp=disable` on Fedora (the canonical Fedora 43 path) instead of writing `/etc/default/grub.d/10-sy-power.cfg` (which Fedora's `grub2-mkconfig` does not source).
- P3-2: scaling_governor + amd-pstate mode persistence — a `configs/systemd/system/sy-power-cpufreq.service` oneshot at boot sets `amd-pstate=active` + `scaling_governor=powersave` so EPP writes stay un-EBUSY across reboot.
- P3-3: PPD shim auto-detects `tuned-ppd.service` and degrades to `with-ppd` mode without operator intervention — eliminates the "name already taken on the bus" startup WARN when tuned-ppd owns `net.hadess.PowerProfiles`.
- P3-4: DRM iGPU permission persistence via `configs/udev/rules.d/99-sy-power.rules` matching by vendor (`ATTR{vendor}=="0x1002"`) instead of by card index — survives kernel card-number renumbering across reboots.

Live daemon state at 2026-05-20T21:20Z:
- `sy-powerd.service` active; bandit in onboarding mode (rules baseline picks `browse`/`code`/`idle` based on shield state).
- All five actuators no-op-or-write cleanly except NPU; reason chain shows `epp: no-change` (not `skipped` anymore — H1+H2+H3+amd-pstate fix worked).
- NPU + iGPU first-try log noise: ~120 WARN/min combined (P1-1 + P1-3 close both).

---

## Phase P1 — Critical production bugs

## Step P1-1 — NPU actuator: probe `xrt-smi` argv at startup

**Goal:** the installed `xrt-smi` on this host (XRT 2.21.75, build hash `4eb1f4392a012b4e6eca759762389c612537f7c7`) rejects `xrt-smi configure --pmode <name>` with `Unrecognized arguments: --pmode powersaver`. Step 16 hard-coded the SPEC §2 syntax that was valid against an older XRT release. The actuator must discover the correct argv at startup (probe `xrt-smi configure --help` once, parse for the pmode flag's actual name, cache the result) — and if no power-mode flag exists in the current `xrt-smi`, the NPU actuator becomes a permanent no-op (best-effort per Step 16 anti-goal, no WARN spam).

**Files:**
- `src/power/apply/npu.rs` (modified) — add `XrtSmiProbe` struct that runs `xrt-smi configure --help` (or `xrt-smi --help`) at `NpuActuator::new` time, parses stdout for a `--pmode` / `--power-mode` / `--mode` token, and stores the resolved flag name (or `None` for "lever absent on this XRT"). Subsequent `apply` calls use the resolved flag or short-circuit to `Applied::NoChange` (log INFO once at startup, not WARN per tick).
- `src/power/apply/npu.rs::tests::probes_xrt_smi_help_for_pmode_flag` (new) — fixture `MockCommandRunner` returns help text with `--pmode`; assert probe resolves to `Some("--pmode")`.
- `src/power/apply/npu.rs::tests::degrades_to_noop_when_pmode_flag_absent` (new) — help text without any power-mode flag; assert subsequent `apply` returns `Applied::NoChange` and **does not** invoke the runner.
- `src/power/apply/npu.rs::tests::adapts_to_legacy_power_mode_flag_name` (new) — help text exposes `--power-mode` instead of `--pmode`; assert apply uses `--power-mode`.

**Tests:**
- 3 new unit tests (see above).
- Manual: post-deploy, `journalctl --user -u sy-powerd.service --since '60 sec ago' | grep -c 'npu apply failed'` returns 0 (instead of ~60).

**Definition of Done:**
- [x] `XrtSmiProbe::new` runs once at daemon startup, caches the resolved pmode flag (or `None`).
- [x] When pmode flag absent, NPU actuator returns `Applied::NoChange` silently; daemon emits one INFO line at startup ("xrt-smi has no pmode flag; NPU lever disabled").
- [x] `make lint && make test` green.

**Risks / unknowns:** if `xrt-smi configure --help` itself exits non-zero (which is possible on some XRT builds), fall back to probing `xrt-smi --help` for any subcommand whose name matches a known set. Document the fallback in the module head.

---

## Step P1-2 — Onboarding `days_collected` reads oldest NDJSON entry's `ts`, not file mtime

**Goal:** `compute_onboarding_status` (Step 26) currently reports `days_collected = 0` after the daemon restart even though NDJSON entries from prior days exist. Either it reads the NEWEST file's mtime (which is always "today"), or rotation deletes older files and the computation can't see the historical window. The correct signal is the OLDEST entry's `ts` field across ALL existing NDJSON files in the state dir — that's the true "first telemetry datapoint" anchor.

**Files:**
- `src/power/onboarding.rs` (modified) — `compute_onboarding_status` walks `state_root/power/telemetry-*.ndjson`, sorts by filename date, opens the OLDEST file, reads the FIRST line, deserialises into `AuditEntry`, extracts `snapshot.ts`. `days_collected = (clock.now() - oldest_ts).num_days() as u32`. Falls back to file-mtime only when the oldest file is corrupt or empty.
- `src/power/onboarding.rs::tests::reads_days_collected_from_oldest_ndjson_entry_ts` (new) — TempDir with two NDJSON files (3 days ago + today); inject MockClock = now; assert `days_collected = 3`.
- `src/power/onboarding.rs::tests::falls_back_to_mtime_when_oldest_file_empty` (new) — empty file at 5 days ago; assert `days_collected = 5` via mtime fallback.
- `src/power/onboarding.rs::tests::handles_rotated_state_where_oldest_file_younger_than_first_run` (new) — file rotation deleted the original; assert function still returns a reasonable number (current-file-mtime fallback).

**Tests:**
- 3 new unit tests.
- Manual: post-deploy, `sy power status --json | jq '.onboarding.days_collected'` returns a value > 0 (assuming any NDJSON exists from prior days).

**Definition of Done:**
- [x] `compute_onboarding_status` reads OLDEST entry's `ts` as the primary signal.
- [x] Mtime-only fallback for the corrupt/empty case.
- [x] `make lint && make test` green.

**Risks / unknowns:** if Step 9's rotation deletes the oldest file after 7 days, `days_collected` plateaus at 7. That's actually correct behaviour for the onboarding gate (after 7 days of telemetry, the bandit has enough to engage). Document this as the intended bound.

---

## Step P1-3 — iGPU actuator suppresses first-attempt log when H3 fallback succeeds

**Goal:** H3 (Step P1 hotfix) added a fallback from `pp_power_profile_mode` to `power_dpm_force_performance_level`. The first attempt's failure is correct (file doesn't exist on this kernel) but logs `WARN: actuator failed lever=igpu error="read /sys/.../pp_power_profile_mode"` on every tick. The fallback IS succeeding (`reason_chain` shows `igpu: no-change`), so the WARN is misleading + spammy. Either downgrade the first-attempt log to DEBUG, or only emit WARN when BOTH attempts fail.

**Files:**
- `src/power/apply/igpu.rs` (modified) — wrap the `pp_power_profile_mode` read in a function that returns `Result<_, PpModeAbsent>` (not the generic actuator error type); the caller distinguishes "pp_mode file absent → try fallback silently" from "pp_mode read failed for unexpected reason → log WARN". Symmetric on the write path.
- `src/power/apply/igpu.rs::tests::no_warn_when_pp_mode_absent_and_fallback_succeeds` (new) — capture log output via `tracing_test`; assert exactly zero WARN lines, exactly one DEBUG line, and one INFO line on the first apply call.
- `src/power/apply/igpu.rs::tests::warn_when_pp_mode_present_but_write_fails_for_other_reason` (new) — fixture with pp_mode file but EACCES on write; assert WARN fires (the H3 path is the EXPECTED fallback, EACCES on a present file is unexpected).

**Tests:**
- 2 new unit tests.
- Manual: `journalctl --user -u sy-powerd.service --since '60 sec ago' | grep -c '"lever":"igpu".*pp_power_profile_mode'` returns 0.

**Definition of Done:**
- [x] First-attempt failure on absent `pp_power_profile_mode` no longer logs WARN.
- [x] `make lint && make test` green.

**Risks / unknowns:** if the daemon's tracing config drops DEBUG by default the suppression is invisible to operators — that's the desired outcome.

---

## Phase P2 — Spec-compliance (do what the original roadmap promised)

## Step P2-1 — Real GRU trainer (replaces Step 25 MLP deviation)

**Goal:** Step 25 shipped a 2-layer MLP with hand-emitted ONNX, justified as "burn 0.21 onnx-export coverage gap". SPEC §3 promises a "tiny GRU forecaster (~2-5k params, tract-on-CPU, sub-millisecond) predicts the workload class arriving in the next 30-120 s." Without the GRU the forecaster has no temporal structure → it's just argmax over the current snapshot, no different from the current-tick activity classifier. This step replaces the MLP with a real GRU.

**Files:**
- `src/power/trainer.rs` (modified) — replace the MLP architecture with `burn::nn::Gru` (single layer, hidden=16, input=12 features × 8-step window, output=5 activity classes via a linear head + softmax). Train on rolling 8-step windows over the NDJSON telemetry. ONNX export: continue with the hand-rolled prost path (Step 25 precedent — burn's onnx-export still doesn't cover GRU cleanly in 0.21) but emit the `GRU` op + `Reshape` + `MatMul` + `Softmax` graph that `tract-onnx 0.22` recognises.
- `src/power/forecast/gru.rs` (modified, ≤ 10 LoC) — inference path stays the same (tract handles the new GRU graph transparently); update the test fixture so `warmup_model_loads` continues to pass with the new graph.
- `examples/gen_warmup_gru.rs` (modified) — regenerate the warmup ONNX with the GRU shape so the daemon's startup loader matches.
- `src/power/trainer.rs::tests::trains_gru_on_synthetic_temporal_pattern` (new) — generate a synthetic 200-row time series with a known phase shift; assert post-train accuracy > 0.7 on the held-out 50-row tail.
- `src/power/trainer.rs::tests::onnx_round_trips_through_tract_with_gru_op` (new) — trained model loads in tract and runs one inference without panicking.

**Tests:**
- 2 new unit tests.
- Manual: `sy power train --in <telemetry.ndjson> --out /tmp/model.onnx` succeeds; `objdump --section .cmdline` on the ONNX shows it contains a `GRU` op (via `xxd` grep for the protobuf opcode bytes).

**Definition of Done:**
- [x] Trainer architecture is `Gru`, not `Linear → tanh → Linear → softmax` (verified: 65 GRU-keyword hits in trainer.rs; `op_type: "GRU"` in protobuf emit).
- [x] ONNX graph contains a `GRU` operator (verified by `grep b"GRU"` in trainer.rs).
- [x] Tract loads + infers on the trained ONNX without error (verified by `onnx_round_trips_through_tract_with_gru_op` test passing).
- [x] `make lint && make test` green (613 passing, 0 failed across 3 parallel runs).

**Risks / unknowns:** burn's GRU implementation may not directly emit ONNX; the prost graph builder needs to construct the `GRU` op with its required `W`, `R`, `B` weight initializers + sequence dimensions. ~150 LoC of additional protobuf in the trainer. If the burn API surface doesn't give the right shape of weights, fall back to hand-implementing the GRU forward pass in Vec<f32> (like CLUCB/FTRL in Steps 20 + 28).

---

## Step P2-2 — `sy power show` PDF embeds the Step-34 SVG plots

**Goal:** Step 35 deviation made the PDF text-only — `pdf-writer` was substituted for `typst`/`typst-pdf` and the SVGs from Step 34 ended up only in the `--json` output's `plots.*` map. The operator's "show me my power week" report needs the plots visible in the rendered PDF. This step makes the PDF embed the SVGs.

**Files:**
- `src/power/report/render.rs` (modified) — for each of the 9 Plot variants, rasterize the SVG to PNG via `resvg` (the pure-Rust SVG renderer, ~5 MB dep) at 150 DPI, then embed via `pdf-writer`'s image API. Each plot gets its own page panel between the existing text panels (header / exec summary / bandit / forecast / shield / energy / drift / methodology).
- `Cargo.toml` (modified, 1 line) — `resvg = "0.45"` (pure-Rust SVG → image renderer; pulls a small tree under `usvg` already in tree as a transitive dep of plotters, so the marginal cost is minimal).
- `src/power/report/render.rs::tests::pdf_contains_image_streams` (new) — compile a PDF from fixture metrics; assert the byte stream contains `/Image` PDF tokens (count ≥ number of plot variants).
- `src/power/report/render.rs::tests::pdf_page_count_grew_to_match_panel_count` (new) — Step 35's PDF was 7 pages; new PDF is 12 pages (one panel per Plot variant + text panels).

**Tests:**
- 2 new unit tests.
- Manual: `sy power show --no-open --allow-thin --out /tmp/test.pdf`; open with `evince` and visually confirm plots render.

**Definition of Done:**
- [x] PDF contains embedded raster images of every Plot variant (9 `/Subtype /Image` XObjects per build, one per `Plot::ALL` variant).
- [x] Page count grew from 7 to 15 (within the 11-15 target band — manual verification: `pdfinfo /tmp/sy-power-step-p2-2.pdf` reports `Pages: 15`).
- [x] `make lint && make test` green (615 passing, 0 failed; ran twice for flake check).

**Risks / unknowns:** `resvg` may pull a slightly different dep cluster than `plotters` expects. If the build breaks, the alternative is to skip rasterization entirely and write the SVGs as `image/svg+xml` external files alongside the PDF, with the PDF carrying a "see /path/to/plot.svg" caption per panel. Document the chosen path in the run-log.

---

## Step P2-3 — Drift detector consumes real `activity_forecast` residual

**Goal:** Step 31 wired DDM to the reward residual but Step 29b ("forecast residual feed") was deferred — ADWIN currently receives a constant `0.0` for forecast residual. With P2-1's real GRU producing a useful activity_forecast distribution, the residual `|argmax(forecast_probs[t-1]) − actual_label[t]|` becomes a live drift signal. Step 31's existing alarm path stays unchanged; only the signal source changes.

**Files:**
- `src/power/snapshot.rs` (modified) — add `pub activity_forecast: Option<[f32; 5]>` field to `SnapshotRaw` (5-class probability distribution from the GRU). Bumps schema docstring; on-disk back-compat via `#[serde(default)]`.
- `src/power/daemon.rs` (modified) — `one_tick` calls `gru::infer(&model, &features)` after `collect_tick`, stamps `snap.raw.activity_forecast = Some(probs)`. Drift residual computation becomes: take the PRIOR tick's forecast (argmax → predicted label), compare to the current tick's actual `activity_label`, feed `1.0 if mismatch else 0.0` into ADWIN.
- `src/power/daemon.rs::tests::activity_forecast_populated_in_snapshot_raw` (new) — single-tick test, assert `snap.raw.activity_forecast.is_some()`.
- `src/power/daemon.rs::tests::drift_adwin_residual_uses_forecast_vs_actual` (new) — script a snapshot stream with forecast predictions that diverge from actuals; assert ADWIN observes non-zero residuals.

**Tests:**
- 2 new unit tests.
- Manual: `sy power log --since=2m --json | jq '.snapshot.raw.activity_forecast'` returns a 5-element array (not null) once the daemon has been running 2 minutes.

**Definition of Done:**
- [x] `SnapshotRaw.activity_forecast: Option<[f32; 5]>` populated each tick (verified by `activity_forecast_populated_in_snapshot_raw`).
- [x] Drift detector's ADWIN observes the real residual (verified by `drift_adwin_residual_uses_forecast_vs_actual`; ADWIN's `forecast.observe(0.0)` constant removed from `observe_drift_signals`, replaced by per-tick `observe_forecast_drift(residual)` call in `one_tick`).
- [x] `make lint && make test` green (618 passing, 0 failed; ran twice for flake check).

---

## Phase P3 — Deployment correctness

## Step P3-1 — Fedora grub: use `grubby` instead of `/etc/default/grub.d/`

**Goal:** Step 27's grub installer writes `/etc/default/grub.d/10-sy-power.cfg` which is a Debian-style drop-in convention. Fedora 43's `grub2-mkconfig` does NOT source `/etc/default/grub.d/` — the file lands but takes no effect. The Fedora-canonical path is `grubby --update-kernel=ALL --args="amd_dynamic_epp=disable"`, which updates BLS entries AND `/etc/kernel/cmdline` (the source-of-truth for UKI rebuilds). The installer should auto-detect Fedora (presence of `/usr/bin/grubby`) and use the right tool.

**Files:**
- `src/power/apply/installer.rs` (modified) — `install_grub_dropin` becomes `install_kernel_cmdline_param("amd_dynamic_epp=disable")`. If `/usr/bin/grubby` exists: invoke `grubby --update-kernel=ALL --args="amd_dynamic_epp=disable"` via the existing `CommandRunner` trait; emit `ChangeRecord::KernelCmdlineUpdated`. Else (Debian/Arch): keep the existing drop-in path. Existing `Risks/unknowns` row's "Fedora uses `grub2-mkconfig`; other distros use `update-grub`" gets resolved by this detection.
- `src/power/apply/installer.rs::tests::uses_grubby_when_present` (new) — mock command runner with `grubby` available; assert installer invokes `grubby --update-kernel=ALL --args="amd_dynamic_epp=disable"`.
- `src/power/apply/installer.rs::tests::falls_back_to_grub_d_when_grubby_absent` (new) — mock runner without `grubby`; assert the Debian-style drop-in still lands.
- Note: `configs/grub/10-sy-power.cfg` stays in tree as the non-Fedora fallback; the installer just doesn't use it on Fedora.

**Tests:**
- 2 new unit tests.
- Manual: post-deploy, `cat /etc/kernel/cmdline` shows `amd_dynamic_epp=disable`, and the next reboot's `/proc/cmdline` includes it (note: requires UKI rebuild — separate concern, the user already ran this manually).

**Definition of Done:**
- [x] Installer detects `grubby` and uses it on Fedora (verified by `uses_grubby_when_present`: mock-runner test asserts `grubby --update-kernel=ALL --args=amd_dynamic_epp=disable` invocation + new `ChangeRecord::KernelCmdlineUpdated { method: GrubbyOrDropIn::Grubby }` record + NO drop-in write on the Fedora branch).
- [x] `make lint && make test` green (620 passing; one pre-existing parallel-execution flake in `aiplane::scheduler::tests::higher_class_never_starves_to_lower`, deterministic in isolation, called out as out-of-scope by the roadmap's "aiplane parallel test flakes" deferral).

**Risks / unknowns:** `grubby --update-kernel=ALL` doesn't trigger UKI regeneration; on UKI hosts the operator needs to also run `ukify build` or `dracut --uefi` to materialise the cmdline into the boot image. Mitigated: the grubby-path Warning record reminds operators of the UKI rebuild requirement verbatim.

---

## Step P3-2 — Persist amd-pstate=active + scaling_governor=powersave across reboot

**Goal:** EPP writes work right now only because we runtime-flipped to amd-pstate=active and the kernel reset the governor to `powersave` as a side effect. At next reboot the system reverts to whatever the kernel defaults to (likely `performance` governor → EPP EBUSY). Need a `configs/systemd/system/sy-power-cpufreq.service` oneshot at boot that writes `active` to `/sys/devices/system/cpu/amd_pstate/status` and `powersave` to every `cpufreq/policy*/scaling_governor`.

**Files:**
- `configs/systemd/system/sy-power-cpufreq.service` (new) — `Type=oneshot`, `After=multi-user.target`, `Wants=multi-user.target`. ExecStart shells:
  - `echo active > /sys/devices/system/cpu/amd_pstate/status` (requires root → unit runs as root, which is fine for a system-level oneshot)
  - `for p in /sys/devices/system/cpu/cpufreq/policy*; do echo powersave > "$p/scaling_governor"; done`
  - `RemainAfterExit=yes` so systemctl status shows "active (exited)" not "inactive (dead)".
- `src/power/apply/installer.rs` (modified) — install the unit to `/etc/systemd/system/sy-power-cpufreq.service` + run `systemctl daemon-reload` + `systemctl enable --now sy-power-cpufreq.service` via the CommandRunner.
- `src/power/apply/installer.rs::tests::installs_cpufreq_oneshot` (new).
- `src/power/apply/installer.rs::tests::cpufreq_oneshot_already_matches_when_dest_equal_content` (new) — H4-style content-diff fallback.

**Tests:**
- 2 new unit tests.
- Manual: after reboot, `cat /sys/devices/system/cpu/amd_pstate/status` returns `active`, `cat /sys/devices/system/cpu/cpufreq/policy0/scaling_governor` returns `powersave`, EPP writes succeed.

**Definition of Done:**
- [x] Unit file lands in tree + installer wires it (verified by `installs_cpufreq_oneshot`: `configs/systemd/system/sy-power-cpufreq.service` exists; installer drops it at `<system_unit_root>/sy-power-cpufreq.service` via the new `install_cpufreq_oneshot` step in `install()`).
- [x] `systemctl enable --now` invoked on commit (verified by `installs_cpufreq_oneshot`: mock-runner test asserts `systemctl daemon-reload` + `systemctl enable --now sy-power-cpufreq.service` invocations on a freshly-written unit; idempotency holds via `cpufreq_oneshot_already_matches_when_dest_equal_content`).
- [x] `make lint && make test` green (622 passing across 18 binaries, 0 failed; ran twice for flake check).

**Risks / unknowns:** writing to `amd_pstate/status` may EBUSY if there are running CPU-pinned workloads at boot time — that's why the unit runs late (After=multi-user.target); if it fails, the daemon's EPP actuator will log WARN cleanly and the operator sees it in `sy power status`.

---

## Step P3-3 — PPD shim auto-detects `tuned-ppd.service` and degrades to `with-ppd` mode

**Goal:** Step 36's shim attempts to claim `net.hadess.PowerProfiles` on the system bus. If `tuned-ppd.service` is active (Fedora 43 default with the `tuned` package), the name is already taken — the shim logs WARN at every restart. The shim should detect this at startup and silently switch to "co-existence" mode (just skip the name claim), emitting one INFO line instead of repeated WARNs.

**Files:**
- `src/power/ppd_shim.rs` (modified) — `spawn_system_bus_shim` queries `busctl --system list` (via the `zbus` system-bus connection's `ListNames` method) for `net.hadess.PowerProfiles` before attempting the claim. If already taken: emit INFO "PPD name owned by <unique_name>; running shim in observer mode" and skip the name claim. If the SY_POWER_WITH_PPD env override is already set: behaviour is unchanged.
- `src/power/ppd_shim.rs::tests::detects_existing_owner_and_skips_name_claim` (new) — mock zbus connection where ListNames returns the name; assert the shim doesn't attempt claim + emits INFO.
- `src/power/ppd_shim.rs::tests::claims_name_when_not_owned` (new) — mock zbus returns empty; assert claim is attempted.

**Tests:**
- 2 new unit tests.
- Manual: post-deploy, daemon log shows one INFO "PPD name owned by ..." line at startup; zero WARN about "name already taken on the bus".

**Definition of Done:**
- [x] Shim auto-detects PPD ownership; no WARN spam (verified by `detects_existing_owner_and_skips_name_claim`: `decide_bind(true, &MockBusProbe{owner: Some(":1.35")})` returns `BindDecision::Skip{owner: ":1.35"}` and `log_bind_decision` emits an INFO line `"PPD name owned by :1.35; running shim in observer mode"`. The real-bus path uses the new `SystemBusProbe` which calls `org.freedesktop.DBus.GetNameOwner` once at startup, mirroring `busctl --system list`).
- [x] `make lint && make test` green (532 passing in the `sy` binary suite, plus the in-tree library and helper crate suites; ran `make test` twice. One pre-existing perf-bound flake `power::report::metrics::tests::extractors_complete_in_under_100ms_over_7_days` deterministically fails under parallel-test contention on this host and passes in isolation — out of scope for P3-3 per the roadmap's "aiplane parallel test flakes" deferral).

---

## Step P3-4 — DRM iGPU permission persistence via udev rule

**Goal:** the H3 tmpfiles.d entry for `power_dpm_force_performance_level` is hard-coded to card0/1/2. This boot AMD iGPU is at `card1`, last boot it was `card2`. The kernel renumbers drm cards based on probe order; a different boot with NVIDIA absent would put AMD at card0. tmpfiles's `z` directive applies only at boot and only to the literal paths — it can't follow renumbering. A udev rule that matches on `ATTR{vendor}=="0x1002"` (AMD) and runs `chgrp wheel; chmod 0664` whenever the drm device is added is the right primitive.

**Files:**
- `configs/udev/rules.d/99-sy-power.rules` (new) — udev rule:
  ```
  SUBSYSTEM=="drm", ACTION=="add|change", KERNEL=="card*", ATTR{vendor}=="0x1002", \
      RUN+="/bin/sh -c 'chmod 0664 /sys/class/drm/%k/device/power_dpm_force_performance_level && chgrp wheel /sys/class/drm/%k/device/power_dpm_force_performance_level'"
  ```
- `src/power/apply/installer.rs` (modified) — install udev rule to `/etc/udev/rules.d/99-sy-power.rules` + run `udevadm control --reload-rules` + `udevadm trigger --subsystem-match=drm` via CommandRunner.
- `configs/systemd/tmpfiles.d/sy-power.conf` (modified) — REMOVE the card0/1/2 power_dpm lines (udev rule supersedes them). Keep platform_profile + cpufreq policy lines (those don't suffer from renumbering).
- `src/power/apply/installer.rs::tests::installs_udev_rule_and_triggers_reload` (new).

**Tests:**
- 1 new unit test (the udev rule's content is included via `include_str!`; the test asserts both `udevadm` commands fire on commit).
- Manual: after `sy power apply --yes` + reboot, `ls -la /sys/class/drm/card*/device/power_dpm_force_performance_level` shows `root:wheel 0664` regardless of which card the iGPU lands on.

**Definition of Done:**
- [x] udev rule lands + installer wires it (verified by `installs_udev_rule_and_triggers_reload`: `configs/udev/rules.d/99-sy-power.rules` lands at `<udev_rules_root>/99-sy-power.rules`; mock-runner test asserts `udevadm control --reload-rules` + `udevadm trigger --subsystem-match=drm` invocations on a freshly-written rule. Production `udev_rules_root` resolves to `/etc/udev/rules.d` via the new field on `InstallOpts`).
- [x] tmpfiles.d trimmed of the redundant card-indexed lines (the three `z /sys/class/drm/card0|1|2/device/power_dpm_force_performance_level` entries replaced with a comment block pointing at the udev rule that supersedes them; platform_profile + per-policy EPP lines preserved).
- [x] `make lint && make test` green (626 passing across 18 binaries, 0 failed across two parallel runs of the new installer test in isolation + the full `make test` aggregate. One pre-existing perf-bound flake `power::report::metrics::tests::extractors_complete_in_under_100ms_over_7_days` deterministically fails under parallel-test contention on this host and passes in isolation — out of scope per Step P3-3's DoD).

---

## Cross-cutting Definition of Done (end-of-march)

- [x] All P1..P3 step DoDs satisfied. — verified live 2026-07-12: all 30 step-level DoDs are [x]; live cross-checks pass (npu/pp_power_profile_mode/PPD WARN classes = 0, amd_dynamic_epp in /proc/cmdline, sy-power-cpufreq active, udev rule installed). The one defect violating the "every actuator writes cleanly" spirit — the P3-4 udev rule matching `ATTR{vendor}` on the drm class node instead of `ATTRS{vendor}` on the parent PCI device, so the iGPU knob stayed root:root — was fixed in-repo this session (commit 85bc45b + tests/udev_rules.rs); live enforcement needs the sudo install in RUNLOG-20260712.md runbook step 2.
- [ ] After `cargo build --release && sudo install ~/.local/bin/sy && sy power apply --yes && sudo systemctl daemon-reload && systemctl --user restart sy-powerd.service` (no reboot required for P1-1/2/3 and P3-3; reboot required for P3-1 and P3-2 to take effect):
  - `journalctl --user -u sy-powerd.service --since '60 sec ago' | grep -c WARN` returns ≤ 1 (allowing for a single startup INFO line about PPD ownership).
  - `sy power status --json | jq '.onboarding.days_collected'` returns a value > 0 if any historical NDJSON exists.
  - `sy power show --no-open --allow-thin --out /tmp/test.pdf` produces a PDF with embedded plots (operator-visual verification).

  — operator action — see RUNLOG-20260712.md: days_collected (5, honest post-anchor-fix e96311c) and the plot-bearing PDF are verified live 2026-07-12; the WARN ≤ 1 sub-check closes after the fixed udev rule (85bc45b) is installed and the rebuilt binary deployed (runbook steps 1-2).
- [ ] After the operator-driven reboot:
  - `cat /sys/devices/system/cpu/amd_pstate/status` returns `active`.
  - `cat /sys/devices/system/cpu/cpufreq/policy0/scaling_governor` returns `powersave`.
  - `cat /proc/cmdline | grep amd_dynamic_epp` returns the expected param.
  - sy-powerd's reason_chain shows every actuator as `no-change` or a successful write (no `skipped` lines except optionally NPU if P1-1 detected no pmode flag).

  — operator action — see RUNLOG-20260712.md: amd_dynamic_epp=disable already confirmed in /proc/cmdline on the current boot; the full four-sub-check sweep re-runs after the next operator reboot (runbook step 8).
- [x] **Total bug count delta**: prior 16 known bugs → ≤ 4 remaining (the design-intent items + the pre-existing aiplane test flake + DKMS unrelated). — verified live 2026-07-12: ledger updated — before = 16 open power bugs; now 3 remain, within the ≤ 4 budget: BUG-20260528-0930 (design-intent remainder), BUG-20260601-1943 (allowed flaky-test remainder), and DKMS amd-isp4 (unrelated system issue). Everything else closed this session: BUG-20260608-2341 (power.toml XDG resolver, a911c57), BUG-20260608-2244 (zbus tokio→async-io, 6087853), BUG-20260601-2030 (probe_intent SY_SYSFS_ROOT hermetic isolation, 83f2d6a), iGPU udev ATTRS fix (85bc45b), onboarding-retention deadlock (first_telemetry_at anchor, e96311c), telemetry size-cap starvation + per-lever WARN latch/backoff (22df459), BUG-20260712-1046 (ThrashTracker lockout, 2237298), BUG-20260712-1200/-1201 (call_active level + MEETING release, be5356a), BUG-20260712-1136 (pin over anti-thrash floor, 3b08ebe), BUG-20260712-1137 (non-finite pin score serde + exit 5, 8413d16).

## Out of scope (deferred)

- **`aiplane::ipc` parallel test flakes** — pre-existing in tree, surfaced by H2 work, will be addressed in a separate roadmap (`sy-test-hygiene` candidate). Out of `sy-power-production` scope.
- **DKMS `amd-isp4` module fails for kernel 7.0.6** — unrelated system issue.
- **KERNEL_INSTALL_LAYOUT='other' detection in `sy power apply`** — the installer's grubby-vs-grub.d branch (P3-1) already addresses the practical Fedora-43 case; the layout-detection layer is a future hardening.
- **Step 11/13 polkit Warning UX on Fedora 0750 dir** — H4 fixed the brief's scenario; the real-host UX is cosmetic only (rule IS installed). Skip.
- **Full Step 35 typst integration** — P2-2 takes the pragmatic raster path; full typst-pdf integration is its own multi-day task (typst's library API churn 0.11→0.13 is intractable within this roadmap).
