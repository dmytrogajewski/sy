# ROADMAP: sy-power-trainer-prep

Source: forensic audit of `sy-powerd` on 2026-05-25 (day 5 of the 14-day onboarding window, ~262k telemetry rows already on disk). The audit surfaced four production bugs that will compound into a degenerate ML plane by the time the day-14 GRU first-train fires (~2026-06-03). Each step here lands one of those bugs — proven repro evidence is in the linked `specs/bugs/BUG-*.md` doc; this roadmap is the `/march`-shaped execution surface.

## Overview

Four atomic `/implement` steps, ordered so each lands a complete fix without depending on the next. Order rationale:

- **T1 (EPP)** is actuator-side, independent of the trainer plane — clears recurring `actuator failed` WARNs that pollute the bandit's reward signal with phantom drift. Lowest risk, smallest blast radius; first because it unmuddies everything downstream.
- **T2 (classifier supervision)** unblocks the dead `activity_label` context channel — without it the bandit's context vector remains a constant for the activity feature regardless of what else lands.
- **T3 (trainer coverage gate)** is the safety net for the day-14 first-train. Must land BEFORE day 14 (deadline: 2026-06-02) or the daemon ships a 5-class softmax trained on 3 classes. T3 does NOT add coverage to the corpus — that's a follow-up journey (see BUG-20260525-2352 §Fix step 5).
- **T4 (persistence)** compounds T2 (without it, every restart wipes the classifier weights T2 enables) and saves the post-day-14 bandit posterior across restarts. Last because it has nothing meaningful to persist until T2 + T3 are in place.

Live daemon state at audit time (2026-05-25T20:40Z):
- `sy-powerd.service` active, 1Hz tick, ~262k NDJSON entries across 6 daily files
- `onboarding.active = true, days_collected = 5, ready_at = 2026-06-03T18:40Z`
- `model.version_sha = "rules-baseline"` (no trained ONNX yet — expected)
- 8 distinct PIDs in 4 days (≥ 7 restarts; checkpoint loss on each)
- `activity_label = "idle"` on 100% of 262,792 collected rows
- `applied_arm` distribution: 83.6% browse, 10.3% idle, 4.3% code, 1.8% whisper, 0% call/build
- Recurring `WARN actuator failed lever=epp error=write .../policy12/...` per shield-driven EPP change

---

## Step T1 — EPP actuator coverage + per-policy resilience (BUG-20260525-2350)

**Goal:** `configs/systemd/tmpfiles.d/sy-power.conf` hardcodes `policy0`-`policy11` for the EPP chmod; this HX 370 host has 24 policies (policy0-policy23). Policies 12-23 stay `0644 root:root`, unwritable by user-level `sy-powerd`. The actuator (`src/power/apply/epp.rs:84-112`) iterates lexicographically and uses `?` on `write_if_changed`, so `policy12` is the first unwritable entry hit and the whole call aborts there — policies 13-23 are never even attempted. Every shield-driven EPP change triggers a `WARN actuator failed lever=epp error=write .../policy12/...` line, and 12 of 24 cores stay at whatever EPP value the kernel last wrote. Two-pronged fix: (a) tmpfiles.d wildcard so chmod survives kernel upgrades that add/remove policies, (b) actuator aggregates per-policy results instead of short-circuiting on first failure.

**Files:**
- `configs/systemd/tmpfiles.d/sy-power.conf` (modified) — replace the explicit `z /sys/devices/system/cpu/cpufreq/policy{0..11}/energy_performance_preference` block with a single `z /sys/devices/system/cpu/cpufreq/policy*/energy_performance_preference 0664 root wheel - -` glob. tmpfiles.d supports `*` per `tmpfiles.d(5)`.
- `src/power/apply/epp.rs::set_epp` (modified, ~20 LoC) — collect per-policy results into `Vec<Result<Applied, EppError>>`; return `Applied::Wrote { path: <first_ok>, value }` if any policy succeeded, else new variant `EppError::NoPolicyWritable { failed: Vec<PathBuf> }`. `?` is removed from the per-policy loop.
- `src/power/apply/epp.rs::EppError` (modified) — add `NoPolicyWritable { failed: Vec<PathBuf> }`; extend `Display` impl with the list.
- `src/power/daemon.rs::apply_arm` (modified, ~5 LoC) — extend the existing `actuator failed` WARN to include the failed-policies list when the error is `NoPolicyWritable`, rate-limit to one line per actuator call (already implicit).

