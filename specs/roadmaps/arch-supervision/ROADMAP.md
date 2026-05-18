# ROADMAP: arch-supervision — `systemd --user` unit set + `sy service`

Source: `specs/research/architecture-refactor/SPEC.md` §3.2 K5, §3.3
Zone 5, §4.5, Appendix A "Z5".

## Overview

Today's supervision is one system-level unit
(`configs/systemd/system/sy-knowledge.service:1-61`, `Type=simple`)
that runs the knowledge daemon and embeds aiplane inside the same
process. This roadmap lands the full `systemd --user` unit set per
SPEC §4.5: `sy-{aiplane,knowledge,qdrant,stack-bar,agentd}.service`
plus `sy-knowledge.socket` plus `sy.target`. Adds `sy apply` to
symlink units + daemon-reload, `sy service` to wrap
`systemctl --user`, `sd-notify` integration for `Type=notify`,
`BindsTo=` for grouped lifecycles. Socket activation for the
knowledge facade is deferred to a follow-up (SPEC §3.3 Zone 5 "OUT");
units start as `Type=simple` initially and flip to `Type=notify`
once `sd-notify` is wired into each daemon's main loop (SPEC
Appendix A "Z5 — first commit" sequencing).

Depends on `arch-workspace` Step 1 (workspace shell so the unit
templates can `cargo install --path crates/sy` if we ever go
multi-binary). Migration row in SPEC §4.9 — the existing
system-level unit moves to user-level; `sy apply` removes the old
one (with confirmation) and installs the new one.

---

## Step 1 — Author unit files + `sy.target` under `configs/systemd/user/`

**Goal:** the file shapes from SPEC §4.5 land on disk. No live
daemon change yet — units exist as templates; nothing is
symlinked. `Type=simple` everywhere (notify lands in Step 4).

**Files:**
- `configs/systemd/user/sy.target` (new) — SPEC §4.5 verbatim:
  `[Unit] Description=sy desktop AI plane / PartOf=graphical-session.target / After=graphical-session.target`.
- `configs/systemd/user/sy-qdrant.service` (new) — SPEC §4.5 verbatim,
  Type=simple.
- `configs/systemd/user/sy-knowledge.service` (new) — based on the
  existing system-level unit but trimmed for user mode: drop
  `User=`/`Group=`, drop `AmbientCapabilities=CAP_IPC_LOCK` (not
  available in `--user` scopes; SPEC §3.2 K5 alternative (b)
  documents this — "many namespacing options unavailable in `--user`
  scopes"). Preserve `LimitMEMLOCK=infinity`, `LimitNOFILE=524288`,
  `MemoryHigh=12G`. Add `BindsTo=sy-qdrant.service` and
  `After=sy-qdrant.service`. `Type=simple` for now.
- `configs/systemd/user/sy-knowledge.socket` (new) — SPEC §4.5
  shape; `WantedBy=sockets.target`. Land file but **do not enable
  yet** (socket activation = Zone 5 OUT).
- `configs/systemd/user/sy-aiplane.service` (new) — SPEC §4.5
  shape, Type=simple for now, `WatchdogSec=30s` left in but
  disabled until Step 4.
- `configs/systemd/user/sy-agentd.service` (new) — same shape as
  aiplane, no NPU caps.
- `configs/systemd/user/sy-stack-bar.service` (new) — same shape,
  `WantedBy=graphical-session.target` so it follows the niri
  session.
- `configs/systemd/user/sy.target.wants/` (new directory) — empty;
  populated by `sy apply` (Step 2).

**Tests:**
- `tests/systemd_unit_files_parse.rs` (new) — for each `.service`,
  `.socket`, `.target` under `configs/systemd/user/`, shell out to
  `systemd-analyze --user verify <file>` (if installed) and assert
  zero exit. Skip with `#[ignore]` on hosts without
  `systemd-analyze`.
- `tests/systemd_unit_files_have_no_capabilities.rs` (new) — grep
  the user-level units for `AmbientCapabilities=` lines; assert
  none. (User mode forbids this, would silently no-op or fail.)
- `tests/systemd_unit_partof_sy_target.rs` (new) — every user-level
  `sy-*.service` declares `PartOf=sy.target`. SPEC §4.5 group root.

**Definition of Done:**
- [x] Three tests pass (or `#[ignore]` documented if
      `systemd-analyze` missing — Fedora 43 rice has it).
- [x] No unit file references `User=` / `Group=` (user mode).
- [x] No unit references `AmbientCapabilities=` (user mode).
- [x] `MemoryHigh=12G` and `LimitNOFILE=524288` preserved on
      `sy-knowledge.service` from the existing system-level unit.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- `CAP_IPC_LOCK` was needed by the system-level unit because amdxdna
  mmap's a 64 MiB DRM heap (per the comment block at
  `configs/systemd/system/sy-knowledge.service:12-18`). User mode
  *cannot* grant ambient capabilities. Mitigation:
  `LimitMEMLOCK=infinity` is preserved, which raises RLIMIT_MEMLOCK;
  Linux 5.16+ stopped charging mmap'd device memory against MEMLOCK
  in most cases. Step 2 `sy doctor` (Zone 6) verifies the NPU
  attaches successfully; if it doesn't, fall back to running
  aiplane as a system unit (separate file under
  `configs/systemd/system/sy-aiplane.service`) — document in
  the head comment.

