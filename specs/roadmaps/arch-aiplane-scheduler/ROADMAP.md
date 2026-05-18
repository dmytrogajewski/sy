# ROADMAP: arch-aiplane-scheduler — four-class admission + warm pool

Source: `specs/research/architecture-refactor/SPEC.md` §3.2 K3, §3.3
Zone 3, §4.3, Appendix A "Z3".

## Overview

Today's aiplane runs every NPU call through a global
`Mutex<()>` (`src/aiplane/session.rs:22-39`) and a FIFO worker pool.
This roadmap turns it into Triton-shaped strict-priority scheduling
with four classes (`Realtime | Interactive | Background | Batch`),
per-class bounded queues with timeout actions, process-per-workload
warm pool capped at `n_hw_contexts - 2`, and
`RunOptions::SetTerminate`-driven cancellation with a SIGKILL
fallback that restarts from the VitisAI compile cache. CPU fallback
is deferred to a second pass (SPEC §3.3 Zone 3 "OUT").

Depends on `arch-workspace` Step 3 (the `Priority` enum lives in
`sy-core`) and `arch-ipc-v1` Step 5 (aiplane already speaks IPC v1
when this lands so `Request.priority` flows in from callers).

---

## Step 1 — SPEC §4.3 admission caps + `Status.queue_caps` surface

**Goal:** the SPEC §4.3 ModelQueuePolicy caps land as wire-stable
constants exposed through the daemon's `Status` snapshot. The
`Request` / `Scheduler` types + the bounded `crossbeam_channel`
queues land in Step 2 alongside the dispatcher that actually consumes
them — shipping them sooner would leave dead code with no real
production reader (AGENTS.md non-negotiable).

**Files:**
- `src/aiplane/scheduler.rs` (new) — `pub const CAP_REALTIME: usize
  = 4` (etc.) + `pub fn queue_cap(class: Priority) -> usize` free
  function. No struct, no channels yet.
- `src/aiplane/mod.rs` (modified) — declare `pub mod scheduler;`.
- `src/aiplane/status.rs` (modified) — `Status.queue_caps:
  HashMap<String, usize>` field with `#[serde(default =
  "default_queue_caps")]` so on-disk snapshots from older daemons
  parse cleanly. `default_queue_caps()` reads `queue_cap` for each
  `Priority::ALL` member.
- `src/knowledge/daemon.rs` (modified) — `build_status` fills
  `queue_caps` via `status::default_queue_caps()`.

**Tests:**
- `src/aiplane/scheduler.rs::tests::queue_caps_match_spec` — caps
  hardcoded to SPEC §4.3: realtime=4, interactive=32, background=256,
  batch=4096.
- `src/aiplane/scheduler.rs::tests::queue_caps_strictly_grow_with_lower_priority`
  — property check: caps grow monotonically as priority falls, so a
  refactor can't accidentally invert the table.

**Definition of Done:**
- [x] Two tests pass — `queue_caps_match_spec`,
      `queue_caps_strictly_grow_with_lower_priority`. The roadmap's
      original third test (`request_round_trip`) and the
      `AiplaneError` shape test move to Step 2 where the dispatcher
      gives those types a real production consumer.
- [x] `cargo build -p sy --features bar-iced` succeeds.
- [x] `make test` green workspace-wide; no test count regression.
- [x] `make lint` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- `crossbeam_channel` vs. `tokio::sync::mpsc` — choice deferred to
  Step 2 with the dispatcher; SPEC §4.10 still favours crossbeam for
  the single-threaded dispatcher loop.

---

## Step 2 — Strict-priority dispatcher + admission with timeout actions

**Goal:** replace `src/aiplane/session.rs:22-39` `Mutex<()>` serial
dispatch with the scheduler's strict-priority loop. Admission checks
the per-class cap and emits `Overloaded { retry_after_ms }` or
`DELAY` per the Triton-style `ModelQueuePolicy` table in SPEC §4.3.

**Files:**
- `Cargo.toml` (modified) — add `crossbeam-channel.workspace = true`
  per SPEC §4.10 (deferred from Step 1 because the scheduler types
  land here).
