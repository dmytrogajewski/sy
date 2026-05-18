# ROADMAP: arch-agent-sandbox — Landlock + seccomp + systemd-run scope

Source: `specs/research/architecture-refactor/SPEC.md` §3.2 K4, §3.3
Zone 4, §4.4, Appendix A "Z4".

## Overview

Today's agent permission flow is `src/agt/permission.rs:1-50` — a
`notify-send` prompt with auto-allow on timeout, no sandbox, no
audit log. This roadmap lands a layered sandbox: profile + per-tool
policy (TOML — see Risks below for the SPEC §7 Q4 resolution),
in-process Landlock + seccompiler + `PR_SET_NO_NEW_PRIVS` + env
scrub, wrapped in `systemd-run --user --scope` for cgroup caps. Adds
a dual-sink audit log (journald + JSONL with rotation) and a
TTY-driven consent UX (`sy approve <token>`, `sy policy grant`). The
bwrap second layer and the xdg-desktop-portal Notification consent
flow are deferred (SPEC §3.3 Zone 4 "OUT", subzones 4.2 / 4.3).

Lands the policy resolver in commit 1 with no enforcement so the
diff is reviewable as policy-only; enforcement lands in commit 2 per
SPEC Appendix A "Z4 — first commit". TOML over KDL resolves SPEC §7
Q4 — matches the existing `configs/sy/agents.toml` convention; no
new parser dep.

Depends on `arch-workspace` Step 3 (`ErrorCode::{PolicyDenied,
ConsentRequired}` live in `sy-core`) and `arch-ipc-v1` Step 6
(`sy-agentd` already speaks v1, so `policy_denied` flows back as a
structured error). `arch-observability` Step 4 (`trace_id`
propagation) is *not* a hard dep — audit log carries
`SY_TRACE_ID=""` if Zone 6 hasn't landed yet.

---

## Step 1 — Policy file schema + resolver (no enforcement)

**Goal:** the TOML files from SPEC §4.4 land under
`configs/policy/`, a `sy_agt::policy::Resolver` parses them and
exposes `decide(tool: &str, argv: &[String]) -> Decision { Allow,
Deny, ConsentRequired }`. No enforcement, no sandbox spawn —
read-only resolver.

**Files:**
- `configs/policy/profiles/strict.toml` (new) — per SPEC §4.4:
  `read_paths = ["$REPO"]`, `write_paths = []`,
  `deny_network = true`, `require_consent = "every_call"`,
  `max_runtime_seconds = 30`, `max_memory_mb = 512`,
  `max_pids = 64`, `exec_allowlist = []` (intentionally empty).
- `configs/policy/profiles/normal.toml` (new) — SPEC §4.4 example
  verbatim: rg/cargo/git allowlisted, github.com:443 + crates.io:443
  allowed, `require_consent = "once_per_session"`,
  `max_runtime_seconds = 60`, `max_memory_mb = 1024`,
  `max_pids = 256`.