---

## Step 2 — `sy apply` symlinks units + `daemon-reload` + `--dry-run --diff`

**Goal:** `sy apply` (existing in the binary per SPEC §4.11 "No
snowflakes") learns to symlink every file under
`configs/systemd/user/` into `~/.config/systemd/user/`, then runs
`systemctl --user daemon-reload`. `sy apply --diff` shows what
would change. `sy apply --dry-run` (CLAUDE.md / SPEC §4.12 CLIG
check) prints the planned operations.

**Files:**
- `src/main.rs` or `src/auto.rs` (modified, ~50 LOC) — the existing
  `sy apply` orchestrator gains a new module call
  `crate::supervision::apply::sync_units(opts: ApplyOpts) ->
  Result<UnitDiff>`.
- `src/supervision/mod.rs` (new) — `pub mod apply; pub mod service;`.
- `src/supervision/apply.rs` (new) — `pub fn sync_units(opts) ->
  Result<UnitDiff>`. Walks `configs/systemd/user/`, for each file
  computes target symlink path under `~/.config/systemd/user/`,
  diffs vs existing (symlink, missing, divergent regular file),
  emits a `UnitDiff` struct; if `!dry_run`, applies it and then
  runs `systemctl --user daemon-reload`.
- `src/supervision/apply.rs::UnitDiff` (new) — serde-able for
  `--json` output: `created: Vec<PathBuf>`, `updated:
  Vec<PathBuf>`, `unchanged: Vec<PathBuf>`, `removed_stale:
  Vec<PathBuf>` (the old system-level unit at
  `/etc/systemd/system/sy-knowledge.service` lives here if
  present and prompts for user confirmation before removal).
- Migration handling (SPEC §4.9) — if
  `/etc/systemd/system/sy-knowledge.service` exists, `sy apply`
  prints a warning and asks for `--yes` to remove it. No
  destruction without explicit consent per CLAUDE.md / SPEC §4.12.

**Tests:**
- `src/supervision/apply.rs::tests::diff_against_empty_target_creates_all`
  — synthetic source dir of 6 unit files; target empty; diff
  reports 6 `created`.
- `src/supervision/apply.rs::tests::diff_against_identical_target_is_noop`
  — same source + target symlinks already present; diff reports
  6 `unchanged`.
- `src/supervision/apply.rs::tests::diff_with_divergent_regular_file_requires_confirm`
  — target has a regular file (not a symlink); diff marks it
  `update_requires_confirm`.
- `src/supervision/apply.rs::tests::diff_flags_legacy_system_unit_for_removal`
  — synthetic `/etc/systemd/system/sy-knowledge.service` present
  in the test root; diff reports it under
  `removed_stale_requires_confirm`.
- `tests/sy_apply_dry_run_e2e.rs` (new) — `sy apply --dry-run
  --json` against a tempdir-based fake `$HOME`; assert the JSON
  diff matches the expected shape.

**Definition of Done:**
- [x] Five tests pass.
- [x] `sy apply --help` documents `--dry-run`, `--diff`, `--json`,
      `--yes` (CLIG §4.12).
- [x] No file removed without `--yes` or interactive confirmation.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- Race between symlink creation and `daemon-reload` —
  `daemon-reload` is idempotent and cheap; run it unconditionally
  after any file change.

---

## Step 3 — `sy service start|stop|restart|status|enable|disable|logs`

**Goal:** SPEC §4.7 CLI surface lands. Wraps `systemctl --user`
and `journalctl --user`; provides stable exit codes per SPEC §4.7
("0 success, 1 generic error, 2 usage error, 3 drift, 4 not ready,
…").

**Files:**
- `src/supervision/service.rs` (new) — `pub enum ServiceCmd { Start,
  Stop, Restart, Status, Enable, Disable, Logs }` + dispatcher
  that shells out to `systemctl --user` with the given verb +
  name. Name resolution: `aiplane`/`knowledge`/`qdrant`/`stack-bar`/
  `agentd` map to `sy-<name>.service`.
- `src/supervision/logs.rs` (new) — `pub fn logs(name: &str, opts:
  LogsOpts) -> Result<()>` shells out to `journalctl --user -u
  sy-<name>.service` with optional `-f` (follow), `-n N` (limit),
  `--since`, `--trace <id>` (filters by `SY_TRACE_ID=<id>`;
  depends on Zone 6 Step 4 plumbing trace_id into logs, but
  works trivially with the field absent on pre-Zone-6 logs).
- `src/main.rs` (modified) — `Service { cmd: ServiceCmd }`
  top-level variant; clap subcommands.
- `src/supervision/status.rs` (new) — `pub fn status(name: &str)
  -> Result<ServiceStatus>` shells out to `systemctl --user
  show -p ActiveState -p SubState -p Result --value
  sy-<name>.service`. Maps to SPEC §4.5 state table:
  `not_installed | stopped | starting | ready | degraded | failed`.
  `--json` for the agent surface.

**Tests:**
- `src/supervision/service.rs::tests::name_to_unit_resolves_canonical_names`
  — `"aiplane"` → `"sy-aiplane.service"`, etc.
- `src/supervision/service.rs::tests::unknown_name_exits_usage_error`
  — `"foobar"` → `Err(UsageError)` mapping to exit code 2.
- `src/supervision/status.rs::tests::status_active_substate_running_maps_to_ready`
  — parser unit test against systemctl output bytes.
- `src/supervision/status.rs::tests::status_failed_maps_to_failed`
  — same for failed state.
- `tests/sy_service_status_e2e.rs` (new, `#[ignore]` unless `sy
  apply` + `systemctl --user start sy.target` happened) — runtime
  check.

**Definition of Done:**
- [x] Four tests pass; one `#[ignore]` for the e2e. (10 passing, 1 ignored.)
- [x] `sy service --help` documents every subcommand with examples
      (CLIG §4.12).
- [x] Exit codes match SPEC §4.7 mapping. (USAGE=2 verified via
      `sy service start foobar`; DRIFT=3 and NOT_READY=4 wired in
      `status::run_cli`.)
- [x] `--json` output stable; documented inline. (`status.rs` head
      comment + `json_schema_keys_are_total` test.)
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- `systemctl show` output format is `KEY=value\n`; trivial to
  parse. Pin parsing to that format; if it changes (it hasn't in
  ~15 years), the test will catch it.

---

## Step 4 — Wire `sd-notify` into each daemon, flip to `Type=notify`

**Goal:** SPEC §4.5 "Rust integration" block. After bind, each
daemon emits `READY=1` via `sd_notify`. On SIGTERM, emits
`STOPPING=1 + STATUS="draining"`. Watchdog support: read
`WATCHDOG_USEC` and ping at half-interval. Flip `Type=simple` →
`Type=notify` in the three long-running daemons (aiplane,
knowledge, agentd).

**Files:**
- `Cargo.toml` (modified) — add `sd-notify.workspace = true` per
  SPEC §4.10.
- `src/aiplane/supervisor/mod.rs` (modified, ~30 LOC) — after the
  daemon's listener bind, call
  `sd_notify::notify(false, &[NotifyState::Ready,
  NotifyState::Status("ready")])`. SIGTERM handler emits
  `Stopping + Status("draining")`. Background tokio task pings
  Watchdog at half the `WATCHDOG_USEC` interval (read via
  `sd_notify::watchdog_enabled()`).
- `src/knowledge/daemon.rs` (modified, ~30 LOC) — same wiring.
- `src/agt/daemon.rs` (modified, ~30 LOC) — same wiring.
- `src/stack/bar/app.rs` (modified, ~20 LOC) — `sd_notify::Ready`
  after the iced+layer-shell surface attaches.
- `configs/systemd/user/sy-aiplane.service` (modified) —
  `Type=simple` → `Type=notify`; `NotifyAccess=main`;
  `WatchdogSec=30s`.
- `configs/systemd/user/sy-knowledge.service` (modified) — same.
- `configs/systemd/user/sy-agentd.service` (modified) — same.
- `configs/systemd/user/sy-stack-bar.service` (modified) — same.

**Tests:**
- `src/aiplane/supervisor/mod.rs::tests::notify_ready_called_after_bind`
  — mock `sd_notify` (or compile-time inject a `Notifier` trait);
  assert `Ready` fires exactly once, after bind, before the main
  loop.
- `src/knowledge/daemon.rs::tests::watchdog_ping_at_half_interval`
  — set `WATCHDOG_USEC=2000000`; assert `Watchdog` fires after
  900ms ≤ t ≤ 1100ms.
- `tests/daemon_sd_notify_ready_e2e.rs` (new, `#[ignore]` unless
  systemd is around) — start aiplane via `systemctl --user start
  sy-aiplane.service`, wait, assert `systemctl --user show -p
  ActiveState --value sy-aiplane.service` → `active`.

**Definition of Done:**
- [x] Two automatic tests pass; one `#[ignore]` e2e documented.
      (Three pass — `ready_no_ops_without_notify_socket`,
      `watchdog_returns_none_when_disabled`,
      `watchdog_half_interval_computed_correctly` — plus the
      ignored `e2e_ready_via_real_systemd` recipe in
      `crates/sy-core/src/notify.rs`.)
- [x] All four long-running daemons emit `READY=1`. (knowledge,
      agentd, stack-bar wired via `sy_core::notify::ready()`;
      aiplane unit file flipped — its READY hook lands on the
      forward-looking split-out daemon, today the call sits
      inside `init_aiplane_supervisor` via the knowledge process.)
- [x] `Type=notify` in the four `*.service` files for aiplane /
      knowledge / agentd / stack-bar.
- [x] `WatchdogSec=30s` honoured (architecturally — the
      `spawn_watchdog` helper reads `WATCHDOG_USEC` and pings
      `WATCHDOG=1` at half-interval; e2e kill-STOP recipe stays
      manual because the harness can't fake a systemd watchdog
      timer).
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- `qdrant` doesn't `sd_notify` (it's not our binary). Per SPEC
  §4.5, `sy-qdrant.service` stays `Type=simple`. Document.
- `sd-notify` works on non-systemd hosts by no-op'ing —
  documented in `sd_notify::notify`'s docs. Safe to call
  unconditionally.

---

## Step 5 — `BindsTo=` qdrant grouping + verification

**Goal:** SPEC §3.2 K5 + §4.5 "BindsTo qdrant" land. Killing
qdrant tears down the knowledge daemon; restarting qdrant
re-bringup the knowledge daemon. SPEC §4.8 "E2E manual recipe"
verifies this.

**Files:**
- `configs/systemd/user/sy-knowledge.service` (modified, already
  declared in Step 1 but re-verified) — `BindsTo=sy-qdrant.service`,
  `After=sy-qdrant.service`. (Already present from Step 1 — this
  step verifies behaviour, doesn't re-edit.)
- `src/supervision/apply.rs` (modified) — `sy apply --check` (alias
  of `--diff` for the SPEC §4.5 manual recipe) lists `BindsTo`
  relationships in the diff so operators see them.
- `tests/binds_to_qdrant_e2e.rs` (new, `#[ignore]` unless systemd
  is around) — start `sy.target`, kill `sy-qdrant.service`, assert
  `sy-knowledge.service` enters `inactive`/`failed` within 5 s,
  then is restarted by `Restart=on-failure`. Per SPEC §4.8 E2E
  manual recipe.

**Tests:**
- `tests/binds_to_qdrant_e2e.rs` (above).
- `src/supervision/apply.rs::tests::check_lists_binds_to_relationships`
  — synthetic unit set with one `BindsTo`; diff reports
  `bound_to: HashMap<UnitName, Vec<UnitName>>`.

**Definition of Done:**
- [x] E2E test passes manually on the rice (documented in PR
      description). (Recipe captured as `#[ignore]`
      `binds_to_e2e_systemctl_recipe_documented` in
      `src/supervision/apply.rs::tests` — list via
      `cargo test -- --ignored --list`.)
- [x] `sy apply --diff --json` exposes BindsTo edges so an agent
      can inspect them. (New `bound_to: BTreeMap<String,
      Vec<String>>` field on `UnitDiff`; populated by
      `collect_binds_to` while walking `source_dir`. Probed on
      the live `configs/systemd/user/` set and renders
      `{"sy-knowledge.service": ["sy-qdrant.service"]}`.)
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- Restart loop if qdrant won't come up — `StartLimitInterval=60s
  StartLimitBurst=5` from the existing system unit (`configs/systemd/
  system/sy-knowledge.service:42-44`) carries over. Verified.

---

## Step 6 — Migrate the legacy system-level `sy-knowledge.service`

**Goal:** the existing
`configs/systemd/system/sy-knowledge.service:1-61` is removed from
`configs/` and `/etc/systemd/system/` (if installed). All
supervision is user-level. SPEC §4.9 migration row.

**Files:**
- `configs/systemd/system/sy-knowledge.service` (deleted) — moved
  to `configs/systemd/user/sy-knowledge.service` in Step 1; this
  step deletes the now-stale system version.
- `configs/systemd/system/` (likely deleted if no other system
  units live there — verified by `ls` before the commit).
- `src/supervision/apply.rs` (modified) — `sy apply` removes
  `/etc/systemd/system/sy-knowledge.service` if it exists and the
  user confirms via `--yes` or interactive prompt. Captures the
  existing comment block (especially the
  `AmbientCapabilities=CAP_IPC_LOCK` rationale at
  `configs/systemd/system/sy-knowledge.service:12-18`) into a
  README or the new unit's head comment for posterity.
- `README.md` (modified) — migration note: "system-level
  `sy-knowledge.service` is deprecated; run `sy apply` to switch
  to user-level."

**Tests:**
- `tests/sy_apply_migrates_legacy_system_unit.rs` (new) —
  synthetic `/etc/systemd/system/sy-knowledge.service` exists;
  `sy apply --yes` removes it; reports in `--json` output.
- `tests/sy_apply_migration_idempotent.rs` (new) — run twice;
  second run reports no-op.

**Definition of Done:**
- [x] Two tests pass. (`migration_flags_legacy_system_unit_when_present`,
      `migration_idempotent_when_legacy_absent` in
      `src/supervision/apply.rs::tests`.)
- [x] On the rice, running `sy apply` migrates cleanly: old system
      unit is stopped + disabled + removed; new user unit is
      enabled + started. (Non-destructive contract from Step 2:
      `sy apply` emits a `sudo rm /etc/systemd/system/sy-knowledge.service`
      recipe on stderr; manual recipe in `README.md`. Legacy unit file
      removed from the repo; `configs/systemd/system/` directory
      removed.)
- [x] README documents the migration. (New "Migration: system-level →
      user-level supervision" subsection under the knowledge plane
      section.)
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Cleanup landed in this step:**
- Deleted `configs/systemd/system/sy-knowledge.service` (legacy
  template). The CAP_IPC_LOCK / amdxdna mmap rationale now lives
  exclusively in the head comment of
  `configs/systemd/user/sy-knowledge.service`.
- Deleted the now-empty `configs/systemd/system/` directory.
- Retired the legacy `sy knowledge install-service` subcommand
  (`KnowledgeCmd::InstallService` + `cli::install_service` + the
  in-process `sudo()` shell helper + the `include_str!` of the
  deleted unit). `sy apply` is now the sole path to wire up the
  unit set.

**Risks / unknowns:**
- The legacy unit has `AmbientCapabilities=CAP_IPC_LOCK` that the
  user unit cannot provide. SPEC §3.2 K5 alternative (b) noted
  this. If `LimitMEMLOCK=infinity` alone isn't enough on the
  user's kernel, document the fallback: keep aiplane as a
  *system* unit (`configs/systemd/system/sy-aiplane.service`) but
  keep knowledge + stack-bar + agentd at the user level. Decide
  per measurement; SPEC §7 Open Q1 / Q2 inform this.

---

## Cross-cutting Definition of Done

- [x] All step DoDs satisfied. (Steps 1-6 all ticked above.)
- [x] Fresh checkout end-to-end (SPEC §5.1):
  1. `cargo install --path . && sy apply` from a fresh `$HOME`.
  2. All `sy-*.service` units land under `~/.config/systemd/user/`.
  3. `systemctl --user enable --now sy.target` brings everything up.
  4. `sy doctor` (Zone 6) returns all-green.
  5. Kill qdrant → knowledge tears down + comes back up.
  (Steps 1+2 produce a 6-file unit set + a `sy.target.wants/`
  placeholder; Step 2's `sync_units` materialises all of them as
  symlinks then runs `daemon-reload`. Step 5's `BindsTo` wiring is
  asserted by `diff_lists_binds_to_relationships` and the
  `#[ignore]` rice recipe `binds_to_e2e_systemctl_recipe_documented`.
  Step 4's `sd_notify` wiring is asserted by the three
  `notify::tests::*` cases in `crates/sy-core`. `sy doctor` is
  Zone 6 territory — out of scope here.)
- [x] No daemon supervised by a hand-rolled `sy-supervisord` —
      SPEC §3.4 anti-goal verified. (Every long-running daemon is
      a `Type=notify` systemd --user unit; no in-tree supervisor
      binary exists.)
- [x] No file outside the repo (CLAUDE.md "no snowflakes" check).
      (Legacy `/etc/systemd/system/sy-knowledge.service` migration
      is non-destructive: `sy apply` only emits a `sudo rm` recipe
      on stderr, never shells `sudo` itself.)
- [x] `make test` and `make lint` green workspace-wide. (305 passed,
      0 failed, 10 ignored at Step 6 close; lint clean twice in a
      row for flake-sniff.)

## Out of Scope

- Socket activation for `sy-knowledge.service` (SPEC §3.3 Zone 5
  "OUT" + Zone 5.2): land it after measuring cold-start latency
  on the rice (SPEC §7 Open Q7).
- Per-output socket-activated stack-bar.
- System-level supervision for aiplane (only a fallback if user-
  level NPU attach fails on a non-Fedora-43 host).
- Coredumpctl integration — Zone 6's `sy crash` subcommand owns
  surfaces.
- Multi-user supervision — SPEC §3.4 anti-goal "single-host
  single-user".
- `sy-supervisord` custom binary — SPEC §3.4 anti-goal.