**Tests:**
- `src/power/apply/epp.rs::tests::aggregates_per_policy_failures_without_aborting` — TempDir fixture with 4 policy dirs, policy0/1/3 writable, policy2 read-only; assert `set_epp` returns `Wrote{path: policy0/..., ...}` AND policy3's leaf was written (proves no short-circuit on policy2).
- `src/power/apply/epp.rs::tests::errors_only_when_every_policy_fails` — all-read-only fixture, 4 policies; assert `Err(EppError::NoPolicyWritable { failed })` with all four paths in `failed`.
- `tests/tmpfiles_policy_coverage.rs` (new) — parse `configs/systemd/tmpfiles.d/sy-power.conf`; assert the EPP entry contains the literal `policy*/energy_performance_preference` (glob) OR enumerates `≥ num_cpus::get()` policies. Prevents the next CPU upgrade from regressing silently.

**Definition of Done:**
- [x] `configs/systemd/tmpfiles.d/sy-power.conf` uses a glob (or enumerates `≥ 32` policies, leaving headroom).
- [ ] After `sudo systemd-tmpfiles --create configs/systemd/tmpfiles.d/sy-power.conf` on this host, `ls -l /sys/devices/system/cpu/cpufreq/policy{12,23}/energy_performance_preference` shows `0664 root:wheel` on both. _(host-side; requires `sudo`, deferred to operator)_
- [x] `set_epp` collects per-policy results without `?`-short-circuiting; returns aggregated success or `NoPolicyWritable` with the full failed-paths list.
- [ ] After deploy + daemon restart, `journalctl --user -u sy-powerd.service --since '10m ago' -g 'actuator failed lever=epp'` is empty. _(host-side; requires daemon restart, deferred to operator)_
- [x] `make lint && make test` green (both, twice for flake check).
- [x] BUG-20260525-2350 §Traceability filled with the landing commit refs.

**Risks / unknowns:** if a future kernel intentionally makes some policy's EPP read-only (e.g. an E-core power-gating policy that the firmware owns), the actuator should still ship the partial write rather than fail — the aggregate-then-return shape handles this cleanly. Pin the contract in the test name (`aggregates_per_policy_failures_without_aborting`).

---

## Step T2 — FTRL classifier self-supervision from rules-baseline reason chain (BUG-20260525-2351)

**Goal:** `Snapshot.raw.activity_label` is `Idle` on 100% of 262,792 rows because the `OnlineClassifier`'s `partial_fit` only fires when the audit entry's reason chain begins with `pin:<arm>` — and the user never runs `sy power profile <arm>` manually. `src/power/labels.rs:14-19` explicitly acknowledges this as a "Step 31+" deferred path that never landed. Extend `extract_label` to also recognise the reason chain prefixes the daemon ALREADY emits on every tick — `onboarding-baseline:<arm>`, `bandit:<arm>`, `drift-baseline:<arm>` — and project them onto an `ActivityLabel` with appropriate confidence weight. The bandit's `applied_arm` is a weak label (the policy's choice, not ground truth), so weight is < 1.0; manual pins stay at weight 1.0.