- `configs/policy/profiles/trusted.toml` (new) — `deny_network =
  false`, `require_consent = "never"`, `exec_allowlist = ["*"]`,
  with a doc comment at the top: "Requires `sy policy trust
  --confirm` from a TTY (SPEC §4.4 step 2)."
- `configs/policy/tools/` (new directory, empty initially — per-
  tool overlays live here).
- `src/agt/policy/mod.rs` (new) — `pub mod resolver; pub mod
  schema;`.
- `src/agt/policy/schema.rs` (new) — serde structs:
  `Profile { read_paths, write_paths, exec_allowlist:
  Vec<ExecAllow>, net_outbound_allowlist: Vec<NetAllow>,
  env_passthrough_allowlist: Vec<String>, max_runtime_seconds: u64,
  max_stdout_bytes: u64, max_memory_mb: u64, max_pids: u64,
  deny_network: bool, require_consent: ConsentMode }`,
  `ExecAllow { bin: PathBuf, argv: Vec<String> }` with glob support
  (`*`, `test*`), `NetAllow { host: String, port: u16 }`,
  `ConsentMode { Never, OncePerSession, EveryCall }`.
- `src/agt/policy/resolver.rs` (new) — `pub struct Resolver { …
  }`, `pub fn load(profile: ProfileName, tool: Option<&str>) ->
  Result<Resolver>`, `pub fn decide(&self, tool: &str, argv:
  &[String]) -> Decision`, `pub fn fingerprint(&self) -> String`
  (sha256 of the resolved policy — SPEC §4.4 step 2).
- `src/agt/mod.rs` (modified) — declare `pub mod policy;`.
- `src/agt/permission.rs:1-50` (modified) — keep the `notify-send`
  flow but route the question through `Resolver::decide` first;
  Decision::Allow short-circuits the prompt, Decision::Deny rejects
  with `ErrorCode::PolicyDenied`, ConsentRequired falls through to
  the existing notify-send.

**Tests:**
- `src/agt/policy/schema.rs::tests::strict_profile_round_trip` —
  load `configs/policy/profiles/strict.toml`; round-trip via
  serde; assert `deny_network = true`.
- `src/agt/policy/schema.rs::tests::normal_profile_round_trip` —
  load `normal.toml`; assert `exec_allowlist` has at least three
  entries (rg, cargo, git per SPEC §4.4).
- `src/agt/policy/resolver.rs::tests::strict_denies_everything` —
  decide `("rg", &["foo"])` against strict; expect
  `Decision::Deny`.
- `src/agt/policy/resolver.rs::tests::normal_allows_rg` — decide
  `("/usr/bin/rg", &["foo"])` against normal; expect
  `Decision::Allow`.
- `src/agt/policy/resolver.rs::tests::normal_consents_for_unknown_tool`
  — decide `("/usr/bin/curl", &["..."])` against normal; expect
  `Decision::ConsentRequired { reason }`.
- `src/agt/policy/resolver.rs::tests::fingerprint_changes_on_overlay`
  — load profile alone, fingerprint A; load profile + tool overlay,
  fingerprint B; assert A ≠ B (SPEC §4.4 step 2 audit).

**Definition of Done:**
- [x] Six tests pass.
- [x] Three profile TOML files exist, valid, and parse.
- [x] `Resolver::decide` returns the spec's three-way decision.
- [x] No sandbox or process-spawn behaviour change yet — `sy agentd`
      still runs as before, just consulting the resolver before
      its existing notify-send.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- TOML vs KDL (SPEC §7 Q4). Resolved: TOML, consistency with
  `configs/sy/agents.toml`. If a future need (nested blocks,
  multi-line strings) makes TOML ugly, switching is mechanical via
  `knus`.
- Glob semantics for `exec_allowlist.argv = ["test*"]` —
  use `globset` (already a workspace dep per `Cargo.toml`).
  Document the syntax in `schema.rs` head comment.

---

## Step 2 — `sy policy show` / `sy policy lint` / `sy policy explain`

**Goal:** operator + agent surface for the policy. `sy policy show`
prints the resolved profile + active overlays. `sy policy lint`
diffs `configs/policy/` against a strict baseline and flags
risky settings. `sy policy explain --tool=… --argv='…'` simulates
the decision — addresses SPEC §5 Friction Map row 4.

**Files:**
- `src/agt/policy/cli.rs` (new) — clap subcommand tree:
  ```
  sy policy show [--profile=<n>] [--tool=<n>] [--json]
  sy policy lint [--profile=<n>] [--json]
  sy policy explain --tool=<n> --argv='<args>' [--profile=<n>] [--json]
  sy policy trust --confirm
  sy policy grant --tool=<n> --scope=<p> --ttl=<dur>
  ```
- `src/agt/policy/grant.rs` (new) — `Grant { tool, scope, ttl,
  granted_at, granted_by_pid, granted_by_tty }`. Persisted to
  `$XDG_RUNTIME_DIR/sy/grants/<uuid>.toml` per SPEC §4.4 "Consent
  UX" step 2 (b). `grant trust --confirm` writes a sentinel file
  `$XDG_STATE_HOME/sy/trusted.toml` with timestamp + pid.
- `src/main.rs` (modified) — `Policy { cmd: PolicyCmd }` variant in
  the top-level clap router.

**Tests:**
- `src/agt/policy/cli.rs::tests::lint_flags_trusted_with_network_on`
  — lint a synthetic profile with `deny_network = false` AND
  `exec_allowlist = ["*"]` → expect a "fail" entry in `--json`
  output.
- `src/agt/policy/cli.rs::tests::explain_normal_rg_returns_allow`
  — `policy explain --tool=/usr/bin/rg --argv='foo'` against
  `normal.toml` → JSON returns `"decision": "Allow"`.
- `src/agt/policy/cli.rs::tests::explain_normal_curl_returns_consent`
  — same against `--tool=/usr/bin/curl` → `"ConsentRequired"`.
- `src/agt/policy/grant.rs::tests::grant_ttl_expires` — set
  `ttl=10ms`; assert `Grant::is_active(now + 20ms) == false`.
- `tests/policy_show_e2e.rs` (new) — `sy policy show --profile
  normal --json` returns a stable schema matching SPEC §4.4
  documentation.

**Definition of Done:**
- [x] Five tests pass.
- [x] `sy policy --help` lists all five subcommands with examples
      (CLIG §4.12 check).
- [x] `sy policy show --json` schema documented inline in
      `policy/cli.rs`.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- `policy trust --confirm` must require a TTY (SPEC §4.4 step 2 (b)
  + CLAUDE.md "non-interactive by default when stdin is not a TTY").
  Implementation: check `isatty(stdin)` and reject otherwise unless
  `--yes` is paired with a deliberate confirmation string passed
  via stdin.

---

## Step 3 — In-process Landlock + seccompiler + `PR_SET_NO_NEW_PRIVS` + env scrub

**Goal:** the actual sandbox enforcement. `sy-agt` forks; in child,
applies the four layers from SPEC §4.4 step 3; execs the target
binary with explicit `argv[]` (never `/bin/sh -c …` per SPEC §3.4
anti-goal).

**Files:**
- `Cargo.toml` (modified) — add `landlock.workspace = true`,
  `seccompiler.workspace = true`, `rustix.workspace = true` (the
  last for `prctl` and `openat2`) per SPEC §4.10.
- `src/agt/sandbox/mod.rs` (new) — `pub mod landlock_layer; pub mod
  seccomp_layer; pub mod env_scrub; pub mod exec;`.
- `src/agt/sandbox/landlock_layer.rs` (new) — `pub fn install(profile:
  &Profile) -> Result<()>`. Builds a `RulesetBuilder` from
  `profile.read_paths` (with `LANDLOCK_ACCESS_FS_READ_FILE |
  READ_DIR`) and `profile.write_paths` (with `WRITE_FILE | …`);
  if kernel ≥ 6.7, adds `LANDLOCK_ACCESS_NET_CONNECT_TCP` per host:port
  in `net_outbound_allowlist`; on older kernels logs a WARN and
  documents the limitation (SPEC §6 risk row 4).
- `src/agt/sandbox/seccomp_layer.rs` (new) — curated syscall
  allowlist with arg matching for `execveat`, `unlinkat`, `mount`
  per SPEC §4.4 step 3.
- `src/agt/sandbox/env_scrub.rs` (new) — `pub fn scrub(env:
  &HashMap<String, String>, allowlist: &[String]) -> HashMap<...>`.
  Keeps only allowlisted vars (`PATH`, `HOME`, `LANG`, `TERM`).
- `src/agt/sandbox/exec.rs` (new) — `pub fn fork_and_exec(profile:
  &Profile, bin: &Path, argv: &[String]) -> Result<ExitStatus>`.
  In child: `prctl(PR_SET_NO_NEW_PRIVS, 1)` → landlock install →
  seccomp install → env scrub → `execve`. In parent: `waitpid` +
  return exit status.
- `src/agt/daemon.rs` (modified) — `Decision::Allow` path now
  routes through `sandbox::exec::fork_and_exec` instead of bare
  `Command::new`.

**Tests:**
- `src/agt/sandbox/landlock_layer.rs::tests::ruleset_builds` —
  build a `RulesetBuilder` from a synthetic profile and call
  `.create()`; assert no error. (Doesn't actually `restrict_self`
  in a test process — that's destructive; coverage is the
  construction shape per SPEC §4.8 unit-test guidance.)
- `src/agt/sandbox/seccomp_layer.rs::tests::filter_constructs` —
  build the seccomp filter from a synthetic allowlist; assert
  bytes pop out the other side.
- `src/agt/sandbox/env_scrub.rs::tests::scrub_keeps_only_allowlist`
  — input map has PATH + HOME + SECRET_TOKEN; allowlist =
  ["PATH", "HOME"]; output has neither SECRET_TOKEN nor anything
  else.
- `tests/sandbox_denies_etc_shadow.rs` (new) — fork a child that
  applies a `strict`-profile landlock ruleset (no read_paths) then
  tries `cat /etc/shadow`; assert child exits with EACCES. Per
  SPEC §4.8 "Sandbox: spawn a sandboxed `cat /etc/shadow`, assert
  `policy_denied` + audit log line".
- `tests/sandbox_allows_allowlisted_tool.rs` (new) — `normal`
  profile, `rg` against a tempdir; succeeds.

**Definition of Done:**
- [x] Five tests pass; the two integration tests succeed on the
      Fedora 43 rice kernel (6.11+). Three unit tests run by default
      (`scrub_keeps_only_allowlist`, `ruleset_builds`,
      `filter_constructs`); the two integration tests
      (`sandbox_denies_etc_shadow_under_strict_profile`,
      `sandbox_allows_rg_under_normal_profile`) are gated on
      `#[ignore]` and verified via `cargo test -- --ignored` against
      kernel 7.0.6 on the rice host.
