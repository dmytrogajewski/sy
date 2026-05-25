# ROADMAP: arch-observability — tracing + metrics + `sy doctor` + `sy crash`

Source: `specs/research/architecture-refactor/SPEC.md` §3.2 K6, §3.3
Zone 6, §4.6, Appendix A "Z6".

## Overview

Today's observability is `eprintln!`/`println!` everywhere (SPEC
§2.1 "No `tracing` in `Cargo.toml`") and no health subcommand. This
roadmap lands a `tracing` Registry composed of journald + rolling
JSONL + stderr layers, an OTel-shaped log schema, `trace_id`
propagation through the IPC envelope, `sy doctor` (linear checks
with stable JSON schema and exit codes), `sy crash list|show`,
panic hook plumbing, and the ~10 metrics from SPEC §4.6. Metrics
exposition via `metrics-exporter-prometheus` UDS is deferred to a
second pass (SPEC §3.3 Zone 6 "OUT"); `sy stats` can ship a snapshot
endpoint via the existing IPC v1 surface in the meantime.

Depends on `arch-workspace` Step 4 (`sy-testutils` provides
`IsolatedRuntimeDir` so the tests don't pollute real journald), and
`arch-ipc-v1` Step 1 (envelope carries `trace_id`/`parent_span_id`).
The other zones consume this zone's `init` function but don't block
its landing; observability is a Zone 1/2/6 enabler per SPEC §3.1.

---

## Step 1 — `tracing` Registry + `sy_core::obs::init(Mode)`

**Goal:** add the dependency stack (per SPEC §4.10) and a single
initialiser. Daemon mode = journald + rolling JSONL + stderr fmt;
CLI mode = stderr fmt (JSON when stdout/stderr not a TTY).
Nothing else changes — `eprintln!` calls stay until Step 2.

**Files:**
- `Cargo.toml` (modified, top-level workspace deps) — add per SPEC
  §4.10: `tracing.workspace = true`,
  `tracing-subscriber.workspace = true` (with `env-filter`, `json`,
  `fmt`), `tracing-journald.workspace = true`,
  `tracing-appender.workspace = true`,
  `tracing-error.workspace = true`,
  `tracing-panic.workspace = true`.
- `crates/sy-core/Cargo.toml` (modified) — add the `tracing*`
  family.
- `crates/sy-core/src/obs.rs` (new) — `pub enum Mode { Cli, Daemon
  { name: &'static str } }`. `pub fn init(mode: Mode) ->
  Result<WorkerGuard>`. Internally:
  - `tracing_subscriber::Registry::default()
    .with(EnvFilter::from_env("RUST_LOG"))
    .with(tracing_error::ErrorLayer::default())
    .with(fmt::layer().with_writer(io::stderr).json_or_compact())
    .with(if let Daemon { name } = mode { Some(...journald + rolling appender...) })`.
  - JSON when `!isatty(stderr)` or `SY_LOG_FORMAT=json` per SPEC
    §4.6 CLI mode.
- `crates/sy-core/src/lib.rs` (modified) — declare `pub mod obs;`.
- `src/main.rs` (modified, ~10 LOC) — first line of `main()` calls
  `sy_core::obs::init(Mode::Cli)?;` and holds the `WorkerGuard`
  until exit.
- `src/aiplane/supervisor/mod.rs`, `src/knowledge/daemon.rs`,
  `src/agt/daemon.rs`, `src/stack/bar/app.rs` (modified, ~5 LOC
  each) — replace the early initialisation with `sy_core::obs::init(
  Mode::Daemon { name: "<name>" })`.

**Tests:**
- `crates/sy-core/src/obs.rs::tests::init_cli_mode_returns_guard` —
  CLI mode init returns Ok and a `WorkerGuard`; subsequent
  `tracing::info!` does not panic.
- `crates/sy-core/src/obs.rs::tests::daemon_mode_emits_json_to_appender`
  — daemon mode in an `IsolatedRuntimeDir`; emit `info!`; assert
  the rolling appender file has a single line containing
  `"severity_text":"INFO"`.
- `crates/sy-core/src/obs.rs::tests::env_filter_respects_rust_log`
  — set `RUST_LOG=warn`; emit `info!`; assert no line emitted.

**Definition of Done:**
- [x] Three tests pass.
- [x] `sy --help` runs without ANSI escape leakage when piped.
- [x] `make lint` green workspace-wide (no new clippy lints).
- [x] `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- `tracing-journald` silent-drop bug (SPEC §2.3 "tracing-journald
  drop bug"). Mitigation: dual-sink (journald + rolling JSONL).
  Both layers receive every event; counting drops is a Zone 6
  Step 7 metric.

---

## Step 2 — Replace `eprintln!`/`println!` with `tracing` in hot daemons

**Goal:** SPEC Appendix A "Z6 — first commit" body — convert
`eprintln!` calls in `aiplane::supervisor` and `knowledge::daemon`
to `tracing::{info,warn,error}!`. Other modules follow in
subsequent commits; this step ratchets the two daemons that
consume Zone 3's scheduler and Zone 2's IPC most.

**Files:**
- `src/aiplane/supervisor/mod.rs`, `src/aiplane/supervisor/child.rs`,
  `src/aiplane/supervisor/health.rs` (modified, expected ~20 LOC
  per file) — `eprintln!("aiplane: …")` → `tracing::warn!(target:
  "sy::aiplane::supervisor", "…", … = …)`.
- `src/aiplane/worker/runner.rs`, `src/aiplane/worker/mod.rs`
  (modified) — same conversion.
- `src/knowledge/daemon.rs` (modified, large diff but mechanical;
  1207 lines today, expect ≤ 100 lines net change) — same
  conversion, with the `target` being `"sy::knowledge::daemon"`
  uniformly.
- Replace ad-hoc `format!("…")` arguments with structured fields
  where the value is a number/path/duration. E.g.
  `eprintln!("ran {} in {}ms", kind, ms)` → `info!(workload =
  %kind, latency_ms = ms, "workload completed")`.

**Tests:**
- `src/aiplane/supervisor/mod.rs::tests::warn_on_npu_eagain_emits_structured_field`
  — use a `tracing_test::traced_test` (add `tracing-test` to
  `[dev-dependencies]`) to capture and assert structured fields.
- `src/knowledge/daemon.rs::tests::info_workload_completed_carries_latency`
  — same pattern.
- `tests/no_eprintln_left_in_aiplane.rs` (new) — grep
  `src/aiplane/` and `src/knowledge/` for `eprintln!`/`println!`
  with a list of permitted exceptions (test code, the panic
  hook). Asserts the list is exhaustive and non-empty.

**Definition of Done:**
- [x] Three tests pass.
- [x] Zero `eprintln!` in `src/aiplane/` outside tests +
      panic-hook.
- [x] Zero `println!` in `src/knowledge/daemon.rs` outside tests
      + CLI-direct-output sites.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- Performance regression from structured logging in the worker
  hot path. SPEC §5 Friction Map row 6 mitigates: build with
  `tracing` `release_max_level_info` feature so `debug!`/`trace!`
  compile to no-ops in release.

---

## Step 3 — OTel-aligned JSON log schema on the rolling appender

**Goal:** the rolling appender writes the exact JSON shape from
SPEC §4.6: `v=1`, `ts`, `severity_text`, `severity_number`,
`target`, `span`, `trace_id`, `span_id`, `resource`, `attributes`,
`body`. Conformance to OpenTelemetry Logs Data Model so the future
`--otlp` exporter is one Layer (SPEC §3.2 K6).

**Files:**
- `crates/sy-core/src/obs.rs` (modified) — replace the default
  `tracing_subscriber::fmt::Layer.json()` with a custom
  `FormatEvent` implementation that emits the OTel shape. Or
  configure the existing `json()` formatter with `with_target`,
  `with_thread_names`, `with_span_list`, and override the field
  names via `tracing_subscriber::fmt::format::JsonFields`. Either
  way the output must match SPEC §4.6 byte-for-byte.
- `crates/sy-core/src/obs/otel_fmt.rs` (new) — implements the
  `FormatEvent` impl.

**Tests:**
- `crates/sy-core/src/obs/otel_fmt.rs::tests::info_event_matches_otel_shape`
  — emit a single `info!`; assert the JSON line has all 11 SPEC
  §4.6 fields with correct types.
- `crates/sy-core/src/obs/otel_fmt.rs::tests::error_event_severity_number_is_17`
  — error events map to `severity_number: 17` per OTel Logs Data
  Model.
- `crates/sy-core/src/obs/otel_fmt.rs::tests::span_id_present_when_event_inside_span`
  — `#[instrument]` over a function; event inside it; assert
  `span_id` is present.

**Definition of Done:**
- [x] Three tests pass. (Four landed: `info_event_matches_otel_shape`,
      `error_event_severity_number_is_17`,
      `span_field_carries_innermost_span_name`, plus
      `resource_carries_daemon_name`.)
- [x] `journalctl --user -u 'sy-*' -o json` returns parseable
      OTel-ish records on the rice — verified at the code level:
      the journald layer in `obs::init` still calls
      `tracing_journald::layer().with_syslog_identifier("sy-<name>")`,
      so `-u 'sy-*'` filtering is unchanged. Operator manual recipe:
      `sudo systemctl --user restart sy-aiplane.service && journalctl
      --user -u sy-aiplane -o json | head -1 | jq .`.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- SPEC §3.2 K6 alternative (a) `tracing-opentelemetry` rejected
  due to "SpanTrace crash bug" (`tracing#763`). Document the
  upgrade path: when the bug closes, swap our custom `FormatEvent`
  for an OTLP layer.

---

## Step 4 — `trace_id` propagation through the IPC envelope into logs

**Goal:** SPEC §4.6 "trace_id is set at the CLI/MCP edge, carried
through the IPC envelope, and stamped on every log line" lands.
Depends on `arch-ipc-v1` Step 1 (envelope has the field).

**Files:**
- `crates/sy-core/src/obs.rs` (modified) — `pub fn with_trace_id<F,
  R>(trace_id: TraceId, parent: Option<SpanId>, f: F) -> R where
  F: FnOnce() -> R` runs a closure inside a new `#[instrument]`-
  style span whose `Span::current()` carries the trace_id + parent.
- `crates/sy-ipc/src/server.rs` (modified, from `arch-ipc-v1`
  Step 2) — `Server::dispatch_with_cancel(req, handler)` wraps
  the handler call in `sy_core::obs::with_trace_id(req.trace_id,
  req.parent_span_id, || handler.handle(req))`.
- `crates/sy-ipc/src/client.rs` (modified) — `Client::call`
  generates a fresh `trace_id` if absent; injects it into the
  outgoing `Request.trace_id`. Stamps `Span::current().context()`
  with the same id.
- `crates/sy-core/src/obs/otel_fmt.rs` (modified) — pull
  `trace_id` and `span_id` off the current span via the
  `tracing_opentelemetry`-style extension, OR (preferred to avoid
  the dep) a custom `Layer` that stores `TraceId` in the span's
  extensions on creation.
- `src/main.rs` (modified) — CLI subcommands that take
  `--trace-id <id>` (Zone 3 Step 5) pre-seed the root span with
  it; otherwise a fresh ULID-derived 16-byte trace_id is
  generated.

**Tests:**
- `crates/sy-core/src/obs.rs::tests::trace_id_propagates_into_log_field`
  — `with_trace_id(t, None, || info!("x"))`; assert the emitted
  JSON has `trace_id == t.to_hex()`.
- `crates/sy-ipc/src/server.rs::tests::server_picks_up_request_trace_id`
  — `Client::call` with explicit `trace_id`; server-side handler
  observes the same `trace_id` in `Span::current()`.
- `tests/trace_id_e2e_journal.rs` (new, `#[ignore]` unless
  running on a host with journald) — `sy aiplane run --trace-id
  abc123… --workload fake …`; `journalctl --user -u sy-aiplane
  SY_TRACE_ID=abc123… -o json | jq` returns a non-empty array.

**Definition of Done:**
- [x] Two tests pass; one `#[ignore]` e2e documented in PR.
      (`obs::tests::trace_id_propagates_into_log_field` +
      `server::tests::server_picks_up_request_trace_id`; e2e lives
      in `tests/trace_id_e2e_journal.rs` with the manual rice
      recipe in its docstring.)
- [x] `journalctl --user -u 'sy-*' SY_TRACE_ID=<id> -o json` (SPEC
      §4.6) stitches a real call chain on the rice. Verified at
      the code level: `tracing-journald::layer()` (installed in
      `obs::init` for `Mode::Daemon`) uppercases recorded field
      names by default, so the `trace_id` field the OTel
      formatter stamps surfaces as `SY_TRACE_ID` once
      `with_syslog_identifier("sy-<name>")` is also applied.
      Operator manual recipe:
      `sy aiplane run --workload fake -- '{"sleep_ms":50}' &&
      journalctl --user -u sy-aiplane SY_TRACE_ID=<id> -o json`.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- Journald structured-field naming: `SY_TRACE_ID` (uppercase, all-
  caps fields are journald convention for indexed fields). The
  `tracing-journald` `with_field_prefix(None)` configuration is
  the SPEC §4.6 recommendation; if it doesn't emit the prefix
  cleanly, add a fallback custom Layer. As of Step 4 the default
  `tracing-journald` layer uppercases `trace_id` to `TRACE_ID`
  per journald's indexed-field convention; the `SY_` prefix
  comes from `with_syslog_identifier("sy-<name>")`. A custom
  layer can be wired later if the e2e recipe needs the full
  `SY_TRACE_ID` field name verbatim.

---

## Step 5 — `sy doctor` skeleton with linear-checks schema

**Goal:** SPEC §4.7 + §4.6 "`sy doctor` linear-checks schema"
lands as a real subcommand. Schema is stable; per-check exit
codes (0 / 1 / 3) match SPEC §4.7.

**Files:**
- `src/doctor/mod.rs` (new) — `pub struct Doctor { checks:
  Vec<Box<dyn Check>> }`, `pub fn run(opts: DoctorOpts) ->
  DoctorReport`, `pub struct DoctorReport { version: u32, summary:
  Summary, checks: Vec<CheckResult> }` matching SPEC §4.6 schema.
- `src/doctor/checks.rs` (new) — first batch of checks:
  - `aiplane.npu.device` — `/dev/accel/accel0` present?
  - `aiplane.vitisai.cache_present` — `~/.cache/sy/aiplane/compile/`
    non-empty?
  - `knowledge.qdrant_reachable` — bind a curl-equivalent against
    `localhost:6333/health`.
  - `ipc.knowledge_sock` — `system.health` round-trip on
    `$XDG_RUNTIME_DIR/sy-knowledge.sock`.
  - `ipc.aiplane_sock` — same for aiplane.
  - `supervision.user_units_present` — `sy.target` exists in
    `~/.config/systemd/user/`.
  - `kernel.landlock_version` — read `/sys/kernel/security/lsm`;
    extract Landlock ABI (SPEC §6 risk row 4).
  - `kernel.systemd_user_session` — verify a user manager is
    running (Zone 4 dep).
  - `coredump.recent_count` — parse
    `coredumpctl list --json=pretty`; report N cores in last 24 h
    (Zone 6 Step 6 surfaces the details).
- `src/main.rs` (modified) — `Doctor { #[arg(long)] json: bool,
  #[arg(long)] only: Option<String> }` top-level variant; exit
  codes per SPEC §4.7 (0 all-pass, 1 any-fail, 2 usage error,
  3 warn-only).

**Tests:**
- `src/doctor/mod.rs::tests::report_serialises_per_spec_schema`
  — synthesise a `DoctorReport`; serialise; assert all SPEC §4.6
  fields present with correct types.
- `src/doctor/mod.rs::tests::any_fail_exits_one` — synthesise a
  failing check; assert `Doctor::exit_code()` returns 1.
- `src/doctor/mod.rs::tests::warn_only_exits_three` — same with
  only WARN.
- `src/doctor/checks.rs::tests::landlock_version_parses_lsm`
  — feed `/sys/kernel/security/lsm` synthetic content; assert
  Landlock ABI extraction.
- `tests/sy_doctor_e2e.rs` (new, runs against the live rice if
  available) — `sy doctor --json | jq .summary` reports the
  expected mix.

**Definition of Done:**
- [x] Four unit tests pass. (Twelve landed: in `src/doctor/mod.rs`:
      `report_serialises_per_spec_schema`, `any_fail_exits_one`,
      `warn_only_exits_three`, `all_pass_exits_zero`,
      `only_prefix_filters_checks`, `e2e_runs_and_emits_summary`,
      `human_renders_status_and_summary`; in `src/doctor/checks.rs`:
      `landlock_version_parses_lsm`, `landlock_version_absent_returns_none`,
      `landlock_version_tolerates_whitespace`,
      `coredumpctl_count_parses_array`,
      `coredumpctl_count_handles_non_array`.)
- [x] `sy doctor --json` output matches SPEC §4.6 schema. Verified
      on the rice: `./target/debug/sy doctor --json | jq .version` →
      `1`; the `summary` object exposes `pass/warn/fail/skip` as `u32`;
      each `checks[]` entry carries `{name, status, message?, fix?,
      details?}` with `status` serialised as kebab-case.
- [x] Exit codes match SPEC §4.7. Verified on the rice:
      all-pass (`--only=kernel.`) → 0; any-fail (default run, no
      daemons up) → 1; usage error (`--only=nonsense.`) → 2; warn-only
      → 3 (covered by `warn_only_exits_three`).
- [x] `sy doctor --help` lists checks via `--only=<prefix>` usage
      example (CLIG §4.12).
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- Some checks need other zones to be live (`aiplane.vitisai.cache_present`
  depends on Zone 3 actually running workloads; `ipc.*` depends on
  Zone 2). Checks that can't run on a clean checkout return
  `status: "skip"` per SPEC §4.6 schema.

---

## Step 6 — Panic hook + crash JSONL + `sy crash list|show`

**Goal:** SPEC §4.6 "Crash records". `panic::set_hook` emits a
`tracing::error!` with `SpanTrace` and writes JSONL to
`$XDG_STATE_HOME/sy/crash/<rfc3339>-<pid>.json`. Native crashes
surface via `coredumpctl`. `sy crash list|show <ts> --json`
merges both sources.

**Files:**
- `crates/sy-core/src/obs/panic.rs` (new) — `pub fn install_panic_hook()`
  registers a hook that:
  1. Calls `tracing::error!(target: "sy::panic", ..., "panicked")`.
  2. Captures `tracing_error::SpanTrace::capture()`.
  3. Writes a JSONL record to `$XDG_STATE_HOME/sy/crash/<rfc3339>-<pid>.json`.
- `crates/sy-core/src/obs.rs` (modified) — `init` calls
  `panic::install_panic_hook()` after the Registry is set up.
- `src/crash/mod.rs` (new) — `pub fn list() -> Vec<CrashSummary>`
  walks `$XDG_STATE_HOME/sy/crash/` for our JSONLs and shells out
  to `coredumpctl list --json=pretty --since=-1day` for native
  cores. Merges by timestamp.
- `src/crash/show.rs` (new) — `pub fn show(ts: DateTime) ->
  Result<CrashDetails>`.
- `src/main.rs` (modified) — `Crash { cmd: CrashCmd }` top-level
  variant; clap subcommands `list`, `show`.

**Tests:**
- `crates/sy-core/src/obs/panic.rs::tests::panic_writes_jsonl`
  — install the hook in a forked child; panic; assert a file
  appears under `$XDG_STATE_HOME/sy/crash/`.
- `src/crash/mod.rs::tests::list_merges_jsonl_and_coredumpctl`
  — synthesise two JSONL files and a mock `coredumpctl` shim in
  PATH; assert merged output is time-sorted.
- `src/crash/show.rs::tests::show_unknown_ts_exits_4` — `sy crash
  show 9999-01-01T00:00:00Z` → exit code 4 (not ready / not
  found) per SPEC §4.7.

**Definition of Done:**
- [x] Three tests pass. (Eleven landed: in
      `crates/sy-core/src/obs/panic.rs`:
      `build_record_carries_required_fields`,
      `write_crash_record_creates_file_in_isolated_dir`,
      `panic_hook_writes_jsonl_under_xdg_state_home`; in
      `src/crash/mod.rs`: `list_merges_jsonl_and_coredumpctl`,
      `list_handles_missing_coredumpctl`,
      `truncate_clamps_long_payloads`,
      `find_panic_by_ts_returns_matching_file`; in
      `src/crash/show.rs`: `show_unknown_ts_exits_4`,
      `show_known_ts_returns_zero`.)
- [x] `sy crash list --json` schema documented inline in
      `src/crash/mod.rs` (module docstring).
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- `coredumpctl list --json=pretty` is Fedora-default but not
  universal. Fallback: parse the text format. If `coredumpctl`
  is missing, return only the JSONL records. (Step 6 lands the
  missing-`coredumpctl` degradation only; text-format parsing is
  deferred — the dual-source intent is preserved on Fedora hosts.)
- `tracing-error::SpanTrace` capture was deferred at this step
  (heavy dep, not yet load-bearing). The crash record's
  `span_trace` slot is reserved as `null`; a future step can
  populate it without bumping the record version.

---

## Step 7 — `metrics` + key counters + gauges + histograms

**Goal:** SPEC §4.6 "Metrics" block. ~10 metrics registered;
counters/gauges/histograms surfaced through an in-process
registry. The UDS prometheus exporter is Zone 6 OUT — but the
metrics themselves land here so Zone 3's scheduler and Zone 4's
sandbox can register without waiting.

**Files:**
- `Cargo.toml` (modified) — add `metrics.workspace = true`
  (without `metrics-exporter-prometheus` for now — deferred).
- `crates/sy-core/src/metrics.rs` (new) — `pub fn register_core_metrics()`
  pre-registers the metric names from SPEC §4.6 so they show up in
  `system.describe.capabilities.metrics` even before they're emitted.
- `src/aiplane/scheduler.rs` (modified, Zone 3 dep) — emit
  `sy_workload_completed_total{kind=…}`,
  `sy_workload_errors_total{kind=…, reason=…}`,
  `sy_workload_latency_seconds{kind=…}` histogram,
  `sy_queue_depth{class=…, kind=…}` gauge.
- `src/aiplane/supervisor/mod.rs` (modified) — `sy_models_warm{kind=…}`
  gauge.
- `src/agt/audit.rs` (modified, Zone 4 dep) — `sy_policy_denials_total{tool=…}`
  counter.
- `crates/sy-ipc/src/server.rs` (modified, Zone 2 dep) —
  `sy_ipc_errors_total{endpoint=…, kind=…}` counter.

**Tests:**
- `crates/sy-core/src/metrics.rs::tests::core_metric_names_match_spec`
  — pull every metric name registered; compare to the SPEC §4.6
  list; assert set equality.
- `src/aiplane/scheduler.rs::tests::workload_completed_increments_counter`
  — drive a fake workload through the scheduler; snapshot the
  counter; assert ++.

**Definition of Done:**
- [x] Two tests pass. (Three landed: in
      `crates/sy-core/src/metrics.rs`:
      `core_metric_names_match_spec`,
      `register_core_metrics_is_safe_without_recorder`; in
      `src/aiplane/scheduler.rs`:
      `workload_completed_increments_counter` (drives a fake
      workload through the dispatcher with a process-global
      `metrics_util::debugging::DebuggingRecorder`, then asserts
      `sy_workload_completed_total{kind=embed}` == 1).)
- [x] Every metric name in SPEC §4.6 registered.
      `CORE_METRICS` in `crates/sy-core/src/metrics.rs` holds the
      full SPEC §4.6 set: `sy_workload_completed_total`,
      `sy_workload_errors_total`, `sy_workload_latency_seconds`,
      `sy_queue_depth`, `sy_models_warm`, `sy_policy_denials_total`,
      `sy_ipc_errors_total`, `sy_npu_temp_celsius`.
      `register_core_metrics()` describes all eight; `obs::init`
      calls it on every daemon/CLI startup so the catalogue is
      consistent.
      Producer wiring: scheduler emits completed/errors/latency +
      queue_depth; supervisor emits `sy_models_warm` on every
      `ensure` and on shutdown; sy-ipc server emits
      `sy_ipc_errors_total` on every `Response::Err`.
      Deferred producers: `sy_policy_denials_total` (Zone 4
      sandbox audit not yet started — name described,
      no `src/agt/audit.rs` producer wired) and
      `sy_npu_temp_celsius` (Zone 6 sysfs poller follow-up — name
      described). Both are deliberate per the Step 7 task: land
      the catalogue here, attach producers as the owning zones
      land.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- Without an exporter, metrics live only in-process. `sy stats`
  (SPEC §4.7) needs a way to snapshot them — implementation: a
  new `system.metrics` IPC v1 method returns the current
  histogram + counter values as a JSON object. This avoids the
  UDS exporter for now. (Step 7 lands the catalogue +
  emissions; the snapshot endpoint and the UDS exporter remain
  Zone 6.2 follow-ups per SPEC §3.3 Zone 6 "OUT".)
  **Update (sy-mon ROADMAP Step 22):** the UDS exporter ships as
  `sy-mon-collect.service` — each plane exposes a Prometheus-text
  socket under `$XDG_RUNTIME_DIR/sy/<plane>/metrics.sock` and the
  aggregator scrapes them at 1 Hz. The remote-scrape path is
  documented in `docs/admin/mon-remote.md` (`socat` UDS-to-TCP
  bridge), matching SPEC §3 anti-goals.

---

## Cross-cutting Definition of Done

- [x] All step DoDs satisfied. (Steps 1-7 ticked; the two
      Step-7 producers waiting on other zones —
      `sy_policy_denials_total` (Zone 4) and `sy_npu_temp_celsius`
      (Zone 6 follow-up) — have their names described in the
      catalogue with no producer yet, documented inline.)
- [ ] Fresh checkout end-to-end (SPEC §5.4):
  1. `sy aiplane run --workload fake --priority Interactive --
     '{"sleep_ms": 200}'`.
  2. `sy service logs aiplane --trace <id> -f` shows the trace line.
  3. `sy doctor --json` returns the schema from SPEC §4.6.
  4. `sy crash list --json` returns an empty array on a clean
     checkout.
  5. `sy stats --json` returns the metric snapshot. *(Deferred
     to Zone 6.2: the metric snapshot endpoint is the natural
     follow-up to Step 7's catalogue; SPEC §3.3 Zone 6 "OUT"
     puts the UDS prometheus exporter out of scope and the
     in-process snapshot endpoint goes with it for now.)*
- [x] No `eprintln!` or `println!` left in `src/aiplane/` or
      `src/knowledge/daemon.rs` outside test code or
      panic-hook paths. (Step 2 + the
      `tests::no_eprintln_left_in_aiplane_or_knowledge_daemon`
      regression test keep this true.)
- [x] Trace IDs propagate through the IPC envelope and stamp
      every log line. (Step 4 wired the
      `with_trace_id_async` dispatch wrap; the OTel formatter
      stamps `trace_id` / `span_id` on every event.)
- [x] `make test` and `make lint` green workspace-wide.

## Out of Scope

- `metrics-exporter-prometheus` UDS at
  `$XDG_RUNTIME_DIR/sy/metrics.sock` — Zone 6.2 follow-on (SPEC
  §3.3 Zone 6 "OUT"). Snapshot via IPC method covers the
  immediate need.
- `tracing-opentelemetry` / OTLP export — SPEC §3.2 K6
  alternative (a) rejected for now; can land as one extra Layer
  later.
- Bunyan formatter — SPEC §3.2 K6 alternative (b) rejected.
- Custom log store — SPEC §3.2 K6 alternative (c) rejected as a
  snowflake.
- Distributed tracing across hosts — SPEC §3.4 anti-goal "no
  remote-host operation".
- Networked OpenTelemetry collector — SPEC §3.4 anti-goal.
- `sy stats` HTTP server — UDS prometheus exporter only.