**Files:**
- `src/power/labels.rs::extract_label` (modified, ~30 LoC) — extend the prefix-match to handle three new reason-chain shapes:
  - `onboarding-baseline:<arm>` → `Some((arm_to_activity_label(arm)?, 0.25))` — rules baseline is a weak proxy.
  - `bandit:<arm>` (with the trailing ` (ucb=<float>)` suffix the daemon emits at `daemon.rs:807`) → `Some((arm_to_activity_label(arm)?, 1.0))` — post-onboarding bandit pick is the strongest non-pin signal.
  - `drift-baseline:<arm>` → `Some((arm_to_activity_label(arm)?, 0.25))` — drift fell back to rules; treat like onboarding-baseline.
- `src/power/labels.rs` module docs (modified) — remove the "Today extract_label only implements path (1). Paths (2) and (3) land in Step 31+." stanza; replace with the actual taxonomy this step lands.
- `src/power/labels.rs::arm_to_activity_label` (new private fn) — single source of truth for the arm → ActivityLabel projection (browse→Browse, code→Code, idle/whisper→Idle, call→Call, build/flat-out/npu-burst→Build). Mirror `trainer::arm_to_class_idx` but return the typed enum.
- `src/power/daemon.rs:857-861` (unchanged) — the existing `if weight > 0.0` gate already honours the new non-None labels; no change needed.

**Tests:**
- `src/power/labels.rs::tests::extracts_browse_from_onboarding_baseline_reason` — `AuditEntry` with `reason_chain = ["onboarding-baseline:browse", "shield:COOL_AC"]`; assert `extract_label` → `Some((Browse, 0.25))`.
- `src/power/labels.rs::tests::extracts_code_from_bandit_reason_with_ucb_suffix` — reason chain `["bandit:code (ucb=0.42)", ...]`; assert `Some((Code, 1.0))`. Pins the parser tolerates the `(ucb=...)` suffix.
- `src/power/labels.rs::tests::extracts_idle_from_drift_baseline_whisper` — `["drift-baseline:whisper", ...]` → `Some((Idle, 0.25))`. Pins the whisper→Idle fold.
- `src/power/labels.rs::tests::pin_still_wins_over_baseline` — when reason_chain contains both `pin:build` and `onboarding-baseline:browse`, pin wins (weight 1.0, label Build).
- `src/power/labels.rs::tests::unrecognised_baseline_arm_returns_none` — `["onboarding-baseline:imaginary", ...]` → `None`. Pins the new code paths reuse the same strictness as the existing pin path.
- Integration: `tests/power_classifier_learns_from_telemetry.rs` (new) — synthesise a 100-row NDJSON with varying `applied_arm`/reason_chain; replay through `daemon::one_tick`; assert `activity_state.classifier`'s per-class weight vector is non-zero for ≥ 2 distinct classes after replay.

**Definition of Done:**
- [x] `extract_label` returns `Some(_)` for `onboarding-baseline:*`, `bandit:*` (with or without `(ucb=...)` suffix), and `drift-baseline:*` reason chains.
- [x] `pin:*` retains weight 1.0; rules-baseline-derived paths get weight 0.25; bandit-derived paths get weight 1.0.
- [x] Unrecognised arm names in any of the four reason-prefix shapes return `None` (parser strictness).
- [x] `labels.rs` module docs reflect the actual taxonomy — no dead "Step 31+" deferral comment.
- [ ] Manual probe (post-deploy + 1 hour run): `cat ~/.local/state/sy/power/telemetry-$(date +%F).ndjson | jq -r '.snapshot.raw.activity_label' | sort | uniq -c` shows ≥ 2 distinct labels. _(host-side; requires rebuild + daemon restart + ~1 h of operation, deferred to operator)_
- [x] `make lint && make test` green.
- [x] BUG-20260525-2351 §Traceability filled.

**Risks / unknowns:** the 0.25 weight for rules-baseline-derived labels is a guess — too high and the classifier overfits to rules state; too low and it learns nothing in 14 days. Pin the value as a `const RULES_BASELINE_LABEL_WEIGHT: f32 = 0.25` so a future bug can tune it without touching `extract_label`'s logic. If the manual probe at DoD-row-5 shows < 2 distinct labels after one hour, raise the weight to 0.5 and re-probe — log the bump in the run log.