- [x] No sandbox layer silently no-ops on missing kernel features
      — Landlock TCP gate uses `CompatLevel::HardRequirement` so a
      profile with `net_outbound_allowlist` entries fails to build
      on pre-6.7 kernels (SPEC §6 risk row 4 mitigation).
- [x] `sy agentd` running with `normal` profile allows `rg` and
      denies `cat /etc/shadow`. Verified by the two integration
      tests above; the daemon-side wiring is exposed via the new
      hidden `sy agt sandbox-exec --profile <p> --bin <b> -- <argv…>`
      subcommand (Step 4's re-exec target landing one step early so
      the sandbox modules have a real binary entry point and
      `cargo clippy -D warnings` accepts them — Step 4 swaps the
      sandbox-exec invocation onto `systemd-run --user --scope`).
      ACP-child-driven tool spawns route through this entry point
      via `AcpChild::spawn`'s `build_acp_command` wrapper (lands
      with Step 4 closure). Per-agent profile is set in
      `~/.config/sy/agents.toml` `[[agent]] sandbox_profile = "normal"`
      (default; `None` opts an agent out — only the in-tree
      `cat`-placeholder daemon tests use that).
- [x] `make lint` and `make test` green workspace-wide.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- Landlock + seccomp + `PR_SET_NO_NEW_PRIVS` interact in load order
  (seccomp's `SECCOMP_FILTER_FLAG_NEW_LISTENER` needs PR_SET_NNP
  for unprivileged use). Apply in the order spec'd: PR_SET_NNP →
  landlock → seccomp → execve. Document in `exec.rs` head comment.
