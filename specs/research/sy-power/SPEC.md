# SPEC: `sy power` — ML-driven power orchestration for Ryzen AI HX 370

> **Revision 2 (2026-05-14).** Rewritten end-to-end from Revision 1
> after user feedback that the previous spec hedged: rules v1, ML v2.
> This revision makes ML the v1 policy. Rules become the **conservative
> baseline the bandit is guaranteed not to underperform** and the
> fallback-on-drift, not the primary actor.

## 1. Summary

`sy power` is an ML-driven, intent-aware power orchestrator on
AMD Ryzen AI HX 370. A tiny **GRU forecaster** (~2-5k params,
`tract`-on-CPU, sub-millisecond) predicts the workload class
arriving in the next 30-120 s. A **Conservative Linear UCB
contextual bandit** picks among ~8 pre-validated power profiles
using (forecast + 12-channel intent signal panel + thermal +
battery) as context. A **post-posed 5-state DFA shield**
enforces hard physical limits before any sysfs write. The
training loop is offline (burn → ONNX → tract export); only the
bandit's reward update is online. Apple-style 14-day onboarding
collects baseline telemetry under rules-only control; after that,
the ML policy engages and the bandit is provably never worse than
the rules by more than its conservative-margin α.

## 2. Background & Research

### Market Context