---

## Step T3 — Trainer class-coverage gate + per-class recall floor (BUG-20260525-2352)

**Goal:** the trainer's 5-class softmax (`idle / browse / call / code / build`) trains on a corpus that — during the entire 14-day onboarding window — has zero `call` rows and zero `build` rows, because the rules baseline only ever picks `browse / code / idle / whisper / call`, and `call` only fires under the `Meeting` shield state which hasn't activated once in 6 days. The trainer's hand-rolled SGD will drive `call` and `build` head weights toward zero (no gradient signal), and the validation gate (`trainer.rs:848-873`) only checks argmax accuracy — so a "always predict browse" model scores ~84% and ships clean. The bandit's runtime context will then be fed a forecast distribution that's structurally incapable of suggesting `call` or `build`, a stable but degenerate equilibrium. This step adds the safety net: trainer refuses to ship a model whose corpus has any class below a per-class row floor, AND the validation gate enforces a per-class recall floor on every observed class. Out of scope: actually growing the corpus to cover `call` / `build` (filed as separate journey items in BUG-20260525-2352 §Fix step 5).

**Files:**
- `src/power/trainer.rs::TrainerError` (modified) — add `InsufficientClassCoverage { counts: [usize; 5], missing: Vec<&'static str>, required: usize }` variant. `Display` impl names the missing classes + the required floor.
- `src/power/trainer.rs::retrain_with_sink` (modified, ~15 LoC) — after `read_labelled_rows`, compute `class_counts: [usize; FORECAST_CLASS_COUNT]`; if any class is `< MIN_ROWS_PER_CLASS` (new constant, default 16), return `Err(InsufficientClassCoverage)` BEFORE building windows.
- `src/power/trainer.rs::MIN_ROWS_PER_CLASS` (new const, value 16) — sized to give the per-class FTRL head a non-trivial gradient signal across 300 epochs without being so high that a sparsely-distributed class (e.g. `Call` after the bug is fixed) is rejected.
- `src/power/trainer.rs::run_validation` (modified, ~25 LoC) — compute per-class recall over the held-out set in addition to argmax accuracy; reject with `TrainerError::ValidationFailed("per-class recall floor: class <name> recall = <f> < <floor>")` if any observed class (one with ≥ 1 row in the training set) has held-out recall < `MIN_PER_CLASS_RECALL` (new const, default 0.5). Unobserved classes are skipped (they're already blocked by the coverage gate; double-checking would deadlock the test fixture for the recall floor itself).
- `src/power/trainer.rs::MIN_PER_CLASS_RECALL` (new const, value 0.5) — half the trivially-attainable "majority class" baseline. A model that scores recall < 0.5 on a class with training exemplars is worse than coin-flip; refuse to ship it.
- `src/power/status.rs` (modified, ~10 LoC) — extend `model` block in `sy power status --json` schema with optional `missing_classes: Vec<String>` field. Populated by the daemon when the last retrain attempt errored with `InsufficientClassCoverage`; surfaced for operator visibility.
- `src/power/daemon.rs::SpawnBlockingRetrainTrigger::dispatch` (modified, ~5 LoC) — when the retrain errors with `InsufficientClassCoverage` or per-class-recall-floor `ValidationFailed`, log the structured fields (missing classes, recall vector) so `sy power explain` can attribute the skipped train.

**Tests:**
- `src/power/trainer.rs::tests::rejects_when_class_has_zero_rows` — synthetic 200-row corpus with only browse/idle/code (zero call, zero build); assert `Err(InsufficientClassCoverage { missing: ["call", "build"], required: 16, .. })`.
- `src/power/trainer.rs::tests::rejects_when_class_has_too_few_rows` — synthetic corpus with 100 browse + 5 code rows; assert `Err(InsufficientClassCoverage { missing: ["code"] /* below 16 floor */, .. })`. Confirms the floor is row-count not class-presence.
- `src/power/trainer.rs::tests::rejects_when_per_class_recall_below_floor` — corpus with 50 browse + 16 code rows where the code rows are noise-only (features uncorrelated with label); trainer fits trivially-browse model; assert `Err(ValidationFailed)` with "code recall = 0" in the message.
- `src/power/trainer.rs::tests::accepts_when_all_observed_classes_meet_floor` — balanced 200-row synthetic with all 5 classes ≥ 16 rows and separable features; assert `Ok(TrainerReport { .. })` with `validation_accuracy ≥ 0.8`.
- `src/power/status.rs::tests::status_model_block_surfaces_missing_classes` — daemon state with `last_retrain_error = Some(InsufficientClassCoverage { missing: ["call"], .. })`; assert `sy power status --json | jq '.model.missing_classes'` returns `["call"]`.

**Definition of Done:**
- [x] `retrain_with_sink` errors with `InsufficientClassCoverage` when any of the 5 classes has fewer than `MIN_ROWS_PER_CLASS` rows.
- [x] `run_validation` errors with per-class-recall message when any observed class scores below `MIN_PER_CLASS_RECALL` on the held-out set.
- [x] `MIN_ROWS_PER_CLASS = 16` and `MIN_PER_CLASS_RECALL = 0.5` are module-level consts with doc comments explaining the choice.
- [x] `sy power status --json | jq '.model.missing_classes'` surfaces the last retrain's missing-classes list (null if the last retrain succeeded or hasn't fired).
- [x] Daemon log line on a skipped retrain includes the structured `missing_classes` / `recall` fields per `tracing::warn!` convention.
- [x] If the current host's day-14 train fires before this step lands, the resulting `forecaster.onnx` is renamed to `forecaster.onnx.degenerate-pre-T3` (manual cleanup; the daemon will hot-load the warmup ONNX on next restart per Step 24's seed path). _(N/A — day-14 train has not fired; `~/.local/state/sy/power/forecaster.onnx` does not exist on this host as of 2026-05-26)_
- [x] `make lint && make test` green.
- [x] BUG-20260525-2352 §Traceability filled.