- `landlock` 0.4 (SPEC §4.10) ABI 4 detection: code must fall
  through gracefully if the kernel offers ABI 3 only.

---

## Step 4 — `systemd-run --user --scope` wrapper for cgroup caps

**Goal:** SPEC §4.4 step 4. Wraps the in-process sandbox in a
transient cgroup scope that provides `MemoryMax`, `CPUQuota`,
`TasksMax`, `RuntimeMaxSec`, `PrivateNetwork`, `ProtectSystem=strict`.

**Files:**
- `src/agt/sandbox/scope.rs` (new) — `pub fn run_in_scope(profile:
  &Profile, bin: &Path, argv: &[String]) -> Result<ExitStatus>`.
  Builds the `systemd-run` argv:
  `systemd-run --user --scope --collect --quiet
   -p MemoryMax={max_memory_mb}M
   -p CPUQuota={cpu_quota}
   -p TasksMax={max_pids}
   -p RuntimeMaxSec={max_runtime_seconds}
   -p ProtectSystem=strict
   -p ReadWritePaths={write_paths_joined}
   -p NoNewPrivileges=yes
   [-p PrivateNetwork=yes if deny_network]
   -- <bin> <argv...>`.
  Inside the scope, exec re-invokes `sy agt sandbox-exec`
  subcommand which loads the same profile and applies the
  in-process layers from Step 3.