| Daemon | Model | Heterogeneous-aware? | Activity-aware? | Predictive? |
|---|---|---|---|---|
| [TLP](https://linrunner.de/tlp/) | One-shot, udev-triggered | iGPU only | No | No |
| [auto-cpufreq](https://github.com/AdnanHodzic/auto-cpufreq) | Polling daemon, 5 s loop | No | Load-only | No |
| [power-profiles-daemon](https://gitlab.freedesktop.org/upower/power-profiles-daemon) | D-Bus, 3 static profiles | No | No | No |
| [system76-power](https://github.com/pop-os/system76-power) | D-Bus, profile + dGPU mux | dGPU only | No | No |
| [tuned](https://tuned-project.org/) | Daemon + Python plugins | No | No | No |
| [ananicy-cpp](https://gitlab.com/ananicy-cpp/ananicy-cpp) | Per-process classifier | No | Per-process names | No |
| ChromeOS [`resourced`](https://chromium.googlesource.com/chromiumos/platform2/+/HEAD/resourced) | Daemon + D-Bus thermal events | No (no NPU on ChromeOS) | **Yes** — WebRTC / fullscreen / gaming | No |
| [Pixel Adaptive Battery](https://store.google.com/us/magazine/pixel-battery-saver-features) | On-device ML, app-launch prediction | N/A | Yes | **Yes** — bucketed |
| [Apple Optimized Battery Charging](https://support.apple.com/en-us/108055) | On-device ML, 14-day onboarding | N/A | Yes (charge habits) | **Yes** |
| [Microsoft Resource Central](https://www.microsoft.com/en-us/research/wp-content/uploads/2017/10/Resource-Central-SOSP17.pdf) | RF/XGBoost, bucketed utilization | N/A (datacenter) | Yes | **Yes** |

The white space remains: **no Linux laptop daemon is both
heterogeneous-aware (CPU × iGPU × NPU as one shared budget) and
predictive (forecasts the next 30-120 s instead of reacting after
load appears).** Pixel/Apple/Resource Central prove the ML-power
shape is deployable — all three converge on bucketed prediction +
offline training + on-device inference + 14-day onboarding. We
port that shape to Linux laptops.

### Technical Context — writable knobs (unchanged from Revision 1)

Safe writable knobs on HX 370 / kernel 6.19+:

1. `/sys/firmware/acpi/platform_profile` — host enumerates
   `quiet balanced performance`. Only sanctioned PPT dial.
2. `…cpufreq/energy_performance_preference` —
   `performance | balance_performance | default | balance_power | power`.
   Blocked when `amd_dynamic_epp=enable`; we require `disable`.
3. `…drm/card*/device/{power_dpm_force_performance_level, pp_power_profile_mode}` —
   iGPU presets.
4. `xrt-smi configure --pmode {default|powersaver|balanced|performance|turbo}` —
   only NPU performance lever.
5. **cgroup v2** under sy's own `systemd --user` slice —
   `cpu.weight`, `cpu.uclamp.{min,max}`, `io.weight`. No privilege.
6. Per-task `sched_setattr(SCHED_FLAG_UTIL_CLAMP_*)` —
   unprivileged, propagates to amd-pstate.

Hands-off list: `ryzenadj`/`ryzen_smu` ([RyzenAdj #309](https://github.com/FlyGoat/RyzenAdj/issues/309),
silently no-ops on OEM-locked Strix and can wedge SMU); NPU
per-tile freq/voltage; fan PWM; `pp_od_clk_voltage`.

### Deep Dives — ML choice rationale

**Why a GRU and not a Hoeffding tree or full RL?**

- **GRU vs Hoeffding tree**: Resource Central (SOSP'17) used Random
  Forests for bucketed VM-utilization prediction because the workloads
  were independent and the feature set wide. Our problem has
  *temporal structure* (PSI trigger fires → builds usually run for
  30-180 s; call inhibitor grabs → call usually lasts 5-60 min). A
  GRU encodes that. The smallest GRU that beats EWMA-on-PMU-counters
  on workload-onset is ~2k params (single layer, hidden=16,
  input=16), trains in seconds, infers in 50-200 µs via
  [tract](https://github.com/sonos/tract) (Sonos ships LSTMs of this
  scale at 70 µs in production). Hoeffding trees in Rust are also a
  non-starter — no production-grade implementation exists in 2026.
- **GRU vs full RL**: every honest paper concedes laptop-scale RL is
  paperware. GearDVFS (MobiCom 2023), FiDRL (TC 2024), DRLCAP —
  benchmarks, no production. The actuator space here is discrete
  (5-10 profiles), so RL is the wrong abstraction. **Contextual
  bandits dominate** (Microsoft Personalizer, Netflix homepage, VW
  `--cb_explore`). The Agrawal-Goyal Thompson sampling regret bound
  ([arXiv 1209.3352](https://arxiv.org/abs/1209.3352)) is tight, the
  math is closed-form, and the Conservative LinUCB variant
  ([Kazerouni NeurIPS 2017](https://arxiv.org/abs/1611.06426))
  guarantees we never underperform a known baseline by more than α
  — provably making rules our floor, not our hope.

**Why offline training, not online RL?**

- On-device exploration on a $2000 laptop is unacceptable: a bad
  action thermal-cycles the package or thrashes a meeting. CQL/IQL
  ([Kumar 2020](https://arxiv.org/abs/2006.04779),
  [Kostrikov 2021](https://arxiv.org/abs/2110.06169)) make
  *training* safe by never exploring online. We do the same: log
  trajectories during rules-only onboarding, batch-train the GRU
  offline (in-process, on the user's idle+plugged window, via
  `burn` with the `ndarray` backend), export to ONNX, hot-swap
  inference via `arc_swap::ArcSwap`. Only the bandit's reward
  update is online — and CLUCB's regret bound covers that.

**Why a 5-state shield, not CMDP / shielded RL training?**

- Alshiekh et al ([AAAI 2018](https://arxiv.org/abs/1708.08611))
  formalized shields as DFAs over MDP abstractions. The **idea** —
  deterministic action filter that vetoes unsafe outputs — is
  trivial to ship in Rust as ~150 LoC. The full *training-time*
  shielding apparatus is overkill for our blast radius. Post-posed
  shielding (filter after the policy decides) is correct for us:
  simpler, requires no policy-side coupling, lets us swap models.

**Why GRU on CPU via tract, not NPU?**

- VitisAI EP cold-compile is multi-second the first time. The
  session-creation and IPC overhead per inference dominate for a
  sub-10k-param model whose CPU inference is ≤200 µs. The NPU is
  the wrong tool for tiny models. Keep NPU for the existing aiplane
  workloads (`embed`, `ocr`, `stt`, `rerank`, `vad`) where models
  are ≥1 M params and dispatch is amortized.

**Why the 12-signal panel (vs PMU counters alone)?**

- Every laptop ML-power deployment uses application-level signals,
  not just PMU. Pixel uses app-launch history; Apple uses location +
  charge habits; Microsoft Resource Central uses tenant ID, hour of
  day, and seasonality. The biggest predictive jump on Linux comes
  from signals nobody else taps:
  - [PSI cgroup-v2 triggers](https://docs.kernel.org/accounting/psi.html)
    fire at build *leading edges* (0-2 s ahead) — not after load
    appears, before.
  - [`systemd-logind` inhibitor list](https://systemd.io/INHIBITOR_LOCKS/)
    is the de-facto single source of truth for "user is in a call"
    on Linux today (Teams/Slack/Discord all `Inhibit("idle")`;
    Zoom adds `com.zoom.HotKeyService`).
  - niri's [`ext-foreign-toplevel-list-v1`](https://wayland.app/protocols/ext-foreign-toplevel-list-v1)
    event-stream gives us focused-app transitions with sub-ms
    latency over its IPC socket.
  - sy already owns the aiplane registry — knowing the NPU queue
    depth in-process is a free perfect signal.
  - Time-of-day + day-of-week cyclical features matter (Pixel data).

**Why 14-day onboarding?**

- Apple's Optimized Battery Charging requires "≥14 days of data and
  ≥9 charges of ≥5 hours" before engaging
  ([Apple support](https://support.apple.com/en-us/108055)). This is
  the most-documented consumer ML-power threshold in industry. We
  mirror it: rules-only for 14 days; bandit + GRU engage after that.

## 3. Proposal

### Approach — two-tier ML with hard safety floor

```
┌──────────────────────────────────────────────────────────────────────┐
│                       sy-powerd (tokio daemon)                       │
│                                                                      │
│   1 Hz sensor tick                                                   │
│   ┌──────────────────────────────────────────────┐                   │
│   │ sensors/* (sysfs, hwmon, PSI, RAPL, NPU)     │                   │
│   │ intent::* (niri, logind, MPRIS, aiplane,     │                   │
│   │            cgroups, TOD/DOW, idle, AC)       │                   │
│   └────────────────────┬─────────────────────────┘                   │
│                        │                                             │
│                        ▼                                             │
│           Snapshot { 12-channel feature vec }                        │
│                        │                                             │
│              ┌─────────┴─────────┐                                   │
│              ▼                   ▼                                   │
│     forecast::predict       activity::classify  ◀── linfa-ftrl       │
│     (GRU via tract,         (online L1 logistic,    auxiliary        │
│      bucketed class,         partial_fit at 1 Hz)                    │
│      30-120 s horizon)                                               │
│              │                   │                                   │
│              └─────────┬─────────┘                                   │
│                        ▼                                             │
│           bandit::propose_ranked                                     │
│           (Conservative LinUCB, 8 arms = profiles)                   │
│                        │                                             │
│                        ▼                                             │
│           ranked_actions: [(profile_id, ucb_score), …]               │
│                        │                                             │
│                        ▼                                             │
│           shield::project (5-state DFA)                              │
│           ┌──── HOT? BATT<10%? MEETING? ───┐                         │
│           │ walk ranked list, return       │                         │
│           │ first action that passes;      │                         │
│           │ else: rules-baseline action    │                         │
│           └────────────────┬───────────────┘                         │
│                            ▼                                         │
│           apply::diff_and_write                                      │
│           (idempotent; skip if sysfs already matches)                │
│                            │                                         │
│       ┌────────────┬───────┼──────────┬───────────┐                  │
│       ▼            ▼       ▼          ▼           ▼                  │
│   polkit:        sysfs:  xrt-smi:   cgroup:    bandit::update_reward │
│   platform_      EPP +  --pmode    cpu.weight  (online; reward =     │
│   profile        iGPU                          perf/W − thermal_pen) │
│                            │                                         │
│                            ▼                                         │
│           log::append (NDJSON, train-ready)                          │
│           + drift::observe(forecast_residual, reward_residual)       │
│                            │                                         │
│                            ▼                                         │
│           waybar IPC + D-Bus PPD signal + systemd sd_notify          │
│                                                                      │
│   Offline (idle+plugged window, weekly or ADWIN-triggered):          │
│     trainer::retrain_gru → burn/autodiff → ONNX → ArcSwap            │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

### Key Decisions

| Decision | Choice | Reasoning | Alternatives |
|---|---|---|---|
| Policy class | **Conservative Linear UCB contextual bandit + GRU forecaster** | Bandit dominates RL when actuator space is small + pre-safed; CLUCB provably ≥ baseline − α. GRU encodes temporal workload structure that EWMA cannot. | Full RL (paperware at laptop scale); pure rule table (no predictive ability); deep RL governor (FiDRL itself concedes invocation cost). |
| Inference path | **`tract` on CPU** for the GRU; NPU **not** used for the policy model | Sub-ms tract inference, no cold-start, no dispatch overhead. NPU EP cold-compile + IPC dominates for sub-10k-param models. | NPU via VitisAI (cold-start cost ≫ gain); pure ONNX Runtime via existing `ort` (works, but tract is smaller and policy daemon shouldn't depend on knowledge plane's `ort` lifecycle). |
| Training | **Offline, in-process, via `burn` with `ndarray` + `autodiff` backends** | Pure Rust, deterministic, no GPU dependency, exports ONNX cleanly. Trains tiny GRU in seconds during idle+plugged windows. | tch-rs (250-500 MB libtorch tax); Python service (snowflake). |
| Onboarding | **14-day rules-only window before bandit engages** | Mirrors Apple Optimized Charging — the most-documented consumer ML-power threshold. Bandit needs hundreds of decisions across diverse contexts. | Engage immediately (cold-start exploration on user's real laptop is unacceptable); ship a vendor-pretrained model (no — every laptop has different workloads). |
| Safety mechanism | **Post-posed 5-state DFA shield + rule-baseline fallback + systemd WatchdogSec** | ~150 LoC hand-coded shield; bandit proposes ranked, shield walks the list. Watchdog reverts to vendor defaults on hang. Three layers, all simple. | Pre-emptive shielding (couples policy to safety automaton); CMDP training (constraint-in-expectation only); CBF projection (overkill for discrete actions). |
| Drift response | **Drop to rules-only until next scheduled retrain** | Database industry standard practice (Databricks, EvidentlyAI runbooks). Never silently degrade. | Auto-retrain online (risks regression); ignore drift (silent failure). |
| Rules' role | **Conservative baseline the bandit cannot underperform by > α; safe action set the shield uses; bootstrap policy during onboarding and drift** | Mathematically rigorous: CLUCB's regret bound is *relative to the baseline*. Rules and ML are co-equal in v1. | Rules as fallback only (loses the regret guarantee); ML alone (no baseline; nothing to anchor exploration). |
| PPD replacement | **Implement `net.hadess.PowerProfiles` D-Bus name; replace PPD via systemd alias** | Fedora 43 GNOME hard-binds to it; `tuned-ppd` precedent. | Sidecar (two daemons fight); leave PPD alone (DE drives it independently — race conditions). |

### ML (Minimum Loveable)

**IN (v1 shipping scope):**

- `sy-powerd` (tokio, systemd `--user`).
- 12-channel intent signal panel (PSI triggers, logind inhibitors,
  niri toplevel stream, aiplane registry tap, TOD/DOW, cgroup
  ancestry of new procs, MPRIS, xdg-portal ScreenCast,
  ext-idle-notify-v1, AC/battery + drain rate, throttling-status).
- 14-day onboarding under rules-only control. Collects
  `~/.local/state/sy/power/telemetry.ndjson` (rotated daily, 7 day
  retention).
- Tiny GRU forecaster (16 input × 16 hidden × buckets output,
  ~2k-5k params). Trained from collected telemetry via `burn` on
  the user's idle+plugged window. Inference via `tract` at 1 Hz.
- Conservative Linear UCB contextual bandit over 8 power-profile
  arms (combinations of `platform_profile × EPP × igpu_mode ×
  npu_pmode × cgroup hints`). Online reward updates; closed-form
  posterior.
- Online auxiliary classifier (`linfa-ftrl`) labeling the *current*
  activity in 5 classes — fed to the GRU as a feature.
- Post-posed 5-state DFA shield (`COOL_AC | WARM_AC | HOT |
  BATTERY_LOW | MEETING`) with the concrete HX 370 constraint set
  (table below).
- Self-supervised labels: `sy power profile <name>` override =
  positive label; throttling event / fan-complaint notification =
  coarse negative; battery-drain residual vs TOD prediction =
  signed label.
- ADWIN on forecast residual + bandit reward residual; on alarm,
  daemon drops to rules-only and schedules an offline retrain.
- systemd `WatchdogSec=10` + revert-to-vendor-default on miss.
- `sy power {status, profile, explain, daemon, log, apply, train, mcp}` CLI.
- `net.hadess.PowerProfiles` D-Bus shim for GNOME compat.
- Waybar tile.
- MCP `power_status` tool for agents to self-throttle.
- Audit log: every action logged with `(timestamp,
  model_version_sha, snapshot_hash, ranked_actions, shield_decision,
  applied_action, reason)`.

**OUT (anti-goals, deliberate):**

- No online exploration of the bandit on day 1 — onboarding is
  rules-only, the bandit warms up afterward.
- No online retraining of the GRU on the hot path. Offline only,
  during idle+plugged windows.
- No kernel-hot-path DRL governor. Period.
- No ryzenadj / ryzen_smu writes — anti-goal forever.
- No fan curve, no per-core voltage, no CO/undervolt.
- No NPU-resident policy model — wrong tool for sub-10k-param model.
- No remote/cloud telemetry. Everything stays local.
- No replacement for `ananicy-cpp`. Per-process nicing remains a
  separate concern.
- No raw window titles in the telemetry (privacy: app_id only).
- No keystroke contents, no notification bodies (only a derived
  coarse boolean "user complained about fan").

### Anti-Goals (expanded)

1. **No SMU mailbox writes.** Bricks the platform_profile path that
   is sy's primary lever on OEM-locked SKUs.
2. **No exploration during user-critical activity.** `MEETING`
   shield state freezes profile thrashing during VAD-active windows
   regardless of bandit recommendations.
3. **No model that can't be reproduced.** Every shipped GRU
   checkpoint must be retrainable from the user's logged telemetry
   alone — no factory-pretrained weights, no cloud distillation.
4. **No black-box decisions.** `sy power explain` must always
   render: snapshot inputs, top-3 bandit-ranked actions with UCB
   scores, shield state, why the chosen action was selected.

## 4. Technical Design

### Architecture

```
src/power/
├── mod.rs              # public surface + tracing setup
├── cli.rs              # `sy power {status, profile, explain, daemon, log, apply, train, mcp}`
├── daemon.rs           # sy-powerd: tokio main loop, IPC server, sd_notify
├── ipc.rs              # Unix-socket protocol
├── sensors/            # hardware reads (no side effects)
│   ├── pstate.rs       # amd-pstate / cpufreq
│   ├── platform.rs     # platform_profile + choices
│   ├── hwmon.rs        # k10temp, amdgpu hwmon
│   ├── rapl.rs         # powercap / amd_energy
│   ├── igpu.rs         # gpu_busy_percent, pp_*
│   ├── npu.rs          # accel0; DRM ioctl on kernel ≥ 7.1
│   └── battery.rs      # /sys/class/power_supply/
├── intent/             # application-level signals
│   ├── psi.rs          # cgroup-v2 pressure poll() triggers
│   ├── logind.rs       # systemd-logind inhibitor watcher (zbus)
│   ├── niri.rs         # niri IPC subscriber
│   ├── aiplane.rs      # in-process registry tap
│   ├── mpris.rs        # MPRIS PlaybackStatus subscriber
│   ├── portal.rs       # xdg-portal ScreenCast Session counter
│   ├── idle.rs         # ext-idle-notify-v1
│   ├── cgroup.rs       # proc ancestry + comm whitelist
│   ├── notify.rs       # fan-complaint coarse-bool sniffer
│   └── time.rs         # TOD + DOW cyclical encoding
├── snapshot.rs         # assembles the 12-channel feature vec
├── forecast/
│   ├── gru.rs          # GRU inference via tract
│   ├── model.rs        # model schema, hot-reload via ArcSwap
│   └── fixtures/       # tiny pretrained "warmup" model (rules-equivalent)
├── activity.rs         # linfa-ftrl online classifier (5 classes)
├── bandit/
│   ├── clucb.rs        # Conservative Linear UCB
│   ├── arms.rs         # 8 power-profile arms enumerated
│   └── reward.rs       # perf/W - thermal_penalty - thrash_penalty
├── shield/
│   ├── dfa.rs          # 5-state DFA + transitions
│   └── project.rs      # walk ranked actions, return first that passes
├── apply/              # actuators (only writers in the codebase)
│   ├── platform.rs     # platform_profile via polkit
│   ├── epp.rs          # energy_performance_preference write
│   ├── igpu.rs         # pp_power_profile_mode
│   ├── npu.rs          # xrt-smi configure --pmode
│   └── cgroup.rs       # systemd --user scope cpu.weight, uclamp
├── drift.rs            # ADWIN + DDM, in-house ~200 LoC
├── trainer.rs          # offline GRU retrain via burn; export ONNX
├── log.rs              # NDJSON writer + rotator + audit
├── ppd_shim.rs         # D-Bus net.hadess.PowerProfiles
└── mcp.rs              # MCP `power_status` tool

configs/sy/
├── power.toml          # arm definitions, shield thresholds, rules baseline
└── intent_whitelist.toml  # comm names per activity class

configs/systemd/user/
└── sy-powerd.service   # WatchdogSec=10 + Restart=on-failure

configs/polkit/
└── 10-sy-power.rules

configs/waybar/
└── modules/sy-power.json
```

**Cargo deps added** (marginal cost ~5-8 MB beyond existing `ort`):

```toml
tract-onnx = "0.22"        # inference, pure-Rust ONNX, sub-ms
burn = { version = "0.20", default-features = false, features = ["ndarray", "autodiff", "train"] }
burn-ndarray = "0.20"
burn-autodiff = "0.20"
trashpanda = "0.x"         # contextual bandits (Thompson + LinUCB + ConservativeLinUCB)
linfa-ftrl = "0.8"         # online L1/L2 logistic regression
adskalman = "0.18"         # state-space filtering
augurs = "0.10"            # forecasting / outlier detection (auxiliary)
safetensors = "0.4"        # GRU weight serialization
arc-swap = "1"             # hot-reload
zbus = "5"                 # D-Bus PPD shim + logind / portal / MPRIS
procfs = "0.18"            # /proc/PID/cgroup, /proc/PID/comm
rhai = "1.20"              # (optional, post-v1) user trigger overrides
```

### Non-Functional Requirements

- **Performance**:
  - Daemon RSS &lt; 50 MB (GRU weights, bandit posterior, ndarray
    buffers).
  - Sensor tick &lt; 5 ms p99.
  - GRU inference &lt; 1 ms p99 (tract on Zen5 CPU).
  - Bandit `propose_ranked` &lt; 100 µs p99 (closed-form linear
    algebra, 8 arms).
  - Shield projection &lt; 50 µs p99.
  - Total per-tick work &lt; 7 ms p99 → daemon overhead ≪ 0.05 W.
  - Offline retrain: bounded at 60 s wall on Zen5 CPU using
    `burn-ndarray`; runs only when AC + idle ≥ 5 min + battery
    SOC &gt; 50%.
- **Reliability**:
  - Writes are diffed against current sysfs (avoid the auto-cpufreq
    #381 anti-pattern of constant rewrites).
  - On daemon crash: systemd `Restart=on-failure`, exit-handler
    writes vendor-default `platform_profile=balanced` and
    EPP=`balance_performance` before exit.
  - `WatchdogSec=10` + `sd_notify(WATCHDOG=1)` every 5 s; on
    miss, systemd kills + restarts.
  - Drift alarm: daemon drops to rules-only and emits a `degraded`
    event over IPC + waybar.
- **Security**:
  - Cgroup of own user slice → no privilege.
  - Polkit-mediated `platform_profile` write under
    `org.sy.PowerProfile.SetProfile`.
  - udev rule chowns EPP files to `wheel` group (productised in
    `configs/`).
  - No `/dev/mem`, no setuid, no `CAP_SYS_RAWIO`.
  - SELinux file_context declared.
- **Observability**:
  - `tracing` spans per tick: snapshot, forecast, classify, propose,
    shield, apply, reward.
  - `tracing-subscriber` JSON layer to stderr (`--log-format json`).
  - NDJSON audit log: every action with
    `(model_version_sha, snapshot_hash, ranked_actions,
    shield_state, applied_action, reason_chain)`.
  - `sy power explain` reads the last N audit entries and renders a
    human story.
  - Waybar tile shows current profile + shield state + (during
    onboarding) `learning: 13d 4h to first ML decision`.
- **Privacy**:
  - **No raw window titles, no keystrokes, no notification bodies,
    no clipboard.**
  - Niri `focused-window` → strip `title`, keep `app_id` only.
  - Notification sniffer extracts a coarse `user_complained_about_fan: bool`
    and discards body text immediately.
  - Telemetry never leaves the host; no opt-in cloud upload in v1.

### Concrete Shield Constraint Set (HX 370)

| Constraint | Hard limit | Rationale |
|---|---|---|
| Tctl peak | 90 °C (act at 85) | AMD Tjmax ≈ 95 °C |
| Tctl sustained 60 s avg | 80 °C | Prevent thermal cycling |
| Package power excursion rate | ≤ +15 W in 2 s | Avoid VRM spike / coil whine |
| Fan RPM (when readable) | ≤ 5500 RPM | Framework HX 370 fan reference max |
| Battery cap on DC, SOC < 25% | force `balanced` or lower | Conserve runtime |
| Battery cap on DC, SOC < 10% | force `quiet` | Emergency |
| Profile-thrash | ≤ 1 change / 30 s | Avoid perf hysteresis pain |
| Profile changes during VAD-active | banned for 30 s | Don't kill voice codec |
| NPU pmode transitions | ≤ 1 / 5 s | XDNA state-change cost |
| EPP delta | ≤ 64 per tick | Smooth transitions |

### Bandit Arms (8 pre-validated profiles)

Each arm is a tuple `(platform_profile, epp, igpu_mode, npu_pmode,
cgroup_overrides)`. Names are illustrative; the table is shipped as
`configs/sy/power.toml`:

| Arm | platform_profile | EPP | iGPU | NPU | cgroup hint |
|---|---|---|---|---|---|
| `whisper` | quiet | power | POWER_SAVING | powersaver | uclamp_max=40 |
| `idle` | quiet | balance_power | POWER_SAVING | powersaver | default |
| `browse` | balanced | balance_power | BOOTUP_DEFAULT | powersaver | default |
| `call` | balanced | balance_performance | BOOTUP_DEFAULT | balanced | uclamp_min=20 |
| `code` | balanced | balance_performance | BOOTUP_DEFAULT | balanced | uclamp_min=30 |
| `build` | performance | balance_performance | POWER_SAVING | powersaver | uclamp_min=60 |
| `npu-burst` | performance | balance_power | POWER_SAVING | turbo | NPU prio |
| `flat-out` | performance | performance | 3D_FULL_SCREEN | turbo | uclamp_min=80 |

The bandit's job: given context, pick the arm with highest CLUCB
score (UCB upper bound on perf/W minus thermal/thrash penalty), subject
to the conservative-margin α relative to the rules baseline.

### CLI / MCP Surface (unchanged from R1 + train/explain expansion)

```text
sy power status [--json]              # current state, profile, shield, reason
sy power explain [--json] [--last=N]  # which arm fired and why (audit replay)
sy power profile <name>               # manual override (cleared by --auto)
sy power profile --auto               # restore bandit control
sy power list-profiles [--json]       # enumerate from configs/sy/power.toml
sy power log [--since=1h] [--json]    # tail telemetry NDJSON
sy power daemon                       # systemd entrypoint
sy power apply [--dry-run]            # install polkit/udev/systemd units
sy power train [--in <ndjson>] [--out <onnx>]  # offline GRU retrain
sy power mcp                          # MCP server (stdio)
```

**`--json` schema (`sy power status --json`):**

```json
{
  "schema": "sy.power.status/v1",
  "ts": "2026-05-14T10:00:00Z",
  "onboarding": {"active": false, "days_collected": 21, "ready_at": "2026-05-07T00:00:00Z"},
  "model": {"version_sha": "ab12cd34", "loaded_at": "2026-05-14T09:00:00Z", "params": 2384},
  "shield_state": "WARM_AC",
  "activity_label": "build",
  "forecast": {
    "horizon_s": 60,
    "next_activity": {"build": 0.78, "code": 0.18, "idle": 0.03, "call": 0.01}
  },
  "bandit": {
    "chosen_arm": "build",
    "ucb_score": 1.34,
    "top3": [["build", 1.34], ["code", 1.21], ["browse", 0.88]],
    "conservative_alpha": 0.05,
    "baseline_arm": "code"
  },
  "applied_policy": {
    "platform_profile": "performance",
    "epp": "balance_performance",
    "igpu_mode": "POWER_SAVING",
    "npu_pmode": "powersaver",
    "cgroup": {"cpu_uclamp_min": 60}
  },
  "sensors": {
    "package_power_w_5tap": 27.4,
    "tctl_c": 71.0,
    "igpu_busy_pct": 4,
    "npu_workloads": 0,
    "battery_pct": 100,
    "ac": true
  },
  "drift": {"adwin_alarm": false, "ddm_warning": false}
}
```

**Exit codes** (unchanged): 0 ok / 1 err / 2 usage / 3 drift /
4 daemon unreachable / 5 polkit denied / 6 unsupported hardware /
7 onboarding-not-complete (when `--require-ml` flag passed).

### Testing Strategy

- **Unit** (pure logic, no host coupling):
  - `sensors::*::parse` over `src/power/fixtures/sys/`.
  - `intent::psi::trigger` against a synthetic `cgroup.pressure`
    fifo.
  - `shield::project` table-tested over the full
    activity × thermal × SOC product.
  - `bandit::clucb` regret-bound check on a synthetic 10k-step
    trace (assert empirical regret ≤ theoretical with high prob).
  - `forecast::gru::infer` deterministic over a fixed ONNX +
    feature window.
  - `drift::adwin` against the classical Bifet test set.
- **Integration** (daemon-in-thread):
  - Spawn `sy-powerd` against a temp `XDG_RUNTIME_DIR` + stubbed
    sensors (`SY_POWER_SENSOR_FAKE=<scripted-timeline>`).
  - Drive a scripted day: idle → browse → call → build → idle.
    Assert (a) onboarding gates the bandit for the configured
    window, (b) shield catches every constraint violation we
    inject, (c) audit log replays cleanly.
  - PPD shim: `gdbus call --system --dest net.hadess.PowerProfiles
    SetActiveProfile` round-trips.
- **Bench**:
  - Per-tick wall time on Zen5 (must be &lt; 7 ms p99).
  - Tract inference latency for the shipped GRU.
- **Manual E2E recipe** at `specs/research/sy-power/RUNBOOK.md`:
  1. `sy power apply` → installs polkit + systemd unit + waybar tile.
  2. `systemctl --user start sy-powerd.service`.
  3. `sy power status` → expect `onboarding.active=true`,
     `model.version_sha="rules-baseline"`.
  4. (after 14 days of normal use) `sy power train` → produces a
     personal ONNX, hot-swapped in.
  5. `cargo build` → expect `activity_label="build"` within 1 s of
     PSI trigger; bandit picks `build` arm; shield permits.
  6. Force overheat with `stress-ng` → assert shield steps to `HOT`
     within 1 s and downgrades profile.

### Migration & Compatibility (unchanged from R1)

- New on-disk state: telemetry NDJSON + GRU checkpoints under
  `~/.local/state/sy/power/`. Schema-versioned per line; 7-day
  rotation; 50 MB/day hard cap; refuse to write if free space &lt; 1 GiB.
- `sy power apply` detects an installed `power-profiles-daemon` and
  either disables it (with `--yes`) or runs sy alongside while
  disabling sy's D-Bus shim (`--with-ppd`).
- Sensor read failures degrade gracefully — daemon never blocks on
  missing hardware.

### Dependencies (full list)

Already in tree: `tokio`, `serde`, `serde_json`, `toml`,
`anyhow`, `clap`, `chrono`, `uuid`, `tracing`, `walkdir`, `reqwest`,
`ort`, `tokenizers`, `ndarray` (via ort).

New (marginal cost ~5-8 MB):

- `tract-onnx` 0.22 — GRU inference. Pure-Rust, sub-ms,
  production-proven (Sonos).
- `burn` 0.20 + `burn-ndarray` + `burn-autodiff` — offline training.
- `trashpanda` — typed contextual bandits.
- `linfa-ftrl` — online logistic regression (auxiliary classifier).
- `adskalman` — state-space filtering of noisy sensor streams.
- `augurs` — auxiliary seasonality / outlier detection.
- `safetensors` — model weight serialization.
- `arc-swap` — hot-reload.
- `zbus` ≥ 5 — D-Bus.
- `procfs` — /proc parsing.
- (post-v1) `rhai` for user-supplied trigger overrides.

System dependencies (productised in `configs/`, no snowflakes):
`xrt-smi`, polkit ≥ 122 (Fedora 43 default), kernel ≥ 6.14
(amdxdna); kernel ≥ 7.1 unlocks NPU mW telemetry (we degrade to
RAPL-delta proxy until then).

## 5. User Journey Sketch

**Actor:** Rice user on HX 370. Daily flow: cargo builds, aiplane
NPU workloads, video calls, idle browsing, occasional fullscreen
video.

**Trigger:** They install/update sy and run `sy apply`.

**Phases:**

1. **Bring-up.** `sy apply` installs polkit + systemd unit +
   waybar tile + disables PPD (confirmation prompt; `--with-ppd`
   to keep both).
2. **Onboarding (days 0-14).** sy-powerd runs the rules-only
   baseline; collects telemetry. Waybar tile shows
   `onboarding 13d 4h`. Status JSON marks `onboarding.active=true`,
   `model.version_sha="rules-baseline"`. Behaviour is identical to
   GNOME PPD + a thermal-aware rule table.
3. **First training.** On day 14, the next idle+plugged window
   (battery ≥ 50%, idle ≥ 5 min) triggers
   `trainer::retrain_gru`. The GRU trains in under a minute on
   the user's actual workflow. Hot-swap: bandit engages.
4. **Adaptive operation.** PSI fires → forecast says
   "build in 5-30 s" → bandit picks `build` arm → shield permits
   (Tctl=71 °C, AC, no VAD) → `platform_profile=performance` writes.
   Call inhibitor grabs → forecast says "call ≥ 5 min" → bandit
   picks `call` → shield enters `MEETING` state and freezes
   thrash for 30 s.
5. **Curiosity.** `sy power explain` answers "why are my fans loud":
   shows snapshot, top-3 ranked arms with UCB, shield state, applied
   action, reason chain. Output is the same shape the audit log
   stores.
6. **Override.** User pins `sy power profile quiet` before a meeting;
   `--auto` returns to bandit.
7. **Drift.** Two months later, the user's daily pattern changes
   (working from a noisy café). ADWIN fires on the forecast
   residual. Daemon drops to rules-only, surfaces a notification:
   "sy-powerd is retraining: drift detected." Next idle+plugged
   window: retrain. Hot-swap. Resume ML control.

**North star:** "I never touch power settings. sy quietly *predicts*
what I'm about to do, picks the right profile *before* the fans
notice, never breaks my meetings, learns from my actual workflow
without phoning home, and when I ask why it tells me exactly which
arm it chose and on what evidence."

### Friction Map

| Friction | Phase | Opportunity |
|---|---|---|
| 14-day onboarding feels long ("when does the ML kick in?") | 2 | Waybar tile + status JSON show the countdown + which rule fired this tick; user feels the daemon is alive. |
| First training spike on day 14 | 3 | Schedule strictly during idle+plugged+night windows; never train mid-meeting. |
| Misclassified activity (e.g. headless cargo job → no build label) | 4 | `sy power explain` exposes the snapshot; user can edit `configs/sy/intent_whitelist.toml` to add their comm name. |
| Bandit explores an unfamiliar arm and a meeting suffers | 4 | `MEETING` shield state outright freezes thrash for 30 s after VAD; CLUCB's conservative margin keeps exploration bounded to ≤ α regret. |
| Drift drops to rules-only, user notices fans louder | 7 | Notification explicitly says "retraining: drift detected"; status JSON exposes `drift.adwin_alarm=true`; recovery is auto on next train window. |
| Privacy concern: "are you watching what I do?" | all | Privacy section in `sy power --help`; `sy power log` shows the exact features stored (app_id only, no titles, no keystrokes, coarse bools only). |

## 6. Risks & Mitigation

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| `amd_dynamic_epp=enable` blocks our EPP writes | Primary lever no-ops | Medium | `sy power apply` adds `amd_dynamic_epp=disable` to grub drop-in. |
| Replacing PPD breaks GNOME quick-settings | UI regression | Low if shim is wire-compatible | Wire-level integration test against `gdbus introspect` in CI; `--with-ppd` opt-out. |
| Bandit exploration during a critical activity | Meeting/call quality dip | Low | `MEETING` shield state freezes thrash; CLUCB conservative-margin α bounds total exploration cost. |
| GRU mispredicts → bandit picks wrong arm → shield catches it | Suboptimal but safe | Medium | Shield is the load-bearing safety mechanism; designed to absorb arbitrarily bad ML output. |
| Sensor blackout on kernel bump (amdgpu CWSR bug on 6.18/6.19 cf. [Framework forum](https://community.frame.work/t/attn-critical-bugs-in-amdgpu-driver-included-with-kernel-6-18-x-6-19-x/79221)) | Forecast goes stale | Medium | Best-effort sensor reads; ADWIN catches the residual jump; daemon degrades to rules. |
| Telemetry NDJSON fills disk | Self-DoS | Low | Daily rotation, 7-day retention, 50 MB/day cap, free-space gate. |
| Cold-start: 14 days is "too long" for users | UX | Medium | Onboarding window is configurable (`SY_POWER_ONBOARDING_DAYS`, min 3); document the Apple-mirror rationale. |
| burn 0.20 API churn between minor versions | Build breakage | Medium | Pin exact minor; abstract the trainer behind a `trait Trainer` so swapping crates is a one-day refactor. |
| `tract` vs `ort` ONNX-op coverage gap | Training works, inference fails on tract | Low | The GRU op-set is tiny and well-covered by tract; CI gate that loads the freshly-trained ONNX in tract before promoting. |
| sched_ext scheduler conflict (scx_lavd memory leak #3340) | Indirect | Known on 6.19 | Don't load `sched_ext` schedulers. utilclamp + cgroup-v2 only. |
| ryzenadj temptation creep | Bricks SMU | Avoided by design | Anti-goal #1; documented in `AGENTS.md`. |
| Bandit's "conservative baseline" is itself wrong (rules don't beat manual settings) | We're optimizing against a bad floor | Medium | The baseline is a *rule table*, not a fixed value; users edit `configs/sy/power.toml`. Audit log shows when bandit chose differently from baseline. |

## 7. Open Questions

1. **Onboarding window length** — 14 days mirrors Apple, but a
   coding-heavy user might collect enough variety in 5 days. Default
   14, expose `SY_POWER_ONBOARDING_DAYS`, document trade-off.
2. **GRU vs. tinier model** — could a Mealy-machine-style state +
   linear classifier match the GRU at half the params? Benchmark
   against the onboarding telemetry before locking the GRU
   architecture; treat the GRU as the upper bound, swap if a smaller
   model matches it on the same residual.
3. **Should `sy agt run` enqueue an activity hint?** Probably yes —
   cheap signal that beats heuristic cmdline matching. Defer to
   `/journey` expansion; if added, the bandit picks `code` arm
   immediately on agt session start.
4. **Self-supervised label thresholds** — what battery-drain residual
   constitutes a "wrong profile" signal? Calibrate against the first
   30 days of beta telemetry.
5. **Reward shaping** — perf/W − thermal_penalty − thrash_penalty is
   the canonical reward, but the three weights matter. Ship a default
   and expose them as `[reward]` block in `power.toml`.
6. **Conservative-margin α** — Kazerouni's CLUCB has α controlling
   the regret guarantee. Smaller α → less exploration → safer +
   slower to converge. Default 0.05; expose as `[bandit] alpha`.

## 8. Hand-off

- **Journey:** `/journey` against this spec → expand Section 5
  into `specs/journeys/JOURNEY-<dt>.md`. Cover concrete user voice,
  error states, and observable end states for each of the seven
  phases.
- **Roadmap:** `/roadmap` against the journey →
  `specs/roadmaps/sy-power-<dt>.md`. Suggested cut points:
  - **R1** — sensor + intent panels only; NDJSON telemetry; no
    actuation. (Onboarding rehearsal.)
  - **R2** — shield + `apply::*` modules + rules-baseline actuator.
    First end-to-end actuation, ML-free.
  - **R3** — bandit (`trashpanda`) wired to the rules baseline.
    Still rules-equivalent, but the bandit is the path.
  - **R4** — GRU forecaster integrated; trainer subcommand;
    14-day onboarding gate.
  - **R5** — `linfa-ftrl` online classifier; full 12-channel intent
    panel.
  - **R6** — drift detection + retrain trigger + waybar tile.
  - **R7** — PPD D-Bus shim + MCP `power_status`.
  - **R8** — Rhai user-trigger overrides (post-v1).
- **Implement:** `/implement` one roadmap step at a time.
- **No new Workload, no new NPU model.** Feature does not invoke
  `/workload` or `/npu-prep`. (Notably: the policy GRU runs on CPU
  via `tract`, *not* on the NPU.)