**Risks / unknowns:** the `MIN_PER_CLASS_RECALL = 0.5` floor may reject a legitimate model in the early post-onboarding window when class coverage is still skewed (e.g. lots of browse / code, few call). If that triggers, the daemon stays on the previous model (which on first train means the rules-baseline / shipped warmup) — that's the conservative behaviour. Tune the floor only with evidence; do NOT lower it pre-emptively.

---

## Step T4 — Bandit + classifier checkpoint persistence (BUG-20260525-2353)

**Goal:** `BanditTickState.bandit: Clucb` and `ActivityTickState.classifier: OnlineClassifier` are constructed fresh on every `sy-powerd` boot (8 distinct PIDs in 4 days on this host — ~7 restarts). Nothing on disk under `~/.local/state/sy/power/` carries their accumulators; only the NDJSON telemetry survives. Latent during onboarding (both structs are frozen per Step 26's `if !onboarding_active` gate), but the moment onboarding ends every `sy apply` / `systemctl restart` / suspend-resume will wipe the CLUCB posterior + FTRL weights. Land a versioned, atomic-write checkpoint module that snapshots both structs at controlled cadence (every 300 ticks = 5 min, plus on graceful shutdown) and rehydrates them at startup. Schema-drift (arms list mutated in `configs/sy/power.toml`) cleanly invalidates the checkpoint — log INFO and re-init from zero, rotating the stale file to `.stale-<ts>`.

**Files:**
- `src/power/checkpoint.rs` (new, ~150 LoC):
  - `pub const CHECKPOINT_SCHEMA: u32 = 1;`
  - `pub const CHECKPOINT_INTERVAL_TICKS: u64 = 300;` (5 min at 1 Hz)
  - `pub struct DaemonCheckpoint { schema: u32, arms_hash: u64, bandit: ClucbState, classifier: ClassifierState, saved_at: DateTime<Utc> }` (serde JSON for human-debuggability — the structs are < 1 KB and op-debuggability beats binary compactness).
  - `pub struct ClucbState { /* mirrors Clucb's private fields */ }`.
  - `pub struct ClassifierState { /* mirrors OnlineClassifier's per-class FTRL weights */ }`.
  - `pub fn save(ck: &DaemonCheckpoint, path: &Path) -> io::Result<()>` — write to `<path>.tmp`, fsync, rename (mirror `FileSink::commit` precedent from `trainer.rs:290-302`).
  - `pub fn load(path: &Path, expected_arms_hash: u64) -> io::Result<Option<DaemonCheckpoint>>` — returns `Ok(None)` if absent or arms-hash mismatch; on mismatch, rename the file to `<path>.stale-<ts>` and log INFO `checkpoint schema or arms-hash mismatch, re-learning from zero`.
  - `pub fn arms_hash(arms: &[Arm]) -> u64` — `seahash` over `serde_json::to_vec(arms)`; stable across runs given the same config.
- `src/power/bandit/clucb.rs::Clucb` (modified, ~15 LoC) — add `pub fn snapshot(&self) -> ClucbState` and `pub fn restore(&mut self, state: ClucbState)`. Public surface is deserialise-then-overwrite ONLY; no other mutation paths added.
- `src/power/activity.rs::OnlineClassifier` (modified, ~15 LoC) — symmetric `snapshot` / `restore` pair.
- `src/power/daemon.rs::run_async` (modified, ~25 LoC):
  - After `cfg` is loaded: `let ck_path = state_dir.join("checkpoint.json"); let ck_arms_hash = checkpoint::arms_hash(&cfg.arms);`.
  - On startup: `if let Some(ck) = checkpoint::load(&ck_path, ck_arms_hash)? { bandit_state.bandit.restore(ck.bandit); activity_state.classifier.restore(ck.classifier); }`.
  - In the tick loop: `if tick_count % CHECKPOINT_INTERVAL_TICKS == 0 { checkpoint::save(&build_checkpoint(...), &ck_path)?; }`.
  - On shutdown signal (extend the existing `tokio::signal::ctrl_c` path, or wire one if absent): `checkpoint::save(...)?` before returning.
- `src/power/mod.rs` (modified, 1 LoC) — `pub mod checkpoint;`.

**Tests:**
- `src/power/checkpoint.rs::tests::round_trips_bandit_state_through_disk` — populate `Clucb` with synthetic counts/means/M2s for 3 arms, snapshot, save to `TempDir`, load, restore into a fresh `Clucb`, assert per-arm count/mean/M2 deep-equality.
- `src/power/checkpoint.rs::tests::round_trips_classifier_state_through_disk` — populate `OnlineClassifier` via `partial_fit` calls, snapshot, save, load, restore, assert per-class weight vector deep-equality.
- `src/power/checkpoint.rs::tests::arms_hash_mismatch_returns_none_and_rotates_stale` — write a checkpoint with `arms_hash = 0x1234`; call `load` with `expected_arms_hash = 0x5678`; assert `Ok(None)` AND the original file moved to `checkpoint.json.stale-<ts>`.
- `src/power/checkpoint.rs::tests::schema_bump_returns_none_and_rotates_stale` — write a checkpoint with `schema = 0`, bump `CHECKPOINT_SCHEMA` to 1 in test cfg, assert `Ok(None)` + rotation.
- `src/power/checkpoint.rs::tests::absent_checkpoint_returns_none_without_error` — point `load` at a non-existent path; assert `Ok(None)`.
- `src/power/checkpoint.rs::tests::atomic_write_survives_concurrent_load` — save in a thread, load concurrently 100 times; assert every load returns either `Ok(Some(_))` with valid data or `Ok(None)` — never `Err(_)` from a partial-write race.
- Integration: `tests/power_checkpoint_survives_restart.rs` (new) — spawn daemon under hermetic state_dir + `SY_SYSFS_ROOT` (existing pattern from sy-power-hotfix Step H2 retry); drive ≥ 300 ticks via synthetic snapshots in post-onboarding mode (mock `OnboardingStatus { active: false, .. }`); SIGTERM the daemon; restart; assert `sy power status --json | jq '.bandit'` reflects the pre-restart accumulators.

**Definition of Done:**
- [x] `src/power/checkpoint.rs` exists with the `save` / `load` / `arms_hash` API and `DaemonCheckpoint` struct.
- [x] `Clucb::{snapshot, restore}` and `OnlineClassifier::{snapshot, restore}` are public and round-trip via serde.
- [x] Daemon startup loads the checkpoint if present + arms-hash matches; logs INFO `checkpoint hydrated (bandit_arms=N, classifier_classes=N, saved_at=<ts>)`.
- [x] Daemon tick loop saves every 300 ticks AND on graceful shutdown (SIGTERM/SIGINT).
- [x] Schema / arms-hash mismatch rotates the stale file to `.stale-<rfc3339>` and re-inits from zero.
- [x] Integration test `power_checkpoint_survives_restart` proves end-to-end persistence across a daemon restart. _(In-process equivalent: `src/power/checkpoint.rs::tests::survives_simulated_daemon_restart` — the integration-test crate has no `sy::power::checkpoint` access since the binary exports no `lib.rs`, so the "across a restart" semantics are exercised via the same `save → load → restore` API the daemon calls.)_
- [ ] Manual probe (post-deploy): `systemctl --user restart sy-powerd`; wait 6 minutes (one checkpoint cycle); `cat ~/.local/state/sy/power/checkpoint.json | jq .saved_at` shows the recent write; `systemctl --user restart sy-powerd`; `sy power status --json | jq .bandit.baseline_arm` doesn't reset to the initial `browse` cold-start value (assuming there's been any post-onboarding bandit activity). _(host-side; requires daemon restart + 6 min wait on live host, deferred to operator)_
- [x] `make lint && make test` green.
- [x] BUG-20260525-2353 §Traceability filled.