- `src/agt/sandbox/mod.rs` (modified) — re-export `scope` and
  switch `daemon.rs`'s call from `exec::fork_and_exec` to
  `scope::run_in_scope`.
- `src/main.rs` (modified) — add a hidden `sy agt sandbox-exec
  --profile <profile> --tool <bin> --argv-json '...'` subcommand
  (re-exec target for the scope).

**Tests:**
- `src/agt/sandbox/scope.rs::tests::systemd_run_argv_for_normal_profile`
  — build the argv for a synthetic normal profile; assert the
  expected `-p MemoryMax=1024M`, `-p TasksMax=256`,
  `-p RuntimeMaxSec=60`, no `-p PrivateNetwork`.
- `src/agt/sandbox/scope.rs::tests::systemd_run_argv_for_strict_profile`
  — strict profile yields `-p PrivateNetwork=yes`.
- `tests/sandbox_scope_e2e.rs` (new, `#[ignore]` unless running
  with systemd available) — run a sandboxed `rg --version` via
  `sy agentd`; succeeds; `systemctl --user list-units --all` lists
  no leaked transient scope (collect=true).
- `tests/sandbox_scope_memory_cap_kills.rs` (new, `#[ignore]`) —
  run a memory-balloon binary with `MemoryMax=64M`; exit status
  reflects OOM kill.

**Definition of Done:**
- [x] Two automatic tests pass
      (`systemd_run_argv_for_normal_profile`,
      `systemd_run_argv_for_strict_profile`); two `#[ignore]`
      tests (`scope_e2e_rg_no_leak`, `scope_memory_cap_oom_kills`)
      have documented manual recipes in `scope.rs::tests` head
      comments and verified green on the Fedora 43 rice host
      against systemd 258 (kernel 7.0.6) via `cargo test --bin sy
      -- --ignored agt::sandbox::scope`.
- [x] No transient scope leaks after `sy agentd` exits — verified
      by `scope_e2e_rg_no_leak` (`systemctl --user list-units
      --type=scope` empty of `run-*.scope` after `--collect` reaps).
- [x] `make lint` and `make test` green workspace-wide (255 pass,
      6 ignored, two consecutive runs flake-clean).
- [x] `sy doctor` (Zone 6, when it lands) can report active scopes.
      Landed: `agent.sandbox.active_scopes` check in
      `src/doctor/checks.rs` shells out to `systemctl --user list-units
      --type=scope --no-legend`, counts non-blank lines, and reports
      `Pass` with `details.count = N` (or `Skip` when `systemctl` is
      absent). Three unit tests cover the parser (empty list, populated
      list) and the missing-`systemctl` Skip path. No prefix filter yet
      — `sy agentd` doesn't stamp a unique scope name; revisit when
      sandbox scopes adopt a `sy-sandbox-<ulid>.scope` naming.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- `systemd-run --user --scope` requires a user manager session
  (`XDG_RUNTIME_DIR` plus an active `dbus-user-session`). On
  Fedora 43 this is the default; non-default environments need a
  fallback. v1: log a clear error and refuse to run; Zone 6 `sy
  doctor` flags the missing prerequisite.
- `RuntimeMaxSec` SIGKILLs the unit (not the agentd parent).
  Confirm via the second `#[ignore]` test.
- **Resolved deviation from SPEC §4.4 step 4 listing**: `--user
  --scope` on Fedora 43 + systemd 258 rejects every non-cgroup
  directive (`NoNewPrivileges`, `PrivateNetwork`,
  `ProtectSystem=strict`, `ReadWritePaths`). The equivalent
  enforcement is supplied by the in-process Step 3 layers
  (`PR_SET_NO_NEW_PRIVS`, Landlock ABI v4 TCP gate, Landlock
  read/write path allowlist). Documented in `scope.rs` module
  head; revisit if/when we migrate to `--service-type=exec`.
- `CPUQuota` omitted — `Profile` has no `max_cpu_pct` knob yet.
  Add a field + emit `-p CPUQuota=…` when the need surfaces.

---

## Step 5 — Dual-sink audit log (journald + JSONL with rotation)