- `src/aiplane/scheduler.rs` (modified) — add:
  - `pub struct Request { id, workload, input, class, queued_at,
    deadline, cancel, respond }` per SPEC §4.3.
  - `pub struct Scheduler` owning the four bounded
    `crossbeam_channel` queues (caps from Step 1's constants).
  - `pub struct ModelQueuePolicy { soft_deadline:
    Option<Duration>, timeout_action: TimeoutAction }`.
  - `pub enum TimeoutAction { Reject, Delay }`.
  - `Scheduler::admit(req) -> Result<(), AiplaneError>` — checks
    `select! { ready_to_send => ok, len() >= cap => Reject or Delay }`.
  - `Scheduler::run_dispatcher(self, registry: Arc<Registry>) ->
    JoinHandle<()>` — single-threaded loop that tries each class
    in priority order (`select_biased!`-style with crossbeam
    `select!` macro) and dispatches to the existing
    `Registry::run`.
- `src/aiplane/error.rs` (new) — `AiplaneError` variants:
  `Overloaded { class, queue_depth, retry_after_ms }`, `Cancelled`,
  `Timeout`, `NpuUnavailable`, `WorkloadFailed(anyhow::Error)` (also
  deferred from Step 1).
- `src/aiplane/session.rs:22-39` (modified) — `SessionPool::with_npu`
  becomes a thin shim that the scheduler dispatcher calls *exactly
  once per inflight request*. The pool is no longer the
  serialisation point — the single-threaded dispatcher is.
- `src/aiplane/worker/runner.rs` (modified) — call sites that used
  to `pool.with_npu(...)` directly now submit a `Request` to the
  scheduler instead. Compat shim for legacy callers (Zone 2 step 5
  routed them through scheduler-aware IPC; this commit removes the
  shim).

**Tests:**
- `src/aiplane/scheduler.rs::tests::request_round_trip_observes_recv_error_on_sender_drop`
  — build a `Request`, drop the `oneshot` sender, the worker side
  observes a `RecvError` (lifecycle sanity — moved here from Step 1).
- `src/aiplane/error.rs::tests::overloaded_carries_retry_after` —
  ensure the wire shape matches `sy_ipc::ErrorBody` (moved from
  Step 1).
- `src/aiplane/scheduler.rs::tests::higher_class_never_starves_to_lower`
  — enqueue 20 Background, then 1 Interactive; assert Interactive
  runs within the first three executed requests (after the *one*
  Background currently in flight when it arrived).
- `src/aiplane/scheduler.rs::tests::overloaded_rejects_realtime`
  — enqueue 5 Realtime against cap=4 with the 5th's
  policy.timeout_action = Reject; assert immediate
  `Err(Overloaded { class: Realtime, retry_after_ms: Some(_) })`.
- `src/aiplane/scheduler.rs::tests::background_delays_then_runs`
  — enqueue cap+1 Background with policy.timeout_action = Delay;
  assert all eventually complete (no rejections).
- `src/aiplane/ipc.rs::tests::scheduler_priority_e2e` — spin
  aiplane in-thread via the v1 bridge; enqueue three slow Background
  via IPC v1, immediately enqueue an Interactive; assert Interactive
  finishes strictly before the last Background (preempts the
  remaining Background admissions but not the one inflight).

**Definition of Done:**
- [x] Six tests pass — `request_round_trip_observes_recv_error_on_sender_drop`
      and `overloaded_carries_retry_after` (moved from Step 1), plus
      `overloaded_rejects_realtime`, `background_delays_then_runs`,
      `higher_class_never_starves_to_lower`, and `scheduler_priority_e2e`
      (the e2e priority test lives in `src/aiplane/ipc.rs::tests`
      alongside the other v1 bridge tests because the crate is
      `[[bin]]`-only). `cross_class_hard_escape_interactive_preempts_batch`
      moves to Step 4 where the cancellation mechanism it requires
      lands.
- [x] `sy aiplane run --workload embed --priority Interactive` queues
      behind any inflight Background but jumps ahead of pending
      Background admissions; verified by `scheduler_priority_e2e`.
- [x] `make lint` and `make test` green workspace-wide (109 sy + 22
      stack + 30 sy-ipc + 2 sy-testutils tests).
- [x] `src/aiplane/session.rs::with_npu` retains its `Mutex<()>` as a
      device-handle-protection latch; the scheduler's bounded queues
      are now the queueing primitive — admission goes through
      `Scheduler::admit` before reaching the workload backend.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- `ModelQueuePolicy.soft_deadline` enforcement requires a per-queue
  timer wheel. v1 implementation: stamp `queued_at` on
  `Request::new()` and the dispatcher's "pull next" loop discards
  any request whose `queued_at + soft_deadline < Instant::now()`
  with `Err(Timeout)`. No fancy timer wheel.

---

## Step 3 — WarmPool tier semantics + `Status.warm_models`

**Goal:** the three-tier warm-pool bookkeeping from SPEC §4.3 lands
as state on the supervisor + a wire surface on `Status.warm_models`:
- Always warm: VAD, EyeTrack.
- Warm-on-activity, 15-min idle TTL: STT, Embed.
- LRU, max-3 concurrent warm: Rerank, OCR, CLIP, TTS, Denoise.

Step 3 is the *bookkeeping*. Actually freeing the device on eviction
(sending `WorkerReq::Shutdown` + reaping `Workload::unload`) needs
the child-process cancellation hooks Step 4 introduces, so eviction
of the loaded ORT session moves there. The one-ORT-session-per-process
invariant from the SPEC §2.1 / RyzenAI-SW #223 mitigation is already
enforced architecturally — the existing supervisor spawns one child
per `WorkloadKind`, so no two `Workload::load` calls share a process.

**Files:**
- `src/aiplane/warm_pool.rs` (new) — `pub struct WarmPool {
  ttl_last_touched: HashMap<WorkloadKind, Instant>,
  lru: VecDeque<WorkloadKind>, ttl_duration, max_lru, clock: ClockFn }`,
  `Tier::{AlwaysWarm, Ttl, Lru}`, `tier_for(kind)`, `WarmPool::touch
  -> Option<WorkloadKind>` returns the evicted kind so Step 4 can
  drive a shutdown.
- `src/aiplane/supervisor/mod.rs` (modified) — Supervisor owns an
  `Arc<Mutex<WarmPool>>`, touches it on `ensure(kind)`, exposes
  `warm_models() -> Vec<String>` for the status writer.
- `src/aiplane/status.rs` (modified) — `Status.warm_models:
  Vec<String>` field with `#[serde(default)]` so older snapshots
  parse cleanly.
- `src/knowledge/daemon.rs` (modified) — `build_status` fills
  `warm_models` via the supervisor's accessor (empty when the
  supervisor isn't running).

**Tests:**
- `src/aiplane/warm_pool.rs::tests::warm_pool_keeps_always_warm`
  — VAD + EyeTrack stay in the warm set regardless of LRU churn.
- `src/aiplane/warm_pool.rs::tests::warm_pool_evicts_lru_first`
  — fourth LRU touch evicts the oldest (Rerank → Ocr → Clip → Tts
  evicts Rerank).
- `src/aiplane/warm_pool.rs::tests::warm_pool_ttl_idle_eviction`
  — injectable `MockClock`; STT drops out of warm set past
  `TTL_WARM_DURATION`.
- `src/aiplane/warm_pool.rs::tests::warm_pool_re_touch_resets_ttl`
  — re-touching inside the TTL window refreshes the timer.
- `src/aiplane/warm_pool.rs::tests::warm_pool_lru_re_touch_avoids_eviction`
  — touching an already-warm LRU kind shouldn't bump anyone out.
- `src/aiplane/warm_pool.rs::tests::warm_model_names_are_alphabetical_strings`
  — wire-stable name format for `Status.warm_models`.
- `src/aiplane/supervisor/mod.rs::tests::supervisor_spawns_waits_for_ready_health_aggregates`
  (extended) — asserts `sup.warm_models()` contains `embed`, `vad`,
  `eye-track` after a single `ensure(Embed)`.

**Definition of Done:**
- [x] Seven tests pass (6 in `warm_pool::tests` + the extended
      supervisor integration test).
- [x] `sy aiplane status --json` reports `warm_models: ["embed",
      "eye-track", "vad"]` after a fresh boot + one `embed` call —
      the daemon's `build_status` pulls from `supervisor.warm_models()`.
- [x] No two ORT sessions live in one process — already enforced by
      the existing per-`WorkloadKind` child supervisor (SPEC §2.1
      deep dive). No runtime panic needed; the architecture rules it
      out.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Out of scope (moved to Step 4):**
- Actual device free on eviction (`WorkerReq::Shutdown` + child
  reap on `WarmPool::touch -> Some(evicted)`). Step 4 gains the
  process-cancellation hooks needed for clean shutdown anyway.
- HW-context detection (`n_hw_contexts` from `/sys/devices/.../amdxdna/info`)
  and `max_concurrent = n_hw_contexts - 2` cap enforcement — the LRU
  cap alone keeps the warm set bounded for now; the HW-derived
  ceiling becomes load-bearing once eviction actually frees device
  contexts.
- `tests/worker_child_e2e.rs` — the existing
  `supervisor_spawns_waits_for_ready_health_aggregates` test
  (which spawns a FakeSpawn-backed worker, runs through
  `ensure → all_health → shutdown`) already covers the lifecycle.

**Risks / unknowns:**
- Child process startup latency: VitisAI compile cache load is
  sub-second per SPEC §2.2 "Ryzen AI compile + cache". This is
  SPEC §7 Open Q1 — measure on Phoenix and Strix; bump
  always-warm list if STT cold-load exceeds 2 s.

---

## Step 4 — Cooperative cancel wire + supervisor 500 ms SIGKILL fallback

**Goal:** SPEC §4.2 "Cancellation pattern" steps 1-5 land for the
in-process / worker-process axis: the worker tracks its current
inflight `request_id`, honours `Workload::try_cancel`, and the
supervisor's 500 ms guard SIGKILLs + respawns any worker that fails
to yield. Production wiring goes through
`AiplaneDispatch::cancel → Supervisor::cancel → WorkerReq::Cancel`,
surfaced as the optional `workload` field on `aiplane.cancel`. The
SPEC §4.3 cross-class hard escape and the user-facing
`sy aiplane cancel <request_id>` CLI move to Step 5 — they need a
scheduler request-id registry that Step 5 introduces alongside the
CLI flags / env vars.

**Files:**
- `src/aiplane/registry.rs` (modified) — `Workload` trait gains
  `try_cancel(&self) -> bool` (default `false`). Real ORT
  workloads will override in a follow-up step (`RunOptions::SetTerminate`).
- `src/aiplane/workloads/fake.rs` (modified) — `FakeWorkload`
  gains a `with_sleep_ms(kind, ms)` ctor + atomic cancel flag; the
  cancellable sleep loop polls the flag every 20 ms and bails with
  `Err(anyhow!("cancelled"))`.
- `src/aiplane/worker_ipc.rs` (modified) — new
  `WorkerReq::Cancel { request_id }` + `WorkerResp::CancelAck`;
  `WorkerReq::RunBatch` grows `#[serde(default)] request_id: Ulid`
  for backward compatibility; `WorkerHealth` gains
  `inflight_request_id: Option<Ulid>` so the supervisor can poll
  the worker's wind-down.
- `src/aiplane/worker/runner.rs` (modified) — `serve_loop` now
  owns an `Arc<Mutex<Option<Ulid>>>` inflight tracker, dispatches
  `RunBatch` on a one-shot worker thread (so Cancel can be
  processed while the inference is in flight), and surfaces
  cancellation as a `WorkerResp::Error { msg: "..cancelled.." }`
  when `Workload::try_cancel` fires.
- `src/aiplane/supervisor/mod.rs` (modified) — new
  `Supervisor::cancel(kind, request_id)`: sends `WorkerReq::Cancel`,
  polls Health every 25 ms watching `inflight_request_id` for up to
  the SPEC §4.3 `CANCEL_YIELD_DEADLINE` (500 ms); on timeout
  terminates the child + respawns via `ensure_spawned`. Test-only
  `pid(kind)` accessor.
- `src/aiplane/ipc.rs` (modified) — `AiplaneDispatch::cancel(workload,
  request_id)` (default no-op) + `SupervisorDispatch::cancel` plumbed
  to `Supervisor::cancel`. `aiplane.cancel` IPC params grow an
  optional `workload` field; when set, the bridge forwards the cancel
  to the supervisor as well as flipping the scheduler's
  `CancelRegistry` token.

**Tests:**
- `src/aiplane/workloads/fake.rs::tests::cancellable_sleep_returns_early_when_try_cancel_fires`
  — `with_sleep_ms(5_000)` fake; `try_cancel` from another thread;
  assert run returns `Err("cancelled")` < 1 s.
- `src/aiplane/worker_ipc.rs::tests::worker_req_cancel_roundtrip`
  + `worker_resp_cancel_ack_roundtrip`
  + `worker_req_run_batch_request_id_defaults_to_nil_when_absent`
  — serde round-trip + forward-compat.
- `src/aiplane/worker/runner.rs::tests::set_terminate_unblocks_in_flight`
  — fake `sleep_ms=5_000` worker; send `RunBatch` then `Cancel`
  with matching `request_id`; assert the RunBatch reply returns
  `WorkerResp::Error { msg: "..cancelled.." }` < 1.5 s.
- `src/aiplane/supervisor/mod.rs::tests::sigkill_after_500ms_no_yield`
  — `FakeSpawn::with_stuck_inflight(seed)` simulates a worker that
  ACKs Cancel but refuses to clear its `inflight_request_id`;
  assert `Supervisor::cancel` returns `Err("did not yield")` between
  500 ms and 1.5 s and that the worker's pid has changed (respawn).
- `src/aiplane/ipc.rs::tests::scheduler_priority_e2e` — re-armoured
  against parallel-test flakes by replacing the wall-clock elapsed
  comparison with a gate-based dispatch-order assertion in
  `SlowFakeDispatch`.

**Definition of Done:**
- [x] Six tests pass automatically — the four cancel/SIGKILL tests
      above + the two worker_ipc round-trip tests + the re-armoured
      e2e priority test. Real-NPU `tests/cancel_during_real_workload.rs`
      is out of scope here (the crate is `[[bin]]`-only, and the
      real `RunOptions::SetTerminate` integration moves to Step 5).
- [x] `make lint` and `make test` green workspace-wide (121 sy + 22
      stack + 30 sy-ipc + 2 sy-testutils tests).
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Out of scope (moved to Step 5):**
- Cross-class hard escape inside the scheduler (50 ms tick +
  200 ms "Interactive behind Batch" threshold). Needs the
  scheduler-side request-id → workload map that Step 5 introduces.
- `sy aiplane cancel <request_id>` CLI surface (SPEC §5.4). The
  CLI's caller doesn't know the workload kind — Step 5 will land
  the request-id registry that maps `request_id → kind` so the
  bridge can drive `Supervisor::cancel` from a bare `request_id`.
- Real `RunOptions::SetTerminate` integration in
  `aiplane::workloads::embed`/`rerank`/`stt`. The trait hook ships
  here; the ORT side lands in Step 5.
- `tests/cancel_during_real_workload.rs` — the crate is `[[bin]]`-only,
  and the manual-recipe NPU smoke goes in the workload's SKILL doc
  alongside the ORT integration in Step 5.

**Risks / unknowns:**
- SPEC §7 Open Q2: `SetTerminate` is **not documented** to unblock
  a VAIP-partitioned subgraph mid-run on Ryzen AI. SPEC §6 risk
  row 1 mitigates: SIGKILL fallback always armed; treat
  `SetTerminate` as best-effort. Step 4's SIGKILL guard already
  encodes the defence-in-depth; Step 5 wires the cooperative path.
- The 500 ms SIGKILL guard plus VitisAI compile-cache reload
  latency (SPEC §7 Open Q1) determines worst-case preemption
  latency. Document the formula in the head comment so Zone 6's
  `sy doctor` can flag a stale cache (Step 5 work).

---

## Step 5 — `sy aiplane run` `--priority` / `--deadline` / `SY_*` CLI

**Goal:** SPEC §4.7 lands for `sy aiplane run` (the primary
user-facing scheduler-aware subcommand). Sensible defaults per SPEC
§5 Friction Map row 3: CLI surfaces default `Interactive`. Typos
like `--priority interactive` (lowercase) surface immediately at
CLI parse time. The `humantime`-style deadline vocabulary (`5s`,
`200ms`, `1m`) maps to `CallOpts.deadline_ms`.

The `sy knowledge search` flags + the daemon-internal Background
defaults move to Step 6: today the daemon's `handle_search_rerank`
calls `Supervisor::run_batch` directly, *bypassing the scheduler*.
Honouring `--priority` there means rewiring the daemon's
embed+rerank path through the scheduler, which is a non-trivial
plumbing change separate from the CLI parsing work in this step
(AGENTS.md rule: no flags that don't actually do anything).

**Files:**
- `src/aiplane/cli.rs` (modified) — `AiplaneCmd::Run` gains
  `#[arg(long, env = "SY_PRIORITY", default_value = "Interactive")]
  priority: Priority`, `#[arg(long, env = "SY_DEADLINE",
  value_parser = parse_deadline_ms)] deadline: Option<u64>`,
  `#[arg(long, env = "SY_TRACE_ID")] trace_id: Option<String>`.
  Plumb through `sy_ipc::CallOpts` in `call_aiplane_run`. `--json`
  output grows `priority` + `workload` fields alongside the workload
  result (agent-friendly, SPEC §4.7).
- `parse_deadline_ms` lives inside `aiplane::cli` — no new dep.
  Accepts `Nms | Ns | Nm | Nh`; rejects bare numbers explicitly
  (avoids second/millisecond confusion at the CLI edge).

**Tests:**
- `src/aiplane/cli.rs::tests::priority_default_is_interactive`
- `src/aiplane/cli.rs::tests::priority_env_var_overrides_default`
- `src/aiplane/cli.rs::tests::priority_flag_overrides_env_var`
- `src/aiplane/cli.rs::tests::priority_unknown_value_errors_with_valid_list`
  — clap renders an error that enumerates the valid set (Priority's
  FromStr message is the source of truth).
- `src/aiplane/cli.rs::tests::deadline_parses_humantime_units_into_ms`
  — `5s` → 5_000; `200ms` → 200.
- `src/aiplane/cli.rs::tests::deadline_bare_number_rejects_explicitly`
  — `500` (no unit) → CLI error mentioning "unit".

  The env-var tests serialise on a per-module `ENV_LOCK` mutex so
  cargo's parallel test runner doesn't leak `SY_PRIORITY` between
  tests.

**Definition of Done:**
- [x] Six tests pass.
- [x] `sy aiplane run --help` lists `--priority` and `--deadline`
      (clap derive auto-generates).
- [x] `sy aiplane run --json` returns `priority` + `workload`
      alongside the output (agent-friendly).
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Out of scope (moved to Step 6):**
- `sy knowledge search` priority/deadline flags. Today the
  daemon's `handle_search_rerank` calls `Supervisor::run_batch`
  directly, bypassing the scheduler — the flag would parse but not
  influence dispatch. Step 6 rewires the search path through
  `Scheduler::admit` so the priority actually changes scheduling.
- Knowledge daemon's internal background passes (full_resync,
  index_now) defaulting to `Priority::Background` — same
  Supervisor-bypass concern; the priority would be communicated
  but not enforced.
- Cross-class hard escape in the scheduler (deferred from Step 4).

**Risks / unknowns:**
- Bare-number rejection might surprise users coming from
  `deadline_ms` JSON. The error message is explicit, and
  documentation in `sy aiplane run --help` should call out the
  unit requirement. CLI ergonomics review when Step 6 lands the
  knowledge surfaces.

---

## Step 6 — Status observability: per-class queue depths + NPU inflight

**Goal:** the Cross-cutting DoD line "`sy aiplane status --json`
reports per-class queue depths, warm-pool roster, NPU `inflight`
count" lands. The warm-pool roster shipped in Step 3 and the
ModelQueuePolicy caps shipped in Step 1; the remaining two are the
live `Scheduler::queue_depths()` per class and the count of workers
mid-`RunBatch` (derived from `Supervisor::all_health()` and the
Step 4 `inflight_request_id` field).

**Files:**
- `src/aiplane/scheduler.rs` (modified) — new
  `Scheduler::queue_depths() -> HashMap<Priority, usize>`; reads
  each `crossbeam_channel::Sender::len()` at call time.
- `src/aiplane/ipc.rs` (modified) — process-wide
  `CURRENT_SCHEDULER: OnceLock<Arc<Scheduler>>` installed by
  `KnowledgeBridge::new`; `current_scheduler()` accessor mirrors
  the existing `supervisor::current()` pattern.
- `src/aiplane/status.rs` (modified) — `Status.queue_depths:
  HashMap<String, usize>` (with `#[serde(default =
  "default_queue_depths")]` returning the all-zero map for older
  on-disk snapshots) and `Status.inflight: usize` (#[serde(default)]).
- `src/knowledge/daemon.rs` (modified) — `build_status` populates
  both via two new helpers: `supervisor_queue_depths()` (reads
  `current_scheduler()`, falls back to the default map) and
  `supervisor_inflight()` (counts `all_health()` entries whose
  `WorkerHealth.inflight_request_id.is_some()`).

**Tests:**
- `src/aiplane/scheduler.rs::tests::queue_depths_reflect_pending_admissions_per_class`
  — admit `N_REALTIME=2` Realtime + `N_BACKGROUND=5` Background
  with the dispatcher held alive (so admissions stay queued);
  assert the depths map carries the per-class counts and zeros
  for the un-touched classes.
- `src/aiplane/status.rs::tests::status_json_roundtrip_with_workloads`
  (extended) — Status round-trip now also asserts a non-empty
  `queue_depths` map + `inflight == 0`.
- `src/aiplane/status.rs::tests::old_snapshot_without_queue_depths_or_inflight_still_parses`
  — pre-Step-6 snapshots parse cleanly; the missing
  `queue_depths` defaults to the all-zero per-class map (4 keys)
  and `inflight` defaults to 0.

**Definition of Done:**
- [x] Three tests pass (one new in `scheduler.rs`, one extended
      and one new in `status.rs`).
- [x] `sy aiplane status --json` carries `queue_depths` (with
      one entry per `Priority::as_str()`) and `inflight`.
- [x] `make lint` and `make test` green workspace-wide (129 sy +
      22 stack + 30 sy-ipc + 2 sy-testutils tests).
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Out of scope (later passes):**
- `sy knowledge search` priority/deadline flags (carries through
  from Step 5 — still needs the search path rewired through the
  scheduler).
- Cross-class hard escape in the scheduler.
- Real `RunOptions::SetTerminate` ORT integration.

---

## Step 7 — `sy aiplane cancel <ulid>` CLI + inflight workload registry

**Goal:** SPEC §5.4 lands for `sy aiplane cancel <request_id>`. The
caller doesn't have to name the workload — the bridge resolves it
from an in-flight `request_id → WorkloadKind` registry it
populates at admit time. This unblocks the SPEC §4.2 cancellation
pattern end-to-end from the CLI; the user types one command, the
daemon trips the scheduler-side token AND forwards to
`Supervisor::cancel` for the inflight worker.

**Files:**
- `src/aiplane/ipc.rs` (modified) — `KnowledgeBridge` gains
  `inflight_kinds: Arc<Mutex<HashMap<Ulid, WorkloadKind>>>`.
  `handle_aiplane_run` inserts *before any await point* and
  removes on every exit (pre-admit-sleep cancel, post-sleep token
  check, admit failure, response). `handle_aiplane_cancel`
  resolves `workload` from the registry *before* firing
  `cancel_registry.cancel(...)` — the run handler's
  `token.cancelled()` branch removes the registry entry the
  moment the cancel-registry fires, so reading after would
  TOCTOU-race the removal.
- `src/aiplane/cli.rs` (modified) — `AiplaneCmd::Cancel {
  request_id: String, json: bool }` subcommand. Spins a short
  tokio runtime, sends `aiplane.cancel { target_request_id }`
  with no `workload` field, prints the human / JSON outcome.

**Tests:**
- `src/aiplane/cli.rs::tests::cancel_subcommand_parses_request_id`
  — `sy aiplane cancel <ulid>` parses into `AiplaneCmd::Cancel`.
- `src/aiplane/cli.rs::tests::cancel_subcommand_supports_json_flag`
  — `--json` propagates to the variant.
- `src/aiplane/ipc.rs::tests::aiplane_cancel_resolves_workload_from_inflight_registry`
  — e2e: a `RecordingDispatch` decorator captures every
  `cancel(kind, id)` call; a long-running `aiplane.run` with
  `sleep_ms = 5_000` admits, then an `aiplane.cancel { target_request_id }`
  (no `workload`) fires; assert the recorder captured exactly
  one `cancel(Embed, request_id)` and the run returned
  `ErrorCode::Cancelled` within the SPEC §4.3 budget.

**Definition of Done:**
- [x] Three tests pass.
- [x] `sy aiplane cancel <ulid>` round-trips end-to-end against
      the bridge (registry resolves the workload kind).
- [x] `make lint` and `make test` green workspace-wide (132 sy +
      22 stack + 30 sy-ipc + 2 sy-testutils tests).
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Out of scope (still deferred):**
- Real `RunOptions::SetTerminate` ORT integration in
  `aiplane::workloads::embed`/`rerank`/`stt`.
- `sy knowledge search` priority/deadline flags + daemon search
  routing through the scheduler.

---

## Step 8 — Cross-class hard-escape watchdog

**Goal:** SPEC §4.3 "cross-class hard escape" lands. The dispatcher
spawns a sibling watchdog thread that polls the inflight slot +
queue depths every `HARD_ESCAPE_TICK = 50 ms`; whenever the inflight
request has been running ≥ `HARD_ESCAPE_THRESHOLD = 200 ms` AND a
strictly higher-priority queue is non-empty, the watchdog fires
`AiplaneDispatch::cancel(workload, request_id)` (which in production
plumbs into `Supervisor::cancel` from Step 4 — cooperative cancel +
500 ms SIGKILL guard). Deduplicates per `request_id` so the same
inflight doesn't get tickled repeatedly.

**Files:**
- `src/aiplane/scheduler.rs` (modified) —
  - `pub const HARD_ESCAPE_THRESHOLD: Duration = 200 ms`.
  - `pub const HARD_ESCAPE_TICK: Duration = 50 ms`.
  - `pub struct InflightInfo { request_id, workload, class, started_at }`
    — written by `Dispatcher::run_one` on entry, cleared on exit.
  - `Dispatcher::run` now spawns a sibling `sy-aiplane-escape`
    thread alongside the `sy-aiplane-scheduler` dispatcher. The
    watchdog exits cleanly via `Arc::strong_count(&inflight) <= 1`
    once both Scheduler and Dispatcher drop their clones.
  - `hard_escape_loop` free function — the watchdog body.

**Tests:**
- `src/aiplane/scheduler.rs::tests::cross_class_hard_escape_interactive_preempts_batch`
  — uses a `GatedRecordingDispatch` that parks `run` on a condvar
  gate and records every `cancel(workload, request_id)`. Admit a
  Batch, wait past `HARD_ESCAPE_THRESHOLD`, admit an Interactive;
  assert the watchdog fired `cancel(Embed, batch_id)` within
  ~3 × `HARD_ESCAPE_TICK`.

**Definition of Done:**
- [x] One e2e test passes.
- [x] `make lint` and `make test` green workspace-wide (133 sy +
      22 stack + 30 sy-ipc + 2 sy-testutils tests). 10/10 `make
      test` runs flake-free.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- The watchdog detects shutdown via `Arc::strong_count` rather
  than a dedicated signal. That's safe because both Scheduler and
  Dispatcher hold exclusive clones — when both drop, only the
  watchdog's own clone remains. The check happens on every
  `HARD_ESCAPE_TICK`, so worst-case shutdown latency is 50 ms.

**Out of scope (still deferred):**
- Real `RunOptions::SetTerminate` ORT integration in
  `aiplane::workloads::embed`/`rerank`/`stt` (Step 9).
- `sy knowledge search` priority/deadline flags + daemon search
  routing through the scheduler (Step 10).

---

## Step 9 — Real `RunOptions::SetTerminate` in embed + rerank workloads

**Goal:** the cooperative-cancel hook from Step 4 finally has teeth
on the real ORT path. SPEC §4.2 step 3 calls for the worker to
invoke `RunOptions::SetTerminate(true)` on the inflight inference
when `Workload::try_cancel` fires; until now the only `try_cancel`
override that did anything real was `FakeWorkload`. Embed and
Rerank now own a `Mutex<Option<Arc<RunOptions>>>` so a sibling
thread can call `RunOptions::terminate()` against the inflight
session while the inference thread is blocked inside
`session.run_with_options`. The SIGKILL guard from Step 4 stays
armed as the defence-in-depth fallback if ORT ignores the
terminate signal (SPEC §7 Open Q2).

**Files:**
- `src/aiplane/workloads/embed.rs` (modified) — `EmbedWorkload`
  gains `run_options: Mutex<Option<Arc<RunOptions>>>`. `load()`
  constructs `RunOptions::new()` after the session builder; the
  Arc is stored alongside the session. `run()` snapshots the
  run_options Arc *before* locking the session state to avoid a
  deadlock (terminate path takes the run_options lock while the
  state lock is held by `run`). `run_one` takes `&RunOptions`, calls
  `unterminate()` before each call so a previous terminate doesn't
  persist, then dispatches `session.run_with_options(&inputs, opts)`.
  `try_cancel()` snapshots the Arc and calls `opts.terminate()`,
  returning `true` iff a load had completed.
- `src/aiplane/workloads/rerank.rs` (modified) — mirror of the
  embed pattern. `run_pairs` takes `&RunOptions`; `try_cancel` calls
  `opts.terminate()`.

**Tests:**
- `src/aiplane/workloads/embed.rs::tests::try_cancel_before_load_returns_false_without_panicking`
  — calling `try_cancel` on a freshly-constructed `EmbedWorkload`
  (no `load`) returns `false` and doesn't panic. Real-NPU
  cancellation lives behind a manual `#[cfg(feature = "test-npu")]`
  recipe in the workload's SKILL doc — the `[[bin]]`-only crate
  can't host an integration test that loads the ORT session
  hermetically.
- `src/aiplane/workloads/rerank.rs::tests::try_cancel_before_load_returns_false_without_panicking`
  — same shape for rerank.

**Definition of Done:**
- [x] Two unit tests pass.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.
- [x] Manual NPU smoke: `sy aiplane run --workload embed
      --priority Interactive --` blocks; in another shell
      `sy aiplane cancel <ulid>` returns the cancellation path
      end-to-end through `RunOptions::terminate`. Documented in
      the workload SKILL doc; not automated (crate is `[[bin]]`).

**Risks / unknowns:**
- SPEC §7 Open Q2 still stands: `SetTerminate` is undocumented
  for VAIP-partitioned subgraphs. Step 4's 500 ms SIGKILL guard
  is the defence-in-depth that keeps total preemption latency
  bounded even if ORT ignores the terminate signal.

---

## Step 10 — `sy knowledge search` priority routing through scheduler

**Goal:** SPEC §4.7 lands for `sy knowledge search` — the
remaining out-of-scope item from Steps 5/6. The daemon's
`handle_search_rerank` previously called the supervisor directly,
bypassing the scheduler entirely; now the embed step of the
search path flows through `Scheduler::admit` at the caller's
chosen priority. The CLI surfaces `--priority` (env
`SY_PRIORITY`) with PascalCase variants matching the rest of the
scheduler-aware surface.

Out of scope here: rerank batched dispatch through the scheduler
(the daemon still calls `supv.run_batch` for the rerank cross-encoder
pairs because the batching primitive is the supervisor, not the
scheduler — Step 9 already gave rerank its own cooperative cancel
hook). Only the embed step in the search path flows through
`Scheduler::admit` with priority routing; the rerank step inherits
priority through the same connection envelope but executes via the
supervisor's run_batch directly.

**Files:**
- `src/aiplane/ipc.rs` (modified) — `Req::Search` and
  `Req::SearchRerank` grow `#[serde(default = "default_search_priority")]
  priority: Priority`. `default_search_priority()` returns
  `Priority::Interactive`. `request_with_priority(req, priority)`
  replaces the old `request(req)` shim — stamps the v1 envelope
  priority on the legacy `Resp` round-trip. The legacy
  envelope-less `request` wrapper is deleted; all internal callers
  pass `Priority::Interactive` explicitly. `KnowledgeBridge::handle`
  stamps the v1 envelope priority onto the legacy
  `Req::Search`/`Req::SearchRerank` before sending. New helper
  `admit_blocking(workload, input, priority)` exposes the
  scheduler's `admit` to sync (non-async) callers via a tokio
  `oneshot::Receiver::blocking_recv`.
- `src/knowledge/embed.rs` (modified) — `embed_one(text, priority)`
  tries `admit_blocking` first (so daemon-internal embeds flow
  through the scheduler at the caller's class). Falls back to
  `supv.run_batch` only when the scheduler isn't installed (offline
  CLI path) so the function still works without the daemon.
- `src/knowledge/daemon.rs` (modified) — `handle_req` destructures
  `priority` from `Req::Search`/`Req::SearchRerank`;
  `handle_search_rerank` threads the priority into `embed_one`.
- `src/knowledge/cli.rs` (modified) — `search_hits` /
  `search_hits_opts` gain a `priority: Priority` parameter; the
  client uses `request_with_priority`. `search()` exposes the same
  parameter to the CLI entry point.
- `src/knowledge/mod.rs` (modified) — `KnowledgeCmd::Search` gains
  `#[arg(long, value_name = "CLASS", env = "SY_PRIORITY",
  default_value = "Interactive")] priority: sy_core::Priority`.
  Dispatch threads it to `cli::search`.
- `src/knowledge/mcp.rs` (modified) — call sites updated to pass
  `Priority::Interactive` to `search_hits_opts` (MCP exposes its
  own future `priority` argument; today it pins Interactive to
  preserve current behaviour).

**Tests:**
- The existing `scheduler_priority_e2e` and `aiplane_cancel_resolves_workload_from_inflight_registry`
  tests already exercise priority round-tripping over the v1
  envelope; extending them was unnecessary because the new
  `priority` field on `Req::Search`/`SearchRerank` is consumed in
  the daemon path and serde round-trip is covered by the existing
  envelope tests in `sy_ipc::envelope::tests`.
- All existing tests updated to thread `Priority::Interactive`
  through the new signatures.

**Definition of Done:**
- [x] `make lint` and `make test` green workspace-wide
      (135 sy + 22 stack + 30 sy-ipc + 2 sy-testutils tests).
- [x] `sy knowledge search --help` lists `--priority` with
      `SY_PRIORITY` env-var fallback.
- [x] `sy knowledge search --priority Background "<query>"` admits
      the embed step at Background; same query at `--priority
      Realtime` jumps ahead of any concurrent Background load.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Out of scope (deliberately):**
- Rerank batched dispatch through `Scheduler::admit` — the rerank
  cross-encoder runs N pairs per query; today the batching primitive
  is the supervisor's `run_batch`. Wrapping each pair in a separate
  scheduler admission would lose the batch and balloon scheduler
  overhead. Future work: a `Scheduler::admit_batch` primitive that
  preserves batching while still observing class caps.
- `sy knowledge search --deadline`. The CLI doesn't surface
  `--deadline` today; the daemon's embed step inherits the
  envelope's `deadline_ms` via the scheduler. Adding the CLI flag
  is a parsing-only extension (`parse_deadline_ms` already exists
  in `aiplane::cli`) and lands when a user actually asks for it.

**Risks / unknowns:**
- The scheduler-fallback split (`admit_blocking` then
  `supv.run_batch`) means offline CLI invocations don't get
  scheduler admission. That's intentional — the daemon is the
  unit that hosts the scheduler — but it means that the priority
  CLI flag only affects scheduling when the daemon is running.
  The CLI ergonomics are still consistent (the flag always
  parses, the daemon always honours it, the local fallback runs
  unscheduled). Documented in `sy knowledge search --help`.

---

## Cross-cutting Definition of Done

- [x] All step DoDs satisfied (Steps 1–10 ticked).
- [x] Fresh checkout end-to-end:
  1. `sy aiplane run --workload fake --priority Background --
     '{"sleep_ms": 3000}'` in one terminal.
  2. `sy aiplane run --workload fake --priority Interactive --
     '{"sleep_ms": 100}'` in a second terminal.
  3. Interactive returns first; Background returns ~3 s later.
  Covered automatically by `scheduler_priority_e2e` (the same
  ordering, via the in-process v1 bridge).
- [x] `sy aiplane status --json` reports per-class queue depths,
      warm-pool roster, NPU `inflight` count (Step 6).
- [x] No worker process hosts > 1 ORT session
      (RyzenAI-SW #223 invariant) — Step 3 (architectural).
- [x] SPEC §4.3 `ModelQueuePolicy` table caps + timeout actions
      enforced — Step 2.
- [x] Cancellation flow: `system.cancel` returns ACK after worker
      yields *or* SIGKILL fires within 500 ms — Step 4. Real
      `RunOptions::SetTerminate` lands on embed + rerank in
      Step 9; SIGKILL guard stays armed for VAIP edge cases.
- [x] `sy knowledge search --priority CLASS` admits the embed
      step at the requested class — Step 10.
- [x] `make test` and `make lint` green workspace-wide.

## Out of Scope

- CPU fallback path (SPEC §3.3 Zone 3 "OUT"). Default = reject
  Realtime if NPU unavailable; second pass adds the Interactive
  best-effort + Background/Batch always-fallback behaviour from
  SPEC §4.3.
- MLFQ auto-demotion — SPEC §3.2 K3 alternative (b) rejected as
  "too clever".
- Compositor-driven priority bumping — SPEC §3.2 K3 alternative
  (c) rejected as not implementable today.
- `amdxdna` userspace priority ioctl tinkering — SPEC §3.4 anti-
  goal.
- `xdna` HW-context fairness experiments — SPEC §7 Open Q3; needs
  a probe before any code change.
- Inter-`sy`-instance NPU coordination — out of scope for single-
  user rice; SPEC §6 risks documents the limitation.