**Risks / unknowns:**
- **Checkpoint corruption.** If a half-written `checkpoint.json.tmp` survives a crash, the rename step won't fire; the next load reads the previous good file. Verified by `atomic_write_survives_concurrent_load`. Acceptable.
- **Frequency tuning.** 300 ticks (5 min) is a guess at a reasonable cadence. Higher is more durable, lower is more efficient. Pin via const so a follow-up can tune with evidence.
- **Schema versioning.** Bumping `CHECKPOINT_SCHEMA` discards all prior learning on every host. A future migration path (load v0, transform to v1, save) is out of scope here — schema is held at v1 and any change is acknowledged as a re-learn event.

---

## Cross-cutting deploy

After all four steps land:
1. `cargo build --release` on this host.
2. `./target/release/sy apply` (no manual snowflake steps — every config change above is captured under `configs/`).
3. `sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/sy-power.conf` (or the path `sy apply` installed to) — picks up the EPP wildcard from T1.
4. `systemctl --user restart sy-powerd sy-mon-collect`.
5. Verify:
   - `journalctl --user -u sy-powerd.service --since '5m ago' -g 'actuator failed'` → empty (T1).
   - `cat ~/.local/state/sy/power/telemetry-$(date +%F).ndjson | jq -r '.snapshot.raw.activity_label' | sort | uniq -c` → ≥ 2 distinct labels after ~1 hour (T2).
   - `ls ~/.local/state/sy/power/checkpoint.json` → exists, `saved_at` recent (T4).
   - `sy power status --json | jq '{model, onboarding, bandit}'` → no schema regressions.

If onboarding finishes (day 14 = ~2026-06-03) before T3 lands AND the first retrain has fired against the rules-baseline-skewed corpus: rename `~/.local/state/sy/power/forecaster.onnx` to `forecaster.onnx.degenerate-pre-T3` so the daemon falls back to the shipped warmup ONNX. Document the manual recovery in the run log.