**Goal:** SPEC §4.4 "Audit log" lands. Every sandbox decision —
allow, deny, consent — emits a structured journald entry AND a
JSONL line. JSONL rotates at 64 MiB with zstd compression.

**Files:**
- `Cargo.toml` (modified) — add `libsystemd.workspace = true` (or
  `systemd-journal-logger`), `zstd.workspace = true`.
- `src/agt/audit.rs` (new) — `pub struct AuditRecord { ts: DateTime,
  tool: String, policy_sha: String, decision: AuditDecision, argv:
  Vec<String>, request_id: Option<Ulid>, trace_id: Option<String>,
  reason: Option<String> }`. `pub fn emit(record: AuditRecord)`
  fires-and-forgets to both sinks.
- `src/agt/audit/journald.rs` (new) — `pub fn emit_journald(record:
  &AuditRecord) -> Result<()>`. Uses `libsystemd::journal::send`
  with structured fields per SPEC §4.4: `SY_TOOL`, `SY_POLICY_SHA`,
  `SY_DECISION`, `SY_ARGV`, `SY_REQUEST_ID`, `SY_TRACE_ID`,
  `MESSAGE_ID`.
- `src/agt/audit/jsonl.rs` (new) — append-only writer to
  `$XDG_STATE_HOME/sy/audit.jsonl`. `pub fn emit_jsonl(record:
  &AuditRecord) -> Result<()>`. Rotation: on each append, check
  file size; if > 64 MiB, rename to `audit.jsonl.1`, zstd-compress
  to `audit.jsonl.1.zst`, delete the renamed file. Older
  `.{n}.zst` shift up to `.{n+1}.zst`; keep last 10.
- `src/agt/sandbox/exec.rs` (modified) — emit an audit record
  before fork and after exec, with the decision and exit status.
- `src/agt/permission.rs` (modified) — `Decision::Deny` and
  `Decision::Allow` both audited; consent-required gets a separate
  `AuditDecision::Consent` variant.

**Tests:**
- `src/agt/audit/jsonl.rs::tests::jsonl_rotation_at_64mib` — write
  a 70 MiB synthetic stream; assert `audit.jsonl` shrinks and
  `audit.jsonl.1.zst` exists.
- `src/agt/audit/jsonl.rs::tests::jsonl_keeps_last_10_archives` —
  rotate 12 times; assert exactly 10 `.zst` files survive.
- `src/agt/audit/journald.rs::tests::journald_emit_does_not_panic_when_missing`
  — on a host without systemd, `emit_journald` returns
  `Err(NotAvailable)` rather than panicking. Defence in depth per
  SPEC §2.3 deep dive on `tracing-journald` silent-drop bug.
- `tests/sandbox_audit_dual_sink.rs` (new) — strict profile,
  deny `cat /etc/shadow`; assert (a) JSONL line written, (b)
  journald has a matching record discoverable via
  `journalctl --user SY_DECISION=deny -o json`.

**Definition of Done:**
- [x] Four tests pass. Six audit unit tests run by default
      (`audit_decision_round_trips_lowercase`,
      `record_now_populates_ts_and_carries_argv`,
      `journald_emit_does_not_panic_when_missing`,
      `jsonl_rotation_at_64mib`, `jsonl_keeps_last_10_archives`,
      `emit_appends_one_json_line`). The dual-sink e2e
      (`dual_sink_emits_both_records`) is `#[ignore]`-gated and was
      verified green on the Fedora 43 rice host via `cargo test
      --bin sy -- --ignored agt::audit::tests::dual_sink_emits_both_records`.
- [x] Audit log shows up under both sinks; rotation works.
      `emit_jsonl` rotates at 64 MiB into `audit.jsonl.1.zst`,
      shifts existing archives up, evicts `.{11}.zst` at the cap.
      The journald sink fires structured `SY_*` fields through
      `libsystemd::logging::journal_send` and is graceful when the
      socket is absent (`Err(AuditSinkError::NotAvailable)`).
- [x] No structured field renames vs SPEC §4.4 — fields emitted:
      `SY_TOOL`, `SY_POLICY_SHA`, `SY_DECISION`, `SY_ARGV`,
      `SY_REQUEST_ID`, `SY_TRACE_ID`, `SY_REASON`, `SY_TS`,
      `MESSAGE_ID`. Lowercase `SY_DECISION` per the SPEC text.
- [x] `make lint` and `make test` green workspace-wide (261 pass,
      6 ignored, two consecutive runs flake-clean).
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- zstd dep weight. SPEC §4.10 doesn't pre-list it; this step adds
  it. If we'd rather avoid the dep, switch to `flate2` (already
  transitive via `image`). Resolved: `zstd = "0.13"` adds ~one
  second of compile time and a small `libzstd` link; the SPEC
  text explicitly names "zstd compression" for the archives, so
  matching the spec letter is the better trade.
- `request_id` and `trace_id` thread-through: `trace_id` is lifted
  from `sy_core::obs::current_trace_ctx()` at the audit call sites
  (permission.rs + sandbox_exec). `request_id` is now backfilled per
  the 2026-05-17 follow-up march pass — the IPC v1 envelope's
  `request_id` rides on `Session::originating_request_id` through
  `handle_permission` / `resolve_permission` / `ask_with_policy` /
  `emit_policy_audit`, on the `agt.approve` envelope through
  `handle_approve` / `emit_approve_audit`, and on the
  `SY_AGT_REQUEST_ID` env var into `sandbox-exec`'s pre/post-fork
  audit records.

---

## Step 6 — `sy approve <token>` + `sy policy grant` TTY consent UX

**Goal:** SPEC §4.4 "Consent UX" full flow. Strict profile returns
`ErrorCode::ConsentRequired { token, policy_diff, expires_at }`;
user approves via `sy approve <token>` on a TTY (or via a pre-issued
`sy policy grant`). No auto-approval; `notify-send` action button
remains for `normal`-profile consent (Zone 4.3 deferred).

**Files:**
- `src/agt/policy/consent.rs` (new) — `pub struct PendingConsent {
  token: Uuid, tool: String, argv: Vec<String>, policy_diff: String,
  expires_at: Instant, decided: oneshot::Sender<Decision> }`.
  `ConsentStore` keeps a `Mutex<HashMap<Uuid, PendingConsent>>` in
  the daemon.
- `src/agt/policy/cli.rs` (modified) — `sy approve <token>` and
  the existing `sy policy grant` from Step 2 connect to the
  daemon via IPC v1 (`agt.approve`, `agt.grant`).
- `src/agt/daemon.rs` (modified) — gains `agt.approve` and
  `agt.grant` IPC methods. The agt session's tool-call path checks
  the `ConsentStore` before falling through to `notify-send`.
- `src/agt/permission.rs:1-50` (modified) — `notify-send` path
  remains as the fallback for `normal` profile + `OncePerSession`
  consent mode. `EveryCall` (strict default) requires
  `sy approve <token>` from a TTY; `notify-send` is bypassed for
  EveryCall.

**Tests:**
- `src/agt/policy/consent.rs::tests::token_expires` — set
  `expires_at = Instant::now() + 1ms`; sleep 10ms; assert
  `decide_pending(token) == Err(Expired)`.
- `src/agt/policy/consent.rs::tests::two_simultaneous_consents_do_not_collide`
  — issue tokens A and B; approve B; A still pending.
- `tests/consent_e2e.rs` (new) — strict profile, tool call;
  daemon emits `ConsentRequired { token }` over IPC; another
  process sends `sy approve <token>`; original call proceeds and
  returns success.
- `tests/consent_refuses_outside_tty.rs` (new) — invoke
  `sy approve <token>` with stdin redirected from `/dev/null`;
  expect exit 2 (usage error) and stderr message about TTY
  requirement. SPEC §4.12 CLIG check.

**Definition of Done:**
- [x] Four tests pass. Implemented as seven `policy::consent::tests`
      (`token_expires`, `two_simultaneous_consents_do_not_collide`,
      `e2e_strict_profile_issues_and_resumes`,
      `cleanup_drops_expired_entries`,
      `snapshot_returns_pending_metadata`,
      `unknown_token_returns_not_found`) plus
      `policy::cli::tests::approve_refuses_outside_tty` covering the
      TTY pre-flight (bare non-TTY, `--token-from-stdin` alone, `--yes`
      alone, override allowed, TTY accept, empty-token reject).
- [x] `sy approve --help` documents the TTY requirement and the
      `--token-from-stdin --yes` override (SPEC §4.12). Verified via
      `cargo run --bin sy -- approve --help`.
- [x] No LLM-inferred auto-approval anywhere (SPEC §3.4 anti-goal).
      `grep -rn "auto.approv\|auto_approve\|llm.*approv" src/`
      returns nothing; `ConsentDecision` has only `Allow` and the
      sole way to flip a token is the TTY-driven `sy approve`.
- [x] Audit log records consent decisions with both the original
      tool call's `request_id` and the operator's pid/uid.
      `daemon::handle_approve` reads `SO_PEERCRED` from the approving
      IPC connection and stamps `approver pid=… uid=… ttl_remaining_ms=…`
      onto `AuditRecord::reason`; `AuditRecord::request_id` carries
      the approving IPC envelope's `request_id` via
      `emit_approve_audit` per the 2026-05-17 follow-up pass — the
      permission path threads the session's originating envelope id
      through `emit_policy_audit` symmetrically.
- [x] `make lint` and `make test` green workspace-wide (268 pass,
      7 ignored, two consecutive runs flake-clean).
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- SPEC §6 risk row 9 "consent UX is too friction-heavy and users
  default to `trusted`". Mitigation: `normal` profile pre-allows
  rg/cargo/git so the *typical* tool call doesn't trigger consent.
  Reserved `trusted` requires `sy policy trust --confirm`.

---

## Cross-cutting Definition of Done

- [x] All step DoDs satisfied.
- [x] Fresh checkout end-to-end:
  1. `sy agentd run --agent claude-code --cwd ~/sources/sy --
      "rg foo"` works with `normal` profile. (Verified manually
      via `sy agt sandbox-run --profile normal --bin /usr/bin/rg --
      --version`; the daemon-driven path goes through the same
      `resolve_permission → Decision::Allow` route now that
      Step 6 is in place.)
  2. `sy agentd run …` with `cat /etc/shadow` is denied;
      audit log shows the denial in both journald and JSONL.
      Auto-covered by Step 3's `sandbox_denies_etc_shadow_under_strict_profile`
      and Step 5's `dual_sink_emits_both_records`.
  3. Strict profile asks for consent; `sy approve <token>` on a
      separate TTY proceeds the call. Auto-covered by Step 6's
      `e2e_strict_profile_issues_and_resumes` (in-process surrogate
      for the cross-process e2e — same `ConsentStore::issue`/`decide`
      contract the daemon's `agt.approve` handler hits).
- [x] No `sh -c <string>` execution path anywhere in
      `src/agt/sandbox/` (SPEC §3.4 anti-goal: "exec is
      `(binary, argv[])`"). Verified `grep -rn '"sh", "-c"\|"sh","-c"\|Command::new("sh")' src/agt/sandbox/`
      returns no hits.
- [x] `firejail` is not a build-time dep (SPEC §3.4 anti-goal).
      Verified `grep -rn firejail Cargo.toml crates/*/Cargo.toml`
      returns nothing.
- [x] `bwrap` is not a hard runtime dep (SPEC §3.3 Zone 4 "OUT":
      bwrap second layer deferred). Verified `grep -rn bwrap src/`
      returns nothing.
- [x] `make test` and `make lint` green workspace-wide (268 pass,
      7 ignored, two consecutive runs flake-clean).

## Out of Scope

- `bwrap` second layer for strict profile (Zone 4.2 — separate
  follow-on once the in-process layer stabilises).
- `xdg-desktop-portal` Notification action-button consent UX
  (Zone 4.3 — mako already handles action buttons; this is purely
  a friction-reducer for the `notify-send`-driven flow once it
  stabilises).
- Per-tool sandbox profile authoring tools beyond `policy lint` —
  YAML/TOML editors etc.
- MCP-specific consent flow nuances beyond the
  `ErrorCode::ConsentRequired` wire shape — MCP host responsibility
  per SPEC §2.2 "MCP host responsibility".
- Network policy beyond `LANDLOCK_ACCESS_NET_CONNECT_TCP`'s
  per-host:port gate — no DNS or eBPF policy.
- Multi-tenant / multi-user sandbox isolation — SPEC §3.4 anti-
  goal "single-host single-user".
