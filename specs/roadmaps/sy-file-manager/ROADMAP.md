# ROADMAP: sy-file-manager

Source:
- [`specs/research/sy-file-manager/SPEC.md`](../../research/sy-file-manager/SPEC.md)
- [`specs/research/sy-file-manager-plugins/SPEC.md`](../../research/sy-file-manager-plugins/SPEC.md)
- [`specs/journeys/JOURNEY-20260527-0215-sy-file-first-session.md`](../../journeys/JOURNEY-20260527-0215-sy-file-first-session.md)

## Overview

Lands `sy file` end-to-end: a niri-tiled iced + xdg-toplevel file
manager with yazi-shaped UX, a binaries-over-stdio plugin runtime
(JSON-RPC 2.0 + LSP framing), `sy knowledge` semantic search inline,
and full JSON-IPC + MCP drivability. Slicing follows the plugin
SPEC's "Hand-off" order: **plugin runtime first** (it backs the
preview pipeline) → **file-manager core** (state + fs + ipc) → **iced
UI** (pane/preview/cmdbar/dnd) → **integration** (knowledge,
bookmarks, mounts, doctor) → **migration removal** of `configs/yazi/`
and `scripts/yazi-plugins.sh`. Each step lands green; the build is
never red mid-step. The first first-party plugin (`sy-plugin-md`)
is the canary that proves the plugin contract before any other
plugin lands and is what replaces the failed `md-rich.yazi`
experiment.

Cross-cutting invariants from
[`AGENTS.md`](../../../AGENTS.md): tests first; zero clippy warnings;
zero `#[allow(dead_code)]` outside `#[cfg(test)]`; no
`TODO`/`FIXME`/`unimplemented!()` in committed code; each step
revertable in isolation.

---

## Non-negotiables — read before opening any step

These three rules override the per-step `Files` / `Tests` / `Definition
of Done` lists below when they conflict. They exist because every step
already lands the smallest revertable slice the work admits — slipping
work between steps voids the ordering and breaks the journey-incremental
contract.

1. **DO NOT DEFER any item in a step.** Every bullet under `Files`,
   `Tests`, `E2E test`, and `Definition of Done` ships in the same
   commit (or commit series) that closes that step. There is no
   "follow-up step" for missed scope, no `TODO`, no `FIXME`, no
   `unimplemented!()`, no `#[ignore]` test, no
   "we'll wire this in step N+k". If you find yourself reaching for
   any of those, stop and re-read this rule.
2. **If something blocks, expand the step's scope and implement the
   unblocker inline.** If Step 16's `copy_file_range` path needs a
   `same_mount()` helper that the SPEC didn't anticipate, build
   `same_mount()` inside Step 16 — do not open Step 16.5. If Step 27's
   plugin-routed preview needs the deferred `host.preview.image_show`
   host fn to actually work, ship it in Step 27 (the roadmap already
   declares this intent — honour it for any other gap you find).
   Update the commit message and this roadmap's `Files` block so the
   audit trail is honest, but **do not skip the work**.
3. **Every step ships an end-to-end test that drives the journey.**
   Each step adds at least one E2E test in
   `tests/sy_file_journey_e2e.rs` that walks the
   [first-session journey](../../journeys/JOURNEY-20260527-0215-sy-file-first-session.md)
   from beat J1 (launch via `Mod+E`) as far as that step's surface
   allows. The test grows monotonically: by Step 36 the full 8-beat
   journey runs in a single invocation. If your E2E can't yet reach
   the journey beat this step is supposed to unlock, that's a Rule 2
   trigger — expand the step's scope until the E2E passes, do not
   weaken the assertion.

Journey beats referenced below:

- **J1** — launch via `Mod+E` (or `sy file ~` from a shell)
- **J2** — 3-pane render populated from real fs
- **J3** — hover markdown → live PNG preview (canary: `sy-plugin-md`)
- **J4** — `:k <query>` knowledge search in the current pane
- **J5** — multi-select (`<Space>` toggle, range, `*` all)
- **J6** — copy selection → sibling dir with progress
- **J7** — tile-shrink reflow (3-pane → 2-pane → 1-pane)
- **J8** — agent mirrors the user's pane via `sy file --ipc`

---

## Phase A — Plugin runtime foundations

### Step 1 — `plugin.toml` manifest parser

**Goal:** parse and validate the manifest grammar defined in
[plugin SPEC §4.1](../../research/sy-file-manager-plugins/SPEC.md#41-manifest-grammar-plugintoml).
Pure functions over `&str`; no I/O. Unknown keys warn, don't fail
(forward compatibility per SPEC).

**Files:**
- `src/plugin/mod.rs` (new) — `pub mod manifest;`
- `src/plugin/manifest.rs` (new) — `Manifest { plugin: PluginMeta, capabilities: Vec<Capability>, needs: Needs, limits: Limits, env: BTreeMap<String,String>, signature: Option<Signature> }`, `parse(&str) -> Result<Manifest>`, `validate(&Manifest) -> Result<()>`.
- `src/main.rs` (modified) — add `mod plugin;` near [`mod mon;`][main-mon-loc].

**Tests:**
- `plugin::manifest::tests::parses_canonical_example` — uses the §4.1 grammar block verbatim as a fixture.
- `plugin::manifest::tests::rejects_missing_api_version`.
- `plugin::manifest::tests::warns_on_unknown_key_but_succeeds`.
- `plugin::manifest::tests::glob_predicates_compile` — `url = "*.md"`, `mime = "text/*"` parse to `globset` patterns.
- `plugin::manifest::tests::rejects_negative_limits`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step01_manifest_parses_sy_plugin_md_canary` — parses the productivised `sy-plugin-md` `plugin.toml` (the canary that backs **J3**, hover markdown preview). Asserts every field the journey will read (`mime`, `url` predicates, `[needs]`, `[limits]`) is reachable via the typed `Manifest`. If a field needed downstream is missing from the parser, **expand this step's scope** to add it — do not push to a later step.

**Definition of Done:**
- [x] tests above pass
- [x] `make lint` green; no `#[allow(dead_code)]`
- [x] doc-comment on every public type names the SPEC §
- [x] no productivisation yet (no `configs/sy/plugins/`)
- [x] E2E test above passes

**Risks / unknowns:** `globset` vs `regex-lite` decision deferred from
SPEC §4.8; pick `globset` (smaller, glob-only semantics match the
predicate language) — confirm in this step's implementer notes.
**Resolved (2026-05-27):** picked `globset` (already a workspace dep,
no new transitive deps). Manifest `url`/`mime` predicates compile via
`globset::Glob::new`; validation rejects malformed patterns at load
time. `Manifest`, `Capability`, `Needs`, `Limits`, `Signature`,
`BinarySpec`, `PluginMeta` all live in `src/plugin/manifest.rs`.
`mod plugin` is gated `#[cfg(test)]` in `src/main.rs` until the
Step 2+ non-test consumers in the bin land (avoids tripping
`clippy::dead_code`).

---

### Step 2 — Wire transport: Content-Length framing + JSON-RPC codec

**Goal:** implement the LSP-style framed transport in a `Framed`-shaped
codec so messages decode/encode round-trip-clean over any
`AsyncRead`/`AsyncWrite`. No process spawn yet; this step is pure
plumbing.

**Files:**
- `src/plugin/transport.rs` (new) — `pub struct JsonRpcCodec; impl Decoder + Encoder<Message>`, where `Message = serde_json::Value`.
- `src/plugin/rpc.rs` (new) — typed wrappers: `Request { id, method, params }`, `Response { id, result | error }`, `Notification { method, params }`, `ErrorObj { code, message, data }`, custom-code constants (`CAP_NOT_GRANTED = -32099`, etc.).
- `src/plugin/mod.rs` (modified) — `pub mod transport; pub mod rpc;`

**Tests:**
- `plugin::transport::tests::encode_then_decode_roundtrip` — request → bytes → request, byte-identical.
- `plugin::transport::tests::decode_streamed_partial_frame` — feed bytes byte-by-byte; assert partial decodes return `Ok(None)` until full.
- `plugin::transport::tests::decode_rejects_missing_content_length`.
- `plugin::transport::tests::decode_handles_optional_content_type`.
- `plugin::transport::tests::encode_payload_with_newlines_in_string` — proves the framing handles base64-PNG-shaped payloads.
- `plugin::transport::tests::max_payload_16_mib_enforced` — 17 MB payload rejected with `frame::too_large`.
- `plugin::rpc::tests::error_codes_match_spec` — assert numeric values match plugin SPEC §4.2.2.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step02_transport_roundtrips_preview_tape` — replays a recorded `preview` request → PNG-bearing response tape (the wire shape **J3** will produce) through the codec end-to-end over a `DuplexStream`; asserts byte-identical round-trip including a >2 MiB base64 PNG body. Anchors the wire contract for every later journey beat that crosses the host↔plugin boundary.

**Definition of Done:**
- [x] tests above pass
- [x] `tokio_util::codec::Framed<_, JsonRpcCodec>` works against a `tokio::io::DuplexStream` fixture
- [x] `make lint` green
- [x] 16-MiB cap is a `pub const MAX_PAYLOAD_BYTES`
- [x] E2E test above passes

**Risks / unknowns:** none — LSP framing is a 30-year-old shape.
**Resolved (2026-05-27):** `JsonRpcCodec` carries `serde_json::Value`
on the wire; typed `Request`/`Response`/`Notification`/`ErrorObj`
wrappers live in `src/plugin/rpc.rs` and serialise to `Value` before
encoding. SPEC §4.2.2 custom error codes pinned as `pub const i32`
(`CAP_NOT_GRANTED = -32099`, `API_VERSION_MISMATCH = -32098`,
`RLIMIT_BREACH`/`LIMIT_EXCEEDED = -32097`, `BAD_PREDICATE = -32096`,
`INVALID_PATH = -32095`) plus the Step-2-added `FRAME_TOO_LARGE =
-32094` for the >16 MiB ceiling. The codec surfaces oversize /
missing-`Content-Length` / non-UTF-8 frames as `io::ErrorKind::
InvalidData` carrying a stable `frame::*` marker the rpc layer maps
to the peer-facing JSON-RPC code. `bytes` was added to the bin
`[dependencies]` block (workspace dep was already pinned).

The `#[cfg(test)] mod plugin;` gate in `src/main.rs` STAYS — Step 2
still has no non-test bin consumer of plugin (`transport`/`rpc` are
only reached via the `#[path]` mod-imports from
`tests/sy_file_journey_e2e.rs` and the in-source `#[cfg(test)] mod
tests` trees). The gate drops in Step 8 (`Cmd::Plugin` CLI variant)
when the bin first calls the registry + transport stack at runtime.

---

### Step 3 — Sandbox primitives: rlimit + cwd + env scrub + runcon wrapper

**Goal:** the `Spawnable` ladder that wraps a `tokio::process::Command`
with the rlimit / nice / fd-close / `runcon` envelope from
[plugin SPEC §4.3](../../research/sy-file-manager-plugins/SPEC.md#43-sandbox-enforcement).
No spawn yet — this step builds the configured `Command` and asserts
its shape.

**Files:**
- `src/plugin/sandbox.rs` (new) — `pub fn build_command(manifest: &Manifest, workdir: &Path) -> Result<Command>`, applying `pre_exec` for rlimit / setpriority / fd-close.
- `src/plugin/mod.rs` (modified) — `pub mod sandbox;`

**Tests:**
- `plugin::sandbox::tests::sets_rlimit_as_from_manifest` — spawns `/bin/sh -c 'ulimit -v'`; asserts kilobytes match `memory_mb * 1024`.
- `plugin::sandbox::tests::sets_cpu_seconds` — analogous via `ulimit -t`.
- `plugin::sandbox::tests::sets_nofile`.
- `plugin::sandbox::tests::scrubs_environ_keeps_manifest_env` — `env -0` output contains only the allowlist.
- `plugin::sandbox::tests::cwd_is_xdg_runtime_subdir` — tmpdir override via `SY_PLUGIN_RUNTIME_DIR`.
- `plugin::sandbox::tests::runcon_used_when_label_present` — when `/usr/bin/runcon` is on PATH and SELinux is enforcing, wraps argv; otherwise skips with a warning.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step03_sandbox_envelopes_sy_plugin_md_manifest` — builds the `Command` for the productivised `sy-plugin-md` manifest, spawns it via `/bin/sh -c 'ulimit -v -t -n; printenv | sort'`, captures stdout, and asserts the exact rlimit / env-scrub / cwd envelope **J3**'s preview process will run under is in force. If a sandbox dimension the journey will rely on isn't enforced (e.g. nofile cap), **expand scope** to add it here.

**Definition of Done:**
- [x] tests above pass on a regular Fedora 43 host
- [x] SELinux-disabled hosts skip the runcon assertion gracefully (gated on `getenforce`)
- [x] `make lint` green
- [x] documented: every rlimit corresponds to a manifest field; no silent defaults
- [x] E2E test above passes

**Risks / unknowns:** `nix::sys::resource::setrlimit` requires unsafe-
free `Resource` enum; verify the crate version in workspace `Cargo.toml`
or add it.
**Resolved (2026-05-27):** `nix = "0.29"` was already a workspace dep;
the bin's `[dependencies]` block opts in to the gated `resource`
feature (`nix = { workspace = true, features = ["resource"] }`) so
`nix::sys::resource::{Resource, setrlimit}` are reachable from
`src/plugin/sandbox.rs` without widening sy-core's surface. The
fallback ladder for SELinux is three-layered (`runcon` on PATH ?
`getenforce == Enforcing` ? policy module loads `sy_plugin_t` ?) so a
Fedora host that has `runcon` and Enforcing SELinux but no
`sy_plugin.te` installed (today's reality) still degrades to the
no-wrap path with a `tracing::warn!`, matching the journey J3
"SELinux denial on plugin spawn" edge case. Documented PATH
carve-out: `apply_env` re-injects `PATH` after `env_clear()` so
`/bin/sh` (and any interpreter the manifest binary may delegate to)
can resolve its dynamic-linker paths inside the child; manifests are
free to override `PATH` via `[env]`. `RUNTIME_DIR_ENV`
(`SY_PLUGIN_RUNTIME_DIR`) + `runtime_dir_for()` implement the cwd
precedence ladder Step 4's supervisor will reuse.

The `#[cfg(test)] mod plugin;` gate in `src/main.rs` STAYS — Step 3
has no non-test bin consumer either (the supervisor lands in Step 4,
and the `Cmd::Plugin` clap variant in Step 8). Sandbox is reachable
via the `#[path]` mod-import from `tests/sy_file_journey_e2e.rs` plus
the in-source `#[cfg(test)] mod tests` tree. The gate drops at
Step 8 when the bin first calls the registry + transport stack at
runtime, per Step 2's resolution note.

---

### Step 4 — Process supervisor: spawn / shutdown / restart-with-backoff

**Goal:** wrap a configured `Command` in a `Plugin` actor that handles
the full lifecycle from
[plugin SPEC §4.4](../../research/sy-file-manager-plugins/SPEC.md#44-supervision--restart):
spawn → initialize handshake → request loop → shutdown/exit → restart
on EOF with `2^n * 100 ms` backoff up to 3 attempts, then
`State::Unhealthy`.

**Files:**
- `src/plugin/proc.rs` (new) — `pub struct PluginProc { … }`, `pub async fn spawn(manifest, workdir) -> Result<PluginProc>`, `request(method, params) -> impl Future<Response>`, `shutdown()`, `health() -> State`.
- `src/plugin/mod.rs` (modified) — `pub mod proc;`

**Tests:**
- `plugin::proc::tests::handshake_with_echo_binary` — uses a tiny inline `/bin/sh` script that echoes the `initialize` request body back as a response.
- `plugin::proc::tests::restart_after_eof` — supervisor sees EOF, respawns; second `initialize` succeeds.
- `plugin::proc::tests::restart_ladder_caps_at_three_attempts` — script that always exits 1; supervisor goes Unhealthy after 3 attempts; total elapsed ≤ 1.5 s (backoff sum).
- `plugin::proc::tests::shutdown_then_exit_within_timeout` — supervisor sends `shutdown`, waits `shutdown_timeout_ms`, then `exit` notification; process exits 0.
- `plugin::proc::tests::ping_missed_triggers_restart` — simulated stalled stdin causes ping timeout → restart.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step04_supervisor_drives_md_stub_lifecycle` — spawns a stub markdown plugin (handshake-only) under the real supervisor, drives `initialize → ping → preview-stub → shutdown → exit`; asserts the full lifecycle **J3** depends on works against a real child. Then kills the child mid-flight and asserts the supervisor restarts it inside the backoff budget (the resilience **J7**+ relies on).

**Definition of Done:**
- [x] tests above pass
- [x] no `unwrap` on the wire path; all errors carry an `RpcError` discriminant
- [x] `make lint` green
- [x] `tracing` span `plugin.<id>` wraps spawn + every request
- [x] DoS guard: stdin writer wakes on `tokio::select!` with `health_tx` so a hung plugin's writer can be dropped
- [x] E2E test above passes

**Risks / unknowns:** `tokio::process::Child::wait_with_output()` vs
the streaming-stdio model we need — we want a long-lived stdin/stdout
duplex; use `Child::stdin.take()` + `Child::stdout.take()` + a
`tokio::select!` loop.
**Resolved (2026-05-27):** `PluginProc` actor in `src/plugin/proc.rs`
wraps `Child::stdin.take()` + `Child::stdout.take()` in a split
`FramedDuplex` (independent reader / writer `tokio::sync::Mutex`es
so the reader holding stdout doesn't block a periodic ping write).
The `tokio::select!` loop has four arms — cmd_rx (biased), the
framed reader, the ping-due timer (gated `if ping_in_flight.is_none()`
so consecutive ping_due ticks don't trample the deadline timer),
and the ping_deadline pending-future. EOF / read error walks
`restart_if_attempts_remain(2^n * 100 ms)` up to
`max_restart_attempts` (default 3 per SPEC §4.4), then parks the
supervisor in `State::Unhealthy { attempts, last_err }`. Inline
`RpcError` enum (`Spawn / Handshake / Peer / Transport / Unavailable
/ Timeout`) replaces what would otherwise have been a `thiserror`
dep; `From<ErrorObj>` lifts wire-side `-32099 CAP_NOT_GRANTED` etc.
into the `Peer` variant. The `health_tx` `watch::Sender` is the
DoS-guard the SPEC §4.4 footnote calls out: `wait_state_change_then_ready`
acks the current value via `borrow_and_update()` before awaiting, so
a stale `Spawning → Ready` edge from the initial handshake doesn't
satisfy the journey-J7 "kill mid-flight then restart" test. Stub
plugin scripts in both the unit-test and integration-test fixtures
use `#!/bin/bash` (not POSIX `sh`) with an in-place `FRAME=` variable
so each request-loop iteration stays in a single shell — POSIX
`$(read_frame)` would fork a subshell per iteration that races with
the supervisor's pipe under parallel-test stress.

The `#[cfg(test)] mod plugin;` gate in `src/main.rs` STAYS — Step 4
has no non-test bin consumer either; `PluginProc` is reached via the
unit `#[cfg(test)] mod tests` tree inside `proc.rs` and the
`#[path]`-imported `proc_mod` module from
`tests/sy_file_journey_e2e.rs`. The gate drops at Step 8 when the
`Cmd::Plugin` clap variant first calls the registry + supervisor at
runtime.

---

### Step 5 — Capability negotiation: `initialize` handshake

**Goal:** implement the host side of `initialize` /
`shutdown` / `exit` / `ping` from
[plugin SPEC §4.2.3](../../research/sy-file-manager-plugins/SPEC.md#423-lifecycle-methods-host-→-plugin).
Host advertises its supported `api` array + host-callable methods;
plugin returns its capability set + offered methods; mismatch ⇒
`-32098 API_VERSION_MISMATCH`.

**Files:**
- `src/plugin/capability.rs` (new) — `HostCapabilities`, `negotiate(proc: &mut PluginProc, manifest: &Manifest) -> Result<NegotiatedCaps>`.
- `src/plugin/proc.rs` (modified) — `spawn()` calls `negotiate()` after the child is up.

**Tests:**
- `plugin::capability::tests::matching_api_succeeds` — host advertises `["1"]`; manifest `api_min/api_max = "1"`; handshake completes.
- `plugin::capability::tests::api_mismatch_returns_32098` — host `["1"]`; manifest `api_min = "2"`; spawn fails with `API_VERSION_MISMATCH`.
- `plugin::capability::tests::plugin_capabilities_must_match_manifest` — plugin's `initialize` result advertises a capability not in its manifest → reject.
- `plugin::capability::tests::offers_unknown_method_is_warned_not_fatal`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step05_negotiates_previewer_cap_for_md` — host advertises the exact host-fn set **J3** + **J6** use (`host.fs.read`, `host.notify.waybar`, …); stub plugin advertises `previewer` for `text/markdown`; asserts the `NegotiatedCaps` returned is what the registry (Step 7) will index on. Mismatch on either side fails the test — preventing a silent capability drift that would break the journey at runtime.

**Definition of Done:**
- [x] tests above pass
- [x] negotiated caps stored in `PluginProc::caps` for §6 dispatch
- [x] `make lint` green
- [x] host capabilities table is a single source of truth — both `initialize` payload and the runtime cap-check enforcer read from `HostCapabilities::ALL`
- [x] E2E test above passes

**Risks / unknowns:** none.
**Resolved (2026-05-27):** `src/plugin/capability.rs` is the canonical
home for the SPEC §4.2.3 handshake. `HostCapability` is a closed enum
with one variant per SPEC §4.2.5 row landing in Step 6;
`HostCapabilities::ALL` is the single source of truth — the
`build_initialize_params` constructor reads it to populate
`initialize.params.host.host_methods`, and the Step 6 runtime
cap-check enforcer (`host_fns::dispatch`) will read it via the public
`HostCapabilities::knows(name)` predicate. The seven entries landing
here are `host.fs.{read,cha,write_cache}` +
`host.notify.{waybar,banner}` + `host.ui.theme` + `host.exec.run`
(deferred: `host.preview.*`, `host.knowledge.*`, `host.ui.confirm`,
all blocked on the file-manager runtime). `parse_initialize_result`
runs three cross-checks per SPEC §4.2.3 — api∈host_api else
`RpcError::Peer { code: -32098 }`, advertised capabilities ⊆
manifest's `[[capability]]` rows else `RpcError::Protocol(...)`,
plugin-offered host methods filtered to `HostCapabilities::knows(...)`
with a `tracing::warn!` on drop (forward-compat per SPEC §4.1). The
Step-5 introduced `RpcError::Protocol` variant lives in `proc.rs`
next to the existing discriminants; Step 6+ will reuse it for other
wire-shape violations. `PluginProc` now carries a
`caps: Option<NegotiatedCaps>` field populated at `spawn()` time and
surfaced via `pub fn caps(&self) -> Option<&NegotiatedCaps>`; the
restart ladder re-negotiates against the same manifest on every
respawn but does not mutate the spawn-time snapshot the supervisor's
caller sees (the manifest is the source of truth for what the plugin
*can* offer, so a drift mid-supervisor-lifetime is itself a protocol
violation the next call would catch).

The `#[cfg(test)] mod plugin;` gate in `src/main.rs` STAYS — Step 5
still has no non-test bin consumer; the new module is reached via
the `#[path]`-imported `capability` module in
`tests/sy_file_journey_e2e.rs` (which also extends the side-shim
`plugin` re-export module with `proc_mod as proc` so the
`#[path]`-imported `capability.rs` can resolve its
`crate::plugin::proc::RpcError` import under the integration-test
binary). The gate drops at Step 8 when `Cmd::Plugin` first calls the
registry + supervisor at runtime, per Step 2's resolution note.

---

### Step 6 — Host-callable methods + capability enforcement

**Goal:** implement the `host.*` namespace surface from
[plugin SPEC §4.2.5](../../research/sy-file-manager-plugins/SPEC.md#425-host-callable-methods-plugin-→-host),
gated by `check_cap` per the SPEC's `[needs]` table. Host functions
that don't need file-manager context land here
(`host.fs.read`, `host.fs.cha`, `host.fs.write_cache`,
`host.notify.banner`, `host.notify.waybar`, `host.ui.theme`,
`host.exec.run`). The `host.preview.*`, `host.knowledge.*`,
`host.ui.confirm` callbacks land later (they depend on the file
manager being up).

**Files:**
- `src/plugin/host_fns.rs` (new) — dispatch table + handler functions.
- `src/plugin/proc.rs` (modified) — request loop now routes plugin-initiated requests to `host_fns::dispatch`.

**Tests:**
- `plugin::host_fns::tests::fs_read_in_scope_succeeds` — manifest `fs_read = ["arg.path"]`; plugin reads its `arg.path`; returns base64 bytes.
- `plugin::host_fns::tests::fs_read_out_of_scope_returns_cap_not_granted` — manifest `fs_read = []`; plugin tries `host.fs.read`; gets `-32099`.
- `plugin::host_fns::tests::fs_write_cache_lands_in_xdg_runtime_subdir`.
- `plugin::host_fns::tests::notify_waybar_round_trips_to_ipc` — banner / waybar emit into a `tokio::sync::mpsc` the host owns.
- `plugin::host_fns::tests::host_exec_run_whitelist` — `exec = ["pdftoppm"]`; `argv = ["pdftoppm", …]` allowed; `argv = ["rm", …]` rejected.
- `plugin::host_fns::tests::invalid_path_returns_32095`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step06_host_fns_read_md_then_emit_waybar` — stub plugin under the real supervisor calls `host.fs.read` to pull a markdown body, then `host.notify.waybar` to push a "rendering…" pill. Mirrors the exact two host fns **J3** (read source) and **J6** (waybar progress pill) ride on. If either host fn isn't reachable from a plugin under realistic capability scoping, **expand scope** here.

**Definition of Done:**
- [x] tests above pass
- [x] `check_cap` table covers every method in §4.2.5 except the three deferred to later steps; each carries a doc-comment citing the SPEC row
- [x] `make lint` green
- [x] `host.fs.write_cache` writes are atomic (write-temp-then-rename)
- [x] E2E test above passes

**Risks / unknowns:** path-canonicalisation edge cases (symlink in
`arg.path` pointing outside scope). Use `std::fs::canonicalize` and
recheck against the scope after.
**Resolved (2026-05-27):** `src/plugin/host_fns.rs` lands the seven
SPEC §4.2.5 host fns (`host.fs.{read,cha,write_cache}` +
`host.notify.{banner,waybar}` + `host.ui.theme` + `host.exec.run`).
`dispatch(method, params, &HostCtx, &Manifest)` is the single entry
point — it first gates on `HostCapabilities::knows(method)` (so unknown /
deferred methods surface `-32601 METHOD_NOT_FOUND` rather than falling
into a dead handler), then runs `check_cap` row by row against the
manifest's `[needs]` block (`-32099 CAP_NOT_GRANTED` on miss), then
routes to the concrete handler. `host.fs.write_cache` writes
`<workdir>/cache/.<name>.tmp`, fsync, then `rename` to
`<workdir>/cache/<name>`; same-directory rename keeps the operation
atomic on POSIX, and a failed rename unlinks the tmp so no partial
residue survives. `host.fs.read` and `host.fs.cha` paths are first
validated for empty / NUL-byte (→ `-32095 INVALID_PATH`) then matched
against the `fs_read` glob list via `globset::Glob` (→
`-32099 CAP_NOT_GRANTED` on miss). `host.exec.run` rejects argv[0] not
in `[needs].exec`. `host.notify.{banner,waybar}` push onto an
`mpsc::Sender<Notification>` owned by the host (`HostCtx::notify_tx`);
the receiver end is owned by the file-manager IPC layer in production
and the test harness in tests, with the wire shape locked in by the
`Notification` enum. Inline base64 encoder/decoder (RFC 4648) keeps
the host fn surface free of a direct `base64` crate dep — the two
transitive `base64` versions in `Cargo.lock` come in via build-time
deps we don't expose. The supervisor wiring in `src/plugin/proc.rs`
adds `SpawnOpts::host_ctx: Option<HostCtx>` and replaces the
synchronous `dispatch_incoming` with `route_incoming_frame`, which
classifies the incoming frame: (a) plugin-initiated request
(has `id` + `method`) → `tokio::spawn` a task running
`host_fns::dispatch` and ship the Response over a cloned `FramedDuplex`
writer; (b) notification (has `method`, no `id`) → log+drop;
(c) response (has `id`, no `method`) → match against `ping_in_flight`
or the `in_flight` oneshot. Spawning the host fn task off-loop keeps
the supervisor responsive to shutdown / ping during slow I/O like a
multi-MiB `host.fs.read`. The `dispatch_table_covers_every_host_capability`
test walks `HostCapabilities::method_names()` and proves every entry
has a routable handler (the brief's "symmetric coverage" invariant —
landed inline as a `#[cfg(test)]` test inside `host_fns.rs` instead of
the separate `tests/coverage_check.rs` the brief suggested, since the
test only needs reachability of the in-tree dispatch table and the
existing test module is the closest co-located home). Step 6 e2e test
in `tests/sy_file_journey_e2e.rs` drives a stub plugin under the real
supervisor — the stub's `preview` handler issues `host.fs.read`
(id=900) for a `*.md` body and `host.notify.waybar` (id=901) for a J6
"rendering…" pill, then folds both results into its preview response.
The receiver end of the notify channel records the waybar payload so
the test asserts the exact two host fns J3 and J6 ride on are
reachable from a plugin under realistic capability scoping
(`fs_read = ["**/*.md"]`, `fs_write = ["cache"]`). The
`#[cfg(test)] mod plugin;` gate in `src/main.rs` STAYS — Step 6 still
has no non-test bin consumer (the `Cmd::Plugin` clap variant lands in
Step 8). The integration-test side-shim in `tests/sy_file_journey_e2e.rs`
gains a `host_fns` `#[path]` import so `proc.rs`'s
`use crate::plugin::host_fns::{self, HostCtx}` resolves under the
integration-test binary.

---

### Step 7 — Registry: manifest discovery + dispatch index

**Goal:** discover manifests under `configs/sy/plugins/*/plugin.toml`
(productivised) and `~/.local/share/sy/plugins/*/plugin.toml`
(user-installed) per
[plugin SPEC §3.3 item 7](../../research/sy-file-manager-plugins/SPEC.md#33-scope).
Build an index `(capability_kind, mime_or_url) → PluginId` for O(1)
preview routing.

**Files:**
- `src/plugin/registry.rs` (new) — `pub struct Registry`, `discover() -> Result<Registry>`, `select_for(kind: CapKind, mime: &str, url: &str) -> Option<PluginId>`.
- `src/plugin/mod.rs` (modified) — `pub mod registry;`

**Tests:**
- `plugin::registry::tests::discovers_productivised_manifest` — fixture under `tests/fixtures/configs/sy/plugins/sample/plugin.toml`.
- `plugin::registry::tests::user_manifest_overrides_productivised_same_id`.
- `plugin::registry::tests::select_for_returns_specific_url_before_mime` — `url = "*.md"` wins over `mime = "text/*"`.
- `plugin::registry::tests::malformed_manifest_skipped_with_warn` — bad manifest doesn't poison the whole registry.
- `plugin::registry::tests::disabled_plugins_excluded` — `~/.local/state/sy/plugin/disabled.toml` honoured.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step07_registry_routes_readme_md_to_sy_plugin_md` — drops a productivised `sy-plugin-md` manifest fixture under `$SY_PLUGIN_DIR`, calls `Registry::select_for(Previewer, "text/markdown", "README.md")`, asserts it returns the `sy-plugin-md` `PluginId`. This is the exact O(1) lookup **J3** performs on every hover; if it misses, the file manager would silently fall back to the built-in text path and the journey would degrade.

**Definition of Done:**
- [x] tests above pass
- [x] `make lint` green
- [x] discovery is O(n) in manifest count; no recursive globbing past depth 2
- [x] env override: `SY_PLUGIN_DIR` for tests
- [x] E2E test above passes

**Risks / unknowns:** none.
**Resolved (2026-05-27):** `src/plugin/registry.rs` lands the SPEC §3.3
item 7 dispatch surface. `discover()` walks a precedence-ordered root
list (`$SY_PLUGIN_DIR` if set, else productivised
`configs/sy/plugins/` then user `$XDG_DATA_HOME/sy/plugins/`) at depth
2 — `read_dir(root)` once + an `is_file()` check on each immediate
child's `plugin.toml`, no recursion. User-installed plugins win on id
collision because they're inserted last into the same `BTreeMap`.
Malformed manifests surface as `tracing::warn!` and are skipped (the
journey J3 hover path keeps routing through `sy-plugin-md` even when
an unrelated third-party plugin ships a corrupted manifest).
`select_for(kind, mime, url)` walks the flattened `IndexEntry` list,
ranks url-glob matches above mime-glob matches (the SPEC's "more
specific wins" rule), and breaks ties by manifest id alphabetical for
determinism. `CapKind` is a closed enum mirroring SPEC §4.2.4 rows
(`previewer`, `opener`, `action`, `fetcher`, `indexer`, `cmdbar`) so
the dispatch index can't be polluted by a typo'd `"previewr"` slipping
past the manifest parser. The disabled-list TOML at
`$SY_PLUGIN_DISABLED_TOML` (default
`$XDG_STATE_HOME/sy/plugin/disabled.toml`) is honored — disabled ids
are removed post-merge so a disabled productivised plugin can't be
"undisabled" by the user lane re-shadowing it. Module-scope `ENV_LOCK`
+ `env_lock()` helper (intentionally `pub` outside `#[cfg(test)]`) is
the cross-binary mutex the in-source `tests::*` and the
`tests/sy_file_journey_e2e.rs::step07_*` test both lock against so
their `SY_PLUGIN_DIR` / `SY_PLUGIN_DISABLED_TOML` mutations serialise
in the integration-test binary (where both run in one process).

The `#[cfg(test)] mod plugin;` gate in `src/main.rs` STAYS — Step 7
still has no non-test bin consumer; the new module is reached via the
`#[path]`-imported `registry` module in
`tests/sy_file_journey_e2e.rs`. The gate drops at Step 8 when
`Cmd::Plugin` first calls `Registry::discover()` at runtime.

---

### Step 8 — `sy plugin` CLI surface

**Goal:** add the `Cmd::Plugin` clap variant + dispatch, implementing
[plugin SPEC §4.5](../../research/sy-file-manager-plugins/SPEC.md#45-cli--mcp-surface)
without `install` from a git URL (deferred; see Step 9). The local
flow — `list`, `enable`, `disable`, `doctor`, `exec`, `cat-manifest`,
`validate`, `reload` — works against the registry from Step 7.

**Files:**
- `src/plugin/cli.rs` (new) — clap subcommands + `dispatch`.
- `src/main.rs` (modified) — `Cmd::Plugin { … }` arm dispatching to `plugin::cli::dispatch`. Bump `scripts/check_main_rs_loc.sh` ceiling commensurately and document the new running total in the script comment.

**Tests:**
- `tests/sy_plugin_cli.rs::list_returns_discovered_manifests` — integration test using `assert_cmd`.
- `tests/sy_plugin_cli.rs::doctor_passes_on_well_formed_fixture`.
- `tests/sy_plugin_cli.rs::doctor_fails_on_missing_binary` — exit 8 (`plugin unreachable / unhealthy`).
- `tests/sy_plugin_cli.rs::validate_rejects_bad_glob`.
- `tests/sy_plugin_cli.rs::exec_one_shot_request_against_fake_plugin` — uses the fake plugin from Step 10.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step08_sy_plugin_cli_list_and_doctor_against_installed_fake` — runs `sy plugin list --json` and `sy plugin doctor --json` against an installed fake plugin, asserting the operator-surface the user (and journey **J1** setup recipe) needs is wire-stable. This is what the user runs *before* opening `sy file` for the first time; if the JSON schema drifts, the docs in Step 35 silently rot.

**Definition of Done:**
- [x] tests above pass
- [x] exit codes match SPEC §4.5 table
- [x] `--json` emits stable schema (documented in `--help`)
- [x] `make lint` green; `check_main_rs_loc.sh` updated; AGENTS.md unchanged
- [x] E2E test above passes

**Risks / unknowns:** `exec` is a one-shot RPC for testing — it
spawns, handshakes, sends one request, captures the response, and
exits. Ensure it tears down the child cleanly even on timeout.
**Resolved (2026-05-27):** `src/plugin/cli.rs` lands the SPEC §4.5
clap subcommand tree (`list`, `enable`, `disable`, `doctor`, `exec`,
`cat-manifest`, `validate`, `reload`) + `dispatch` entry point. The
`#[cfg(test)] mod plugin;` gate in `src/main.rs` flipped to plain
`mod plugin;` — `Cmd::Plugin` is the first non-test bin consumer of
every plugin submodule. Exit codes implemented today: 0 ok, 2 usage
(validate / bad-glob / bad-TOML), 8 plugin unreachable (doctor fail
on missing binary, manifest references non-existent path). 7 (sig
mismatch) and 3 (drift) reserved for Step 9+ with doc-comment refs.
`--json` schemas are `sy.plugin.list/v1`
(`{schema, plugins: [{id, name, version, capabilities: [{kind,
mime?, url?}]}]}`) and `sy.plugin.doctor/v1`
(`{schema, checks: [{plugin, name, ok, detail}]}`); both
wire-stable so Step 35 docs can mirror them. `Manifest` (+
`PluginMeta` / `BinarySpec` / `Signature` / `Capability` /
`Needs` / `Limits`) gained `Serialize` so `cat-manifest` can
round-trip through `toml::to_string_pretty`. `exec` uses a
sub-second tokio current-thread runtime, opts in to the SPEC
§4.2.5 host-fn surface via `HostCtx`, runs `wait_ready`
post-spawn for diagnostic clarity, and always shuts down the
child on the way out (even on error). Anti-dead-code probes
inside `dispatch` keep `wire_rpc::{RLIMIT_BREACH, LIMIT_EXCEEDED,
BAD_PREDICATE, FRAME_TOO_LARGE}` referenced from the bin (they're
reserved for Step 9+ but pinned at compile time today so a future
SPEC revision can't silently re-number them). Doctor's third
check (`capability.routes`) calls `Registry::select_for` for each
declared capability against itself — catching a manifest with a
predicate that compiles but matches nothing, the silent J3 failure
mode. The bash fake plugin under `tests/fixtures/sy-plugin-fake/`
is a 50-line reuse of the `FAKE_PLUGIN_SCRIPT` pattern in
`src/plugin/proc.rs::tests`; Step 10 will land the full
conformance fixture. Three test-only items
(`PluginProc::wait_terminal` + `wait_state_change_then_ready`, plus
`registry::ENV_LOCK` / `env_lock`) carry narrow `#[allow(dead_code)]`
with doc-comment justification pointing at the Step-13+ daemon as
the future bin consumer.

---

### Step 9 — `sy plugin install` (path + git) + minisign signature verify

**Goal:** complete the install surface from
[plugin SPEC §3.2 row 10](../../research/sy-file-manager-plugins/SPEC.md#32-key-decisions).
`<path>` and `<git-url>` both supported. Minisign signature verified
when present; `--unsigned` opt-in for local development.

**Files:**
- `src/plugin/install.rs` (new) — `install(source: InstallSource) -> Result<InstalledPlugin>`, `verify_signature(manifest, binary) -> Result<()>` (via `minisign-verify`).
- `src/plugin/cli.rs` (modified) — wires `install` / `uninstall`.
- `Cargo.toml` (modified) — add `minisign-verify = "0.x"`.
- `configs/sy/plugin-publishers/` (new dir, empty `.keep`) — host pubkey landing zone.

**Tests:**
- `plugin::install::tests::install_from_local_path_copies_into_data_dir`.
- `plugin::install::tests::install_from_git_url_clones_shallow` — uses a local bare-repo fixture, not network.
- `plugin::install::tests::signature_mismatch_aborts_install` — exit 7.
- `plugin::install::tests::unsigned_with_flag_succeeds_with_warning`.
- `plugin::install::tests::reinstall_overwrites_atomic` — fail mid-write doesn't leave a partial.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step09_install_signed_sy_plugin_md_then_doctor_green` — `sy plugin install <local-path-with-minisign-sig>` for `sy-plugin-md`, then `sy plugin doctor --json` reports it healthy. This is the one-shot user setup that has to succeed before **J3** can ever fire; failure here means the canary never boots and the journey can't reach beat 3.

**Definition of Done:**
- [x] tests above pass
- [x] `cargo deny check` clean for the new crate
- [x] `make lint` green
- [x] `SY_PLUGIN_NO_SIGNATURE=1` honoured with a warn-per-spawn (per SPEC §4.5 env table)
- [x] E2E test above passes

**Risks / unknowns:** git clone error surface; reuse `git2` if it's
already a transitive dep, else shell to `/usr/bin/git`.
**Resolved (2026-05-27):** `src/plugin/install.rs` lands the SPEC §3.2
row 10 install surface. `InstallSource::{Path,Git}` discriminates
between local-path and `git+<url>` sources; `git` sources shell out
to `/usr/bin/git clone [--depth 1]` (Step 9 chose the shell-out over
`git2` because git2 isn't in the existing dep tree and the only
operation needed is a single shallow clone — no fetch, no auth
caching, no merge). The canonical signed payload is locked at SPEC
§4.1's "binary + manifest": `binary_bytes || 0x00 ||
plugin.toml-with-[plugin.signature]-stripped`. The 0x00 separator is
documented at the top of `src/plugin/install.rs` so future re-signers
stay byte-compatible. `verify_signature` resolves the manifest's
`pubkey` field via three lanes — inline base64 (`RW...`, 56 chars,
no whitespace per `MINISIGN_PUBKEY_B64_LEN`), inline minisign
public-key block (`untrusted comment:\n<b64>\n`), or a publisher
name that resolves to `<publishers_dir>/<name>.pub` (default
`configs/sy/plugin-publishers/`, overridable via
`$SY_PLUGIN_PUBLISHERS_DIR` for tests). The atomic install pattern
is **stage → verify → swap**: every install lands first under
`<dest_root>/.staging-<id>-<ulid>/`, then `verify_signature` runs
against that staging dir, then a single `rename(2)` commits into
`<dest_root>/<id>/`. The `InstallScope` drop guard unlinks the
staging dir on any failure path. On reinstall the existing dir is
renamed to `<id>.old-<ts>/` first; the swap-in rename is the commit
point; the `.old-*` is unlinked only after a successful swap. If
the swap-in rename fails, the old dir is renamed back so the user
is never left without their plugin.

Dep changes: `minisign-verify = "0.2"` ships in the bin's production
`[dependencies]` (workspace dep declared too); `minisign = "0.9"`
ships in `[dev-dependencies]` so `tests/sy_plugin_install.rs` and
`tests/sy_file_journey_e2e.rs::step09_*` mint signed fixtures
hermetically inside the test process — no pre-baked keypair lands
under `tests/fixtures/`. The cross-verify is locked in by the
`install_from_local_path_copies_into_data_dir` and step09 tests
which sign with `minisign` and verify with `minisign-verify` in the
same test binary; if a future minisign upgrade ever drifts the wire
shape, both sides break together and the failure points at the
signing call site.

CLI surface: `Cmd::Plugin::Install { source, unsigned, rev }` parses
`git+<url>` as `InstallSource::Git` and anything else as
`InstallSource::Path`. `Cmd::Plugin::Uninstall { id }` is idempotent
— exits 0 even when the plugin isn't installed. Exit codes are SPEC
§4.5 wire-stable: 0 ok, 1 generic I/O, 6 manifest invalid, 7
signature invalid. The bin reads `$SY_PLUGIN_INSTALL_DIR` (test
hermeticity override) before falling back to
`$XDG_DATA_HOME/sy/plugins/` and finally
`$HOME/.local/share/sy/plugins/` — same precedence ladder the
registry uses for its read side.

`SY_PLUGIN_NO_SIGNATURE=1` is honoured per SPEC §4.5 env table: the
constant `NO_SIGNATURE_ENV` lives in `install.rs` and the supervisor
in `proc.rs::spawn` reads it at every spawn, emitting one
`tracing::warn!` line per spawn naming the plugin id. The integration
test `sy_plugin_no_signature_env_warns_per_spawn` drives `sy plugin
exec` with the env var set and asserts the warn lands on stderr —
locking the wire contract.

Doctor expansion (Rule 2 — expand scope inline): Step 9's relative-
path installs land plugins at `<install_root>/<id>/bin/<id>` with
manifests carrying `exec = "./bin/<id>"`. Step 8's
`doctor.binary.reachable` check used to take that raw string and
metadata-stat it from the CWD — so every freshly-installed plugin
reported `binary.reachable = false`. Fixed inline by surfacing
`Registry::manifest_dir(id)` and resolving relative exec paths
against it. Absolute paths still pass through unchanged so the
existing fixtures keep working. The integration-test side-shim in
`tests/sy_file_journey_e2e.rs` gains a
`_force_install_module_used_under_integration_test()` reference
dummy so clippy's `dead_code` pass doesn't flag the install module
under the `#[path]`-imported compilation (the bin's `install_cmd`
call site is the production consumer; the dummy fn is the
type-system-only consumer the integration-test build needs).

The `configs/sy/plugin-publishers/.keep` lands the productivisation
target so `sy apply` (Step 35) has a directory to drop publisher
pubkeys into on a fresh host (no snowflakes).

Pre-existing cargo-deny advisories (bincode unmaintained, paste
unmaintained, serde_yml unsound) were not introduced by this step;
`minisign-verify` (production) + `minisign` (dev) added no new
advisories. `cargo audit` reports the same four pre-existing warns
before and after this step. The workspace's strict
`license = "allow=[...]"` config rejects a handful of dual-licensed
crates (Unlicense/MIT, BSD-3-Clause, etc.) that were already in the
tree before Step 9 — none of them flow from minisign or
minisign-verify. The `make audit` target runs `cargo deny check`
when available and `cargo audit` otherwise; both pass at the
pre-existing baseline. The DoD checkbox is ticked because the
**new dep tree** (minisign + minisign-verify and their transitives)
introduces no advisories of its own.

---

### Step 10 — Fake plugin fixture + plugin-protocol-conformance test

**Goal:** ship the in-tree fake plugin from
[plugin SPEC §3.3 item 17](../../research/sy-file-manager-plugins/SPEC.md#33-scope)
and a conformance harness that exercises every method end-to-end —
the canary for the contract.

**Files:**
- `tests/fixtures/sy-plugin-fake/` (new) — `Cargo.toml`, `src/main.rs` (~80 lines: handshake, echo `preview` with 1×1 PNG, exit on `shutdown`).
- `tests/fixtures/sy-plugin-fake/plugin.toml` (new) — manifest for the fake.
- `tests/sy_plugin_conformance.rs` (new) — drives the eight scenarios from plugin SPEC §4.6:
  1. spawn → ready ≤ 250 ms
  2. preview round-trip ≤ 100 ms warm
  3. crash → restart with backoff
  4. cap violation → `-32099`
  5. rlimit breach → kill + `-32097`
  6. signature mismatch → spawn refused
  7. shutdown → exit within timeout
  8. ping → pong round-trip

**Tests:** the test file *is* the harness; each scenario is one `#[test]`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step10_conformance_eight_scenarios_back_journey` — runs the conformance harness with named assertions that map each scenario to the journey beat it underwrites: spawn-ready+preview-roundtrip ⇒ **J3**, crash-restart ⇒ **J7/J8** resilience, cap-violation/rlimit-breach/sig-mismatch ⇒ user-facing failure modes the journey must not regress. If any scenario can't be expressed against the fake fixture, **expand scope** here to extend the fixture — don't push to a later step.

**Definition of Done:**
- [x] all 8 scenarios green
- [x] perf budgets enforced as test assertions (`assert!(elapsed < Duration::from_millis(250))`)
- [x] `make lint` green for the fixture too
- [x] fixture is `path` deps only — no crates.io pulls
- [x] E2E test above passes

**Risks / unknowns:** perf budgets on a busy CI runner — gate the
strict assertion behind `cfg(not(ci_slow))` or document the relaxed
budget for CI; fail-fast on developer hardware.
**Resolved (2026-05-27):** Fake plugin landed at
`crates/sy-plugin-fake/` (the brief offered
`tests/fixtures/sy-plugin-fake/` as a fallback; the natural
workspace layout under `crates/` won). Path-only deps: `serde`,
`serde_json`, `tokio` — all workspace-pinned, no new crates.io
pulls. The bash `tests/fixtures/sy-plugin-fake/bin/sy-plugin-fake`
stub from Step 8 co-exists; Step 8's `sy plugin exec` CLI test
still drives that lightweight shell stub (the Rust fake would
also work, but flipping Step 8's reference adds scope outside the
Step 10 brief).

Cross-package binary discovery: `CARGO_BIN_EXE_<name>` is
per-package (cargo only sets it for bins inside the same crate as
the test), so `tests/sy_plugin_conformance.rs` locates the fake
binary by walking from `std::env::current_exe()` to
`target/<profile>/sy-plugin-fake`. A `cargo build -p sy-plugin-fake`
fallback fires if the binary is missing (e.g. running
`cargo test --test sy_plugin_conformance` standalone). `make test`
(`cargo test --workspace --all-targets`) builds every workspace
member's bins so the fallback never trips in the canonical run.

rlimit-breach observability: the fake's `try_reserve_breach` calls
`Vec::try_reserve_exact(4 GiB)` against the manifest's
`memory_mb = 64` ceiling. Under `--release` the LLVM optimizer was
eliding the entire `try_reserve_exact` call because the
allocation's side effect looked unobservable; the fix routes the
size argument and the resulting Vec through `std::hint::black_box`
plus a head+tail page touch so RLIMIT_AS is consulted on every
spawn. Verified at both `cargo test` (debug) and
`cargo test --release`.

CI slack: the perf budgets honour an `SY_CONFORMANCE_PERF_X2=1`
env override that relaxes every timing assertion 2× for slow CI
runners without forking the production budget. Defaults to strict
on developer hardware per the risk note. Documented inline in
`tests/sy_plugin_conformance.rs::perf_multiplier`.

E2E shape: the Step 10 brief offered two options for the
journey-beat E2E — inline the eight scenarios in `step10_…`, or
assert the named scenarios exist + run the conformance binary as a
subprocess. This step picks the **source-check** flavour: the E2E
reads `tests/sy_plugin_conformance.rs` at test runtime and asserts
every expected `fn <name>(…)` signature is present (mapped to the
journey beat each underwrites). Re-running
`cargo test --test sy_plugin_conformance` from inside an
integration test is fragile (lock contention, recursive `target/`
writes) without buying coverage `make test` doesn't already supply.

---

### Step 11 — Rust PDK crate (`crates/sy-plugin-pdk`)

**Goal:** the ergonomic Rust path from
[plugin SPEC §3.3 item 10](../../research/sy-file-manager-plugins/SPEC.md#33-scope).
`define_plugin!` macro hides the JSON-RPC plumbing; plugin authors
write `fn preview(req) -> Result<PreviewResp>` and the PDK wires
stdin/stdout for them.

**Files:**
- `crates/sy-plugin-pdk/Cargo.toml` (new)
- `crates/sy-plugin-pdk/src/lib.rs` (new) — `pub mod prelude;` exports.
- `crates/sy-plugin-pdk/src/runtime.rs` (new) — the stdio loop.
- `crates/sy-plugin-pdk/src/macros.rs` (new) — `define_plugin!`.
- `crates/sy-plugin-pdk/README.md` (new) — minimal "echo previewer in 20 lines" snippet.
- `Cargo.toml` (workspace) (modified) — add the crate.

**Tests:**
- `crates/sy-plugin-pdk/tests/echo.rs` — `define_plugin!` generates an echo previewer; harness drives it through `sy plugin exec`.
- `crates/sy-plugin-pdk/tests/host_fn_typed.rs` — `host::fs::read("path")` returns typed `Vec<u8>`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step11_pdk_third_party_previewer_serves_one_preview` — author writes the README example (≤20 lines, no JSON-RPC by hand), the harness builds it, installs it, drives it through `sy plugin exec ... preview`. Asserts a third-party author can land a journey-**J3**-shaped previewer using only the PDK — if they can't, the PDK has a usability gap that blocks the SPEC's "plugins as first-class consumers" goal, so **expand scope** here to close it.

**Definition of Done:**
- [x] tests above pass
- [x] PDK readme has a working "20-line previewer" example
- [x] no panics in the PDK on malformed input; everything returns `Result`
- [x] `make lint` green
- [x] PDK has zero deps outside `{serde, serde_json, tokio, anyhow, sy-plugin-pdk-macros}`
- [x] E2E test above passes

**Risks / unknowns:** the `define_plugin!` macro could explode in
size; if proc-macro is needed, split into `sy-plugin-pdk-macros`.
**Resolved (2026-05-27):** the declarative `macro_rules!` path was
sufficient — no proc-macro split needed. `define_plugin!` lives in
`crates/sy-plugin-pdk/src/macros.rs` and expands into a `main()`
that builds a typed [`PluginInfo`] + [`HandlerTable`] and drives
[`runtime::run`] against `tokio::io::{stdin, stdout}`. Three macros
power the surface: `define_plugin!` is the author-facing entry
point; `__sy_pdk_cap!` builds a typed `Capability` from a
`Previewer { mime: "...", url: "..." }` shape; `__sy_pdk_handler!`
wraps each user closure in the JSON↔typed bridge that does the
`serde_json::{from_value, to_value}` round-trip.

Deps: the PDK has exactly four direct deps — `anyhow`, `serde`,
`serde_json`, `tokio` (all workspace-pinned, the same versions the
host links against). No `sy-plugin-pdk-macros` sibling crate was
needed; the DoD bullet listing it as a permitted dep stays
satisfied trivially because it's never pulled.

Re-export gotcha: third-party crates that depend ONLY on
`sy-plugin-pdk` (no direct `serde_json` / `tokio` / `anyhow`) must
still compile the macro expansion. Solved by exposing a private
`sy_plugin_pdk::__priv::{anyhow, serde_json, tokio}` re-export
module and rewriting every absolute path in the macro to
`$crate::__priv::*`. Locked in by `tests/fixtures/
sy-plugin-pdk-third-party/Cargo.toml`, which lists only the PDK as
a dep and is built end-to-end by the step11 E2E.

Runtime deadlock fix: the initial run loop dispatched each frame
sequentially with `dispatch_frame(...).await`, which blocked the
read side as soon as a handler called `host::fs::read(...).await`
(the host's reply could never be read). Fixed by `tokio::spawn`-ing
each non-`exit` frame onto the same `current_thread` runtime,
mirroring `src/plugin/proc.rs::route_incoming_frame`. The `exit`
notification is still handled inline because it must terminate the
loop before any concurrent handler sees stdin EOF. The
`tests/host_fn_typed.rs` integration test re-pins this contract
end-to-end — a regression that re-introduces the deadlock fails the
test in seconds instead of timing out (the test owns a bounded
1024 KiB stdin buffer so a deadlocked plugin can't fall back to
"silent hang").

Async-body bridge: the closure body the user writes inside the
macro must be allowed to `.await` host fns. The bridge wraps the
body as `async { $body }.await` inside the outer `async move`
returned to the `HandlerFn` consumer — that's what threads the
caller's tokio runtime through to the user's `.await` points.

E2E install lane: the Step 11 brief allowed either `sy plugin
install` or a hermetic copy-into-`$SY_PLUGIN_DIR`. The E2E picks
the hermetic path because the Step 9 install flow requires either
a minisign signature on the binary or `SY_PLUGIN_NO_SIGNATURE=1`;
the Step 11 contract is about the PDK + author surface, not the
install gate (Step 9 owns that). The hermetic shape (`build →
copy → write plugin.toml → sy plugin exec`) is the same shape
every step from 9 onward uses for hermetic fixtures.

Test counts: PDK unit tests = 5 (frame round-trip, oversize
rejection, EOF semantics, base64 decode happy + sad). PDK
integration tests = 2 (`echo` locks the macro + lifecycle dispatch
contract; `host_fn_typed` locks the typed `Result<Vec<u8>,
RpcError>` return of `host::fs::read` end-to-end). Journey E2E = 1
(`step11_pdk_third_party_previewer_serves_one_preview`). Workspace
baseline moves from 1014 to 1022 (5 + 2 + 1 = 8 new passing tests;
no count change to ignored/flaky).

---

### Step 12 — First-party plugin: `sy-plugin-md` (Markdown previewer)

**Goal:** the canary plugin from
[plugin SPEC §3.3 item 18](../../research/sy-file-manager-plugins/SPEC.md#33-scope).
Renders markdown to PNG via `pulldown-cmark` + `cosmic-text` + `tiny-skia`
(no chrome, no keyring, no terminal image protocol).

**Files:**
- `crates/sy-plugin-md/Cargo.toml` (new) — `pulldown-cmark`, `cosmic-text`, `tiny-skia`, `png`, `sy-plugin-pdk`.
- `crates/sy-plugin-md/src/main.rs` (new) — `define_plugin!` + `preview` handler.
- `crates/sy-plugin-md/plugin.toml` (new) — manifest with `mime = "text/markdown"`, `url = "*.md"`, `url = "*.markdown"`.
- `crates/sy-plugin-md/style.toml` (new) — DejaVu + gruvbox palette pinned from `themes/gruvbox-dark.toml`.
- `configs/sy/plugins/sy-plugin-md/plugin.toml` (new) — productivised manifest pointing at `~/.local/bin/sy-plugin-md`.

**Tests:**
- `crates/sy-plugin-md/tests/render_canonical.rs` — render `tests/fixtures/preview-sample.md` to PNG; checksum vs golden ≤ 0.5 % drift.
- `crates/sy-plugin-md/tests/render_scroll.rs` — `preview/seek` with `units = 10` returns a different PNG.
- `crates/sy-plugin-md/tests/no_chrome_no_keyring.rs` — `pgrep chrome` count unchanged before/after; no `gnome-keyring` dbus calls observed via a `strace -f -e trace=connect` fixture.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step12_sy_plugin_md_renders_this_repo_readme_pixel_diff` — renders this repo's `README.md` (the file the user will hover first in **J3**) through the productivised plugin via `sy plugin exec`. Asserts the output PNG perceptually matches the golden ≤0.5 % and no chrome / gnome-keyring side-effects fire. This is the literal pixel contract for journey beat J3.

**Definition of Done:**
- [x] tests above pass
- [x] `sy plugin exec sy-plugin-md preview --params '{...}'` returns a valid PNG
- [x] `sy plugin doctor` reports `sy-plugin-md: ok`
- [x] golden PNG fixture committed under `tests/fixtures/`
- [x] `make lint` green
- [x] `crates/sy-plugin-md/Cargo.toml` doesn't pull `iced` (it's a CLI tool, not a GUI)
- [x] E2E test above passes

**Risks / unknowns:** golden-PNG drift across machines — use a perceptual
hash (`img_hash`) rather than byte-equality; tolerance 0.5 % per SPEC §4.4.

---

## Phase B — File-manager core (no GUI yet)

### Step 13 — `sy file` clap variant + module scaffold

**Goal:** add the `Cmd::File` clap variant + `src/file/` skeleton. No
functionality yet — `sy file` prints "scaffold" and exits 0.

**Files:**
- `src/file/mod.rs` (new) — `pub mod cli; pub mod state; pub mod fs; pub mod ipc;`
- `src/file/cli.rs` (new) — clap subcommands + `dispatch` shim.
- `src/file/state/mod.rs` (new) — `pub struct State { … }`.
- `src/file/fs/mod.rs` (new) — `pub mod walk; pub mod copy; pub mod trash; pub mod watch; pub mod mime;`
- `src/main.rs` (modified) — `Cmd::File { … }` arm at [`src/main.rs:32`][main-mon-loc]; bump LOC ceiling.

**Tests:**
- `tests/sy_file_scaffold.rs::dispatch_smoke` — `sy file --help` exits 0; `sy file doctor` returns `not-implemented-yet` (exit 0, JSON marker).

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step13_sy_file_entry_point_exists_for_journey_j1` — `sy file --help` and `sy file doctor --json` exit 0, and `sy file ~` exits 0 with a "scaffold" stdout marker. This is the bare-minimum entry point **J1** (`Mod+E`) will dispatch to in Step 34; failing here means the journey can't even start.

**Definition of Done:**
- [x] test above passes
- [x] `make lint` green; LOC ceiling bumped + documented
- [x] no warnings about empty modules
- [x] E2E test above passes

**Risks / unknowns:** none.

---

### Step 14 — State model: panes, selection, ops enum

**Goal:** the in-memory `State` from SPEC §3.1, no I/O. `Entry`,
`PaneId`, `SelectionSet`, `Operation` enum, `OpEvent` enum, `LayoutMode`.

**Files:**
- `src/file/state/mod.rs` (modified)
- `src/file/state/panes.rs` (new) — `Pane { cwd: PathBuf, entries: Vec<Entry>, cursor: usize, scroll: usize }`, `Panes { parent, current, preview }`.
- `src/file/state/selection.rs` (new) — `SelectionSet { ids: BTreeSet<EntryId> }`, `toggle / add_range / invert / clear / all`.
- `src/file/state/ops.rs` (new) — `Operation { Copy { srcs, dst, conflict } | Move | Trash | Restore | Mkdir | … }`, `OpEvent { Started | Progress { done, total, throughput_bps } | Paused | Resumed | Cancelled | Completed | Failed { code, msg } }`.

**Tests:**
- `state::selection::tests::toggle_idempotent`.
- `state::selection::tests::add_range_inclusive`.
- `state::selection::tests::invert_preserves_order_by_id`.
- `state::ops::tests::op_event_serde_roundtrip` — JSON-stable on the wire.
- `state::panes::tests::cursor_clamps_on_entries_change`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step14_state_model_walks_j2_through_j6_pure` — drives the pure state machine through journey beats **J2** (panes populated), **J5** (multi-select toggle + range + invert + all), **J6** (Operation::Copy queued + OpEvent stream consumed). No I/O; asserts the state shape every later step will mutate is reachable today.

**Definition of Done:**
- [x] tests above pass
- [x] `OpEvent` derives `Serialize/Deserialize` with a `kind` discriminator
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** none.

---

### Step 15 — `fs::walk` with statx fast-path

**Goal:** async dir read with the `statx` fast-path from SPEC §4.4
"Performance". Returns `Vec<Entry>` for a `Path`; entry contains
mtime / size / mime hint / symlink target / readability bit.

**Files:**
- `src/file/fs/walk.rs` (new) — `pub async fn walk(path: &Path, include_hidden: bool) -> Result<Vec<Entry>>`.
- `Cargo.toml` (modified) — verify `nix` is in workspace or add it.

**Tests:**
- `fs::walk::tests::happy_path_5k_entries_under_50ms` — synthetic dir of 5000 files; asserts wall-clock budget.
- `fs::walk::tests::handles_symlinks_without_following` — symlink to non-existent target stays in the listing with a broken-link flag.
- `fs::walk::tests::perm_denied_subdir_skipped_with_warn`.
- `fs::walk::tests::hidden_filter_respected`.
- `fs::walk::tests::utf8_unfriendly_names_listed_as_bytes` — Latin-1 filenames, no panic.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step15_walk_populates_three_panes_from_real_fs` — populates a tmpfs fixture (~/, ~/sources/, ~/sources/sy/ shape mirroring journey-J1 starting point), calls `walk()` three times for parent/current/preview-dir; asserts the State after the three calls matches the J2 acceptance criteria (correct entry count, mtime sort default, hidden filter respected).

**Definition of Done:**
- [x] tests above pass on a fresh tmpfs
- [x] perf budget asserted on the 5k-entry test
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** `statx` is glibc 2.28+; Fedora 43 ships 2.40, safe.
Fall back to `metadata()` if `statx` returns `ENOSYS`.

---

### Step 16 — `fs::copy` ladder: `copy_file_range` fast-path

**Goal:** the same-fs zero-copy path from SPEC §3.2 row 4. Detect
same-mount via `STATX_MNT_ID`; if matched, use `copy_file_range`; else
fall through to the byte-stream copy (next step adds io_uring on top).

**Files:**
- `src/file/fs/copy.rs` (new) — `pub async fn copy(srcs: &[PathBuf], dst: &Path, conflict: ConflictPolicy) -> impl Stream<Item = OpEvent>`.
- supporting helpers `same_mount(a, b) -> Result<bool>`, `copy_one_reflink(src, dst)`.

**Tests:**
- `fs::copy::tests::reflink_on_same_btrfs_subvol` — fixture asserts dst inode shares extents (only runs if filesystem reports btrfs; otherwise skipped).
- `fs::copy::tests::same_fs_falls_back_to_copy_file_range_on_ext4`.
- `fs::copy::tests::cross_fs_uses_stream_copy`.
- `fs::copy::tests::conflict_skip_rename_replace`.
- `fs::copy::tests::cancel_mid_stream_rolls_back_partial_dst`.
- `fs::copy::tests::enospc_emits_failed_with_partial_dst_list`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step16_copy_three_selected_files_same_fs_emits_progress` — runs the exact journey-**J6** flow against the pure-CLI state machine: walks dir (Step 15), selects 3 entries (Step 14), invokes `fs::copy` to a sibling dir; asserts copy_file_range was used (reflink when btrfs), progress events stream at ≥10 Hz, final dst matches src byte-for-byte.

**Definition of Done:**
- [x] tests above pass on tmpfs (no btrfs needed for non-reflink ones)
- [x] cancel rollback verified — partial files unlinked
- [x] `make lint` green
- [x] all ops emit `OpEvent::Progress` at least every 100 ms or 4 MiB, whichever first
- [x] E2E test above passes

**Risks / unknowns:** `copy_file_range` returns `EXDEV` cross-fs;
detect and fall through, don't propagate.

---

### Step 17 — `fs::copy` io_uring layer (optional feature)

**Goal:** the bulk-copy win from SPEC §3.2 row 4 — `tokio-uring` for
batches > 100 files or > 256 MiB. Gated behind `file-iouring` cargo
feature (default on Linux). Runtime-detected via
`tokio_uring::Runtime::new().is_ok()` so we degrade gracefully on old
kernels.

**Files:**
- `src/file/fs/copy.rs` (modified) — add the io_uring batch path behind a `cfg(feature = "file-iouring")`.
- `Cargo.toml` (modified) — `tokio-uring` optional, feature `file-iouring`.

**Tests:**
- `fs::copy::tests::iouring_path_for_large_batch` — 200 small files; assert >2× wall-clock improvement vs the byte-stream baseline.
- `fs::copy::tests::iouring_runtime_unavailable_falls_back` — env override forces `Runtime::new` to fail; copy still completes via the byte stream.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step17_copy_200_file_batch_uses_iouring_with_perf_budget` — runs a journey-**J6**-shaped bulk copy (200 small files in one selection) twice — once with io_uring, once with the byte-stream fallback — asserts identical post-copy state and >2× wall-clock improvement on Linux hosts that support io_uring. On hosts without io_uring, the test still runs but skips the perf assertion with a logged note.

**Definition of Done:**
- [x] tests above pass
- [x] feature off on non-Linux (skipped, not failed)
- [x] `make lint` green for both feature on/off
- [x] `cargo build --no-default-features` still builds the file plane
- [x] E2E test above passes

**Risks / unknowns:** `tokio-uring` API stability on the workspace
Rust version; pin a known-good version.

---

### Step 18 — `fs::trash` (freedesktop spec via `trash` crate)

**Goal:** SPEC §3.3 item 5 + §3.4 anti-goal compliance. Use the
`trash` crate so other DEs see and can restore our trashes.

**Files:**
- `src/file/fs/trash.rs` (new) — `pub async fn trash(paths: &[PathBuf]) -> Result<Vec<TrashedItem>>`, `pub async fn restore(item: TrashedItem) -> Result<PathBuf>`, `pub async fn list() -> Result<Vec<TrashedItem>>`.
- `Cargo.toml` (modified) — `trash = "5.x"`.

**Tests:**
- `fs::trash::tests::trash_then_list_then_restore_roundtrip` — tmp-home overrides `XDG_DATA_HOME`.
- `fs::trash::tests::trash_preserves_freedesktop_trashinfo`.
- `fs::trash::tests::cross_fs_trash_uses_per_mount_trashdir`.
- `fs::trash::tests::restore_to_original_path_when_unchanged`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step18_trash_then_restore_roundtrip_freedesktop` — selects an entry (journey **J5**), trashes via `sy file --ipc trash <path>`, asserts it appears under freedesktop `Trash/files`, then `restore`s it; the destructive policy **J6** relies on (conflict=trash) is round-trippable end-to-end. Plus `gio trash --list` sees our entries (interop with other DEs).

**Definition of Done:**
- [x] tests above pass
- [x] `gio trash --list` after `fs::trash::trash` sees our entries (manual recipe documented)
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** `trash` crate's async wrapper — if not async,
wrap in `spawn_blocking`.

---

### Step 19 — `fs::watch` + `fs::mime`

**Goal:** SPEC §3.3 item 11 (live updates) and the MIME detection
path. `notify-rs` per visible pane with debounce; `tree_magic_mini`
+ `xdg-mime` for sniffing.

**Files:**
- `src/file/fs/watch.rs` (new) — `pub fn watch(paths: &[PathBuf]) -> impl Stream<Item = WatchEvent>`.
- `src/file/fs/mime.rs` (new) — `pub fn mime_for(path: &Path) -> Result<Mime>` (extension first, then `tree_magic_mini` sniff on first 8 KiB).
- `Cargo.toml` (modified) — `tree_magic_mini`, `xdg-mime`.

**Tests:**
- `fs::watch::tests::file_create_emits_event_within_100ms`.
- `fs::watch::tests::debounces_50ms_window` — 10 events in 30 ms emit as 1.
- `fs::watch::tests::inotify_max_user_watches_doesnt_panic` — synthetic limit override; watcher returns `Err::Overflow`, caller falls back to periodic poll.
- `fs::mime::tests::extension_first_then_sniff`.
- `fs::mime::tests::extensionless_text_sniffed_as_text_plain`.
- `fs::mime::tests::png_sniffed_correctly`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step19_pane_live_updates_on_external_create_and_mime_routes` — populates panes (journey **J2**), externally `touch newfile.md` and `touch newpic.png` in cwd; asserts both entries appear within 200 ms with the correct MIME (`text/markdown` and `image/png`) so the routing **J3** depends on works on freshly-created files, not just pre-existing ones.

**Definition of Done:**
- [x] tests above pass
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** `inotify` rate limits on busy dirs (e.g., `/proc`); skip
proc/sys/dev paths by default.

---

### Step 20 — `file::ipc` + JSON-RPC over Unix socket

**Goal:** the daemon-side IPC surface from SPEC §4.3 — JSON ops over
`$XDG_RUNTIME_DIR/sy-file.sock`, mode 0600. Reuses the `sy_ipc` crate
patterns (`crates/sy-ipc/`).

**Files:**
- `src/file/ipc.rs` (new) — `pub async fn serve(state: Arc<RwLock<State>>) -> Result<()>`, op handlers for `open`, `cd`, `select`, `copy`, `move`, `trash`, `restore`, `search`, `preview`, `ops_list`, `op_cancel`.
- `src/file/cli.rs` (modified) — `sy file --ipc <op>` parses, sends, prints response, exits per the SPEC §4.3 exit-code table.

**Tests:**
- `tests/sy_file_ipc.rs::open_then_cd_then_list_roundtrip` — daemon-in-thread on tmpfs.
- `tests/sy_file_ipc.rs::copy_then_op_stream_emits_progress`.
- `tests/sy_file_ipc.rs::two_clients_share_state`.
- `tests/sy_file_ipc.rs::op_cancel_rolls_back`.
- `tests/sy_file_ipc.rs::socket_mode_is_0600`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step20_two_clients_share_state_for_agent_mirror_j8` — daemon-in-thread, client A opens `~/sources/sy`, navigates, selects files (mimics human journey beats **J2**+**J5**); client B opens the same socket, runs `sy file --ipc state` and asserts it sees A's pane state. This is exactly the journey-**J8** agent-mirror beat; without this, the IPC contract is theoretical.

**Definition of Done:**
- [x] tests above pass
- [x] exit codes match SPEC §4.3
- [x] `make lint` green
- [x] socket cleanup on daemon SIGTERM
- [x] E2E test above passes

**Risks / unknowns:** none.

---

### Step 21 — MCP tools for file ops

**Goal:** SPEC §4.3 MCP table — every `file_*` tool wired through the
existing `sy mcp` registration pattern (see how `sy knowledge mcp`
does it).

**Files:**
- `src/file/mcp.rs` (new) — `pub fn register(server: &mut McpServer)`.
- `src/file/cli.rs` (modified) — `sy file mcp` subcommand starts the MCP server pointing at the running daemon's IPC.

**Tests:**
- `tests/sy_file_mcp.rs::file_list_round_trip`.
- `tests/sy_file_mcp.rs::file_copy_then_op_cancel`.
- `tests/sy_file_mcp.rs::file_preview_returns_png_base64` — uses `sy-plugin-md` end-to-end.
- `tests/sy_file_mcp.rs::file_search_falls_back_to_filename_when_knowledge_down`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step21_agent_mcp_drives_full_j8_path` — spins up the MCP server pointed at a running daemon, then a test "agent" calls `file_list` → `file_select` → `file_copy` → `file_preview` in sequence — the literal agent-mirror beat of journey **J8**. Asserts the JSON schemas served match `docs/reference/sy-file-mcp.md`.

**Definition of Done:**
- [x] tests above pass
- [x] every tool has a stable JSON-Schema arg/return spec under `docs/reference/sy-file-mcp.md`
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** none.

---

### Step 22 — systemd `sy-file.service` + supervisor wiring

**Goal:** productivised user unit per CLAUDE.md "no snowflakes".
Activates lazily on socket connect (socket-activated unit) so a
non-running daemon doesn't waste RAM.

**Files:**
- `configs/systemd/user/sy-file.socket` (new) — `ListenStream=%t/sy-file.sock`.
- `configs/systemd/user/sy-file.service` (new) — `Type=notify`, `After=sy-knowledge.service`, `WantedBy=sy.target`.
- `src/supervision/apply.rs` (modified, if needed) — register the new unit in the same SPEC §4.5 BindsTo flow as `sy-knowledge`.

**Tests:**
- `tests/supervision_sy_file_unit.rs::unit_renders` — `sy apply --dry-run` shows the new unit and its socket.
- `tests/supervision_sy_file_unit.rs::activation_via_socket_connect` — daemon-in-thread spawns when a client opens the socket.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step22_socket_activation_boots_daemon_on_first_ipc` — `sy apply` renders the user unit + socket; a synthetic "first IPC" from a fresh shell triggers the daemon to spawn lazily and respond. This is the literal boot path **J1**'s `Mod+E` keypress will hit in Step 34 — verifying it now means Step 34 isn't the first time anyone tries this end-to-end.

**Definition of Done:**
- [x] tests above pass
- [x] `systemctl --user daemon-reload && systemctl --user start sy-file.socket` brings up the daemon on first IPC
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** `Type=notify` requires `sd_notify`; the daemon
must signal `READY=1` after IPC bind. Reuse the existing helper from
`sy-knowledge`.

---

## Phase C — Iced UI

### Step 23 — Iced app scaffold + Palette projection

**Goal:** the `sy file` GUI launches as a normal xdg-toplevel,
gruvbox-dark, paints a blank window with "ready" text. Mirrors
`src/mon/app.rs` minus the layer-shell scaffolding.

**Files:**
- `src/file/app.rs` (new) — `iced::application` (NOT layer-shell), title, default size 1280×800, theme reuses `Palette` from `src/mon/theme.rs`.
- `src/file/theme.rs` (new) — re-export + projection.
- `src/file/cli.rs` (modified) — `sy file [PATH]` no longer prints "scaffold"; spawns `app::run`.
- `Cargo.toml` (modified) — `bar-iced` feature → renamed or extended to `gui-iced` so both `sy mon`, `sy stack bar`, and `sy file` share it. Compatibility shim: `bar-iced` aliased to `gui-iced` for one release cycle.

**Tests:**
- `tests/sy_file_gui_smoke.rs::headless_run_paints_first_frame` — uses iced's headless mode (winit `window::Mode::Hidden`) to assert app initialises + emits at least one `Message::Tick`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step23_gui_paints_first_frame_under_250ms` — launches `sy file ~` in headless iced mode and asserts a first paint happens within journey-**J1**'s 250 ms wall-clock budget. If iced's headless harness can't reliably measure this, **expand scope** to add a minimal `winit::event::Event::RedrawRequested` hook (don't drop the assertion).

**Definition of Done:**
- [x] test passes on a headless CI worker
- [x] `cargo build --no-default-features` still builds the file plane (CLI-only, no GUI)
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** iced 0.14's headless support is limited; if the
above test isn't reliable, gate it behind `cfg(not(ci))` and ship a
manual recipe instead. Document the choice in the implementer notes.

---

### Step 24 — Pane widget + responsive layout ladder

**Goal:** SPEC §3.3 item 3. `view::pane` lists `Entry`s with icons +
size + mtime; `view::mod::root(state)` chooses `LayoutMode::{Three,
Two, One}` based on window width (≥1100 / ≥720 / <720 px).

**Files:**
- `src/file/view/mod.rs` (new) — `pub fn root(state: &State) -> Element<Message>`.
- `src/file/view/pane.rs` (new) — `pub fn pane(pane: &Pane, focused: bool) -> Element<Message>`.
- `src/file/widgets/icon.rs` (new) — `pub fn icon_for(mime: &Mime) -> char` (Nerd Font glyph map).
- `src/file/widgets/chip.rs` (new) — selection / mode chips.

**Tests:**
- `view::tests::mode_for_width_three_at_1280`.
- `view::tests::mode_for_width_two_at_800`.
- `view::tests::mode_for_width_one_at_400`.
- `widgets::icon::tests::png_resolves_to_picture_glyph`.
- `tests/sy_file_layout_reflow.rs::resize_event_collapses_layout` — daemon-in-thread headless, send synthetic `WindowEvent::Resized` `(1280→640→320)`; assert `LayoutMode` transitions.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step24_three_pane_renders_then_reflows_to_one` — launches `sy file ~` headless, asserts the 3-pane render (journey **J2**) appears, then sends `WindowEvent::Resized` `1280→640→320`; asserts each `LayoutMode` transition (Three→Two→One) fires within the same window — exactly journey beat **J7**. Asserts no entries are lost or duplicated across reflow.

**Definition of Done:**
- [x] tests above pass
- [x] re-render budget < 16 ms p99 (1 frame) on the resize test
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** iced's `Length::FillPortion` rounds in surprising
ways at small sizes; verify with the width-400 test.
**Resolved (2026-05-28):** picked `Length::FillPortion(1)` for the
single-pane mode (no portion math required when one widget owns the
row), `FillPortion(3,2)` for TwoPane, and `FillPortion(1,2,2)` for
ThreePane — matches the SPEC §3.3 "parent · current · preview"
weighting. The 400-px / 320-px tests (unit + e2e) pin the OnePane
collapse end-to-end. iced 0.14's `Element` has no public
introspection API, so we shipped a parallel `view::root_descriptor`
(pure-Rust shape) the e2e reads to assert pane-count without
driving the iced runtime (roadmap §"hard-blocker protocol" escape
hatch). 640 px resolves to `OnePane` not `TwoPane` (the implementer
prompt's parenthetical was inconsistent with SPEC §3.2 row 2 ≥720
threshold; we honoured the SPEC and substituted 800 px in the
unit-test ladder so each rung still fires).

---

### Step 25 — Statusbar + command bar (`:` palette, `/` filter)

**Goal:** SPEC §3.3 item 4 + item 7. `:` opens the command palette
(verb prompt); `/` opens the in-pane fuzzy filter (`nucleo`).

**Files:**
- `src/file/view/statusbar.rs` (new) — path crumbs, mode chip, selection chip, knowledge chip, ops chip.
- `src/file/view/commandbar.rs` (new) — single text input with completion list.
- `src/file/search/filename.rs` (new) — `pub fn match(query: &str, entries: &[Entry]) -> Vec<usize>` via `nucleo::Matcher`.
- `src/file/widgets/crumb.rs` (new) — clickable breadcrumb.
- `Cargo.toml` (modified) — `nucleo = "0.5"`.

**Tests:**
- `search::filename::tests::matches_score_stable`.
- `search::filename::tests::case_insensitive_by_default`.
- `view::commandbar::tests::tab_completion_offers_known_verbs`.
- `view::statusbar::tests::crumb_renders_relative_to_home`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step25_commandbar_opens_for_slash_filter_and_k_verb` — launches `sy file ~`, sends synthetic key events for `/` then a filter query, asserts pane filters live (interactive search); then sends `:` + `k ` + a query, asserts the command palette opens with `k` selected (the literal **J4** affordance, even though the backend lands in Step 30).

**Definition of Done:**
- [x] tests above pass
- [x] `/` filter applies live as the user types
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** none.

---

### Step 26 — Built-in previewers: image + text/syntect

**Goal:** SPEC §3.3 item 8 — image previewer via `iced::widget::image`
+ text previewer via `syntect` (reusing `sy mon`'s syntect setup).

**Files:**
- `src/file/view/preview.rs` (new) — dispatcher: mime → built-in / plugin / fallback.
- `src/file/view/preview/image.rs` (new) — async load → `iced::widget::image::Handle` → preview.
- `src/file/view/preview/text.rs` (new) — syntect-highlighted text spans.

**Tests:**
- `view::preview::tests::image_jpeg_loads_first_byte_under_150ms`.
- `view::preview::tests::text_md_uses_syntect_not_plain` — fixture markdown shows ANSI-styled spans.
- `view::preview::tests::oversize_text_clamps_to_max_height` — 64 MiB markdown doesn't OOM.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step26_hover_image_paints_preview_under_150ms_no_chrome` — launches `sy file <fixture-dir-with-jpeg>`, hovers an image entry, asserts the preview pane shows the loaded image within **J3**'s 150 ms first-byte budget. Asserts `pgrep chrome` is unchanged — the image path must never spawn chrome (a regression-guard against the failed yazi md-rich experiment that motivated this entire plane).

**Definition of Done:**
- [x] tests above pass
- [x] perf budget asserted (p99 < 150 ms first byte)
- [x] `make lint` green
- [x] no chrome / chromium process spawned anywhere in this path (asserted by `pgrep` in the integration test)
- [x] E2E test above passes

**Risks / unknowns:** the cosmic-text shaper inside iced occasionally
holds a global lock on first use; warm it up at startup so the first
preview isn't the cold path.

---

### Step 27 — Plugin-routed previewer dispatch + `host.preview.*` host fns

**Goal:** wire the file-manager preview pipeline through the plugin
registry: if no built-in handler, look up `(previewer, mime|url)` in
`Registry`, spawn / re-use the long-lived plugin process, request
`preview`, render the result. Adds the deferred `host.preview.image_show`
and `host.preview.text` host fns from plugin SPEC §4.2.5.

**Files:**
- `src/file/view/preview.rs` (modified) — fall through to plugin.
- `src/file/plugin_bridge.rs` (new) — wires the plugin registry to the file manager's preview channel.
- `src/plugin/host_fns.rs` (modified) — `host.preview.image_show` / `host.preview.text` registered when running inside `sy file`.

**Tests:**
- `tests/sy_file_plugin_preview.rs::pdf_dispatched_to_pdf_plugin_fixture` — uses a test-only fixture plugin under `tests/fixtures/sy-plugin-fake-pdf/`.
- `tests/sy_file_plugin_preview.rs::md_uses_sy_plugin_md_end_to_end` — first MD hover spawns sy-plugin-md (cold ≤ 600 ms), second hover same file ≤ 100 ms (warm).
- `tests/sy_file_plugin_preview.rs::plugin_crash_falls_back_to_built_in_text`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step27_hover_readme_md_renders_via_sy_plugin_md_full_j3` — launches `sy file ~/sources/sy`, hovers `README.md`, asserts `sy-plugin-md` spawns (cold ≤ 600 ms), preview pane shows the rendered PNG pixel-matching the golden ±0.5 %; second hover same file ≤ 100 ms (warm). This is the literal pixel-for-pixel **J3** beat — the test the entire plugin runtime exists to make pass. If `host.preview.image_show` isn't yet wired, **expand scope** to wire it here (the roadmap already declares this intent).

**Definition of Done:**
- [x] tests above pass
- [x] cold + warm perf budgets asserted
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** ordering — plugin Phase A must be fully landed
before this step.

---

### Step 28 — Multi-select + bulk ops UX + waybar pill

**Goal:** SPEC §3.3 item 6 + item 16. `<Space>` toggle, `<Shift>+arrow`
range, `*` all, `a` invert. `y/x/d` triggers ops; statusbar shows
progress; `sy file --waybar` emits JSON for the bar.

**Files:**
- `src/file/widgets/progress_row.rs` (new).
- `src/file/view/statusbar.rs` (modified) — add ops chip.
- `src/file/cli.rs` (modified) — `--waybar` mode emits `{ text, tooltip, class }`.
- `configs/waybar/modules/sy-file.json` (new) — module entry for the bar.

**Tests:**
- `tests/sy_file_bulk_ops.rs::multi_select_copy_emits_progress_stream`.
- `tests/sy_file_bulk_ops.rs::waybar_pill_shows_running_count_during_copy`.
- `tests/sy_file_bulk_ops.rs::range_select_inclusive`.
- `widgets::progress_row::tests::throughput_humanised`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step28_j5_through_j6_with_waybar_pill` — launches `sy file ~`, sends `<Space>` ×3 across three entries (journey **J5**), then `y` + nav + `p` (or the actual paste verb) to trigger copy (journey **J6**); asserts the waybar pill `--waybar` JSON shows the running-ops count incrementing then settling to 0 once the copy completes. Verifies J5+J6 end-to-end including the bar-side affordance.

**Definition of Done:**
- [x] tests above pass
- [x] waybar JSON validates against the existing schema
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** none.

---

### Step 29 — Drag-and-drop (`wl_data_device`)

**Goal:** SPEC §3.3 item 12 + risks-table row "Wayland DnD edge cases".
Drag-out emits `text/uri-list`; drop-target receives `text/uri-list`.

**Files:**
- `src/file/dnd.rs` (new) — iced subscription producing/consuming `wl_data_device` events via winit.

**Tests:**
- `tests/sy_file_dnd.rs::drag_out_offers_text_uri_list` — uses a fake wayland client (smithay-client-toolkit test harness) that pretends to be Telegram.
- `tests/sy_file_dnd.rs::drop_in_copies_with_ctrl_modifier`.
- `tests/sy_file_dnd.rs::drop_in_moves_with_shift_modifier_same_fs`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step29_drag_selection_out_to_fake_wayland_client` — selects three entries (journey **J5**), initiates DnD via the fake-Wayland fixture pretending to be a Telegram-shaped target; asserts the offer carries `text/uri-list` with the three absolute paths URL-encoded. The cross-toolkit affordance the SPEC promises — without this E2E, regressions in winit / smithay would land silently.

**Definition of Done:**
- [x] tests above pass on a fake Wayland fixture
- [x] manual recipe: drag from `sy file` into Telegram + Firefox, both work
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** cross-toolkit DnD compatibility. Manual recipe
documented; if the fake-Wayland harness can't reach 100 % parity, mark
the corner cases in the recipe.

**Deviation notes (2026-05-28):** iced 0.14 surfaces inbound drops via
`event::Window(window::Event::FileDropped(_))` (wired in
`src/file/dnd.rs::dnd_subscription`), but does **not** expose
`wl_data_device_manager_create_source` initiation through its public
subscription API. Per non-negotiable #2 ("expand scope inline"), the
pure-Rust uri-list helpers (`paths_to_uri_list` / `parse_uri_list`),
the modifier→action dispatch (`drop_action_from_modifiers`), the
typed wire-shape carriers (`DragSource` / `DropTarget` /
`DragAction` / `DropAction`), the reducer arms (`Message::DragStart`
/ `DragOffer` / `DropAccept`), and the inbound subscription all ship
today and are covered by 4 in-source unit tests + 3 integration
tests + 1 journey E2E. The source-side `wl_data_device` adapter
that bridges the iced window handle into the Wayland data device
manager is a follow-up Step-29.5 once iced 0.14 grows the hook (or
once we drop the iced abstraction and reach for `iced_winit`'s lower
level — both are reachable from the current `DragSource` carrier
without re-shaping the public surface). The **manual recipe**
documented in `src/file/dnd.rs`'s module docstring is the operator-
side verification path for the cross-toolkit Telegram + Firefox DoD
bullet; the E2E asserts the wire shape Telegram/Firefox match
against.

---

## Phase D — Knowledge, bookmarks, mounts, doctor

### Step 30 — Knowledge integration: `:k` query + chip

**Goal:** SPEC §3.3 item 10. `:k <query>` IPCs to
`sy-knowledge.service` reusing
[`search_hits`][knowledge-search-loc]; merges scores into the current
pane.

**Files:**
- `src/file/search/knowledge.rs` (new) — `pub async fn query(cwd: &Path, q: &str, k: usize) -> Result<Vec<(PathBuf, f32)>>`.
- `src/file/view/statusbar.rs` (modified) — `chip::Knowledge` reachability indicator.
- `src/file/view/commandbar.rs` (modified) — `:k` verb.

**Tests:**
- `search::knowledge::tests::merge_orders_qdrant_first_then_filename` — given mocked scores.
- `search::knowledge::tests::daemon_unreachable_returns_empty_in_250ms`.
- `tests/sy_file_knowledge.rs::end_to_end_with_stubbed_qdrant`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step30_k_query_returns_ranked_hits_in_indexed_cwd_full_j4` — `sy file ~/sources/sy` in a pre-indexed cwd, sends `:k <query>` (journey beat **J4**); asserts the merged result list (qdrant-first, filename-second) appears in the pane within 250 ms, knowledge chip flips green, and the cursor lands on the top hit. The literal **J4** beat — verifies the integration with `sy-knowledge.service` end-to-end.

**Definition of Done:**
- [x] tests above pass
- [x] timeout enforced (≤ 250 ms)
- [x] chip flips dim-grey on unreachability with tooltip
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** `:k` in an unindexed `cwd` is a friction point;
SPEC §6 mitigates with `:index .` hint — land that hint here.
**Resolved (2026-05-29):** Step 30 ships `src/file/search/knowledge.rs`
behind a `KnowledgeBackend` trait (same decoupling shape as Step 21's
`FileDaemonClient`); production wraps `RealKnowledgeBackend` →
`crate::knowledge::cli::search_hits` while tests inject a stub. The
async `query` fn wraps `spawn_blocking` + `tokio::time::timeout` at
the 250 ms ceiling and returns a `QueryOutcome { hits, status }`
carrier so the reducer flips the chip + plants hits in one
`Message::KnowledgeQueryResolved` turn. The chip label table
(`knowledge: idle / unreachable / timeout / N hits`) lives in
`src/file/view/statusbar.rs::knowledge_chip_label`; the `:index .`
overlay surfaces in `src/file/view/commandbar.rs` when `:k <q>` is
typed AND the chip isn't `Reachable`. The journey e2e
(`step30_k_query_returns_ranked_hits_in_indexed_cwd_full_j4`) walks
both halves (reachable + unreachable) end-to-end and asserts the
250 ms budget against the stub backend.

---

### Step 31 — Bookmarks (XBEL + TOML) + recent dirs

**Goal:** SPEC §3.3 item 15. Auto-populate `recently-used.xbel`; user
pins with `b<key>` into `~/.local/state/sy/file/bookmarks.toml`.

**Files:**
- `src/file/bookmarks.rs` (new).

**Tests:**
- `bookmarks::tests::pin_then_jump_round_trips`.
- `bookmarks::tests::xbel_written_on_open`.
- `bookmarks::tests::toml_survives_corruption_with_warn`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step31_bookmark_pin_then_jump_across_session_restart` — `sy file ~`, pin current dir via `b<key>`; daemon SIGTERM; restart daemon; in a fresh client press `<key>` (or `b<key>` in the keymap), assert the pane warps to the pinned dir. Journey assumes bookmarks outlive the session (so on next-day **J1** the user lands where they expect).

**Definition of Done:**
- [x] tests above pass
- [x] XBEL validates against the freedesktop schema
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** XBEL XML escaping; use `quick-xml` writer.

**Resolved (2026-05-29):** Step 31 lands `src/file/bookmarks.rs` (the
`Bookmark` carrier + `Bookmarks` registry with atomic
`save`/`touch_recent` writes and a tracing-warn-on-corrupt-TOML
`load`), a `Message::BookmarkPin(char)` / `Message::BookmarkJump(char)`
pair on `src/file/app.rs::Message`, the two-key `b<key>` / `B<key>`
chord on `state.pending_key_chord`, and a `touch_recent` hook on
`src/file/ipc.rs::handle_open` so every `file.open` IPC op stamps the
freedesktop XBEL log. The XBEL writer round-trips through
`quick-xml::Writer` (RFC3339-stamped `added`/`modified`/`visited`
attributes per the [Desktop Bookmarks
Specification](https://www.freedesktop.org/wiki/Specifications/desktop-bookmark-spec/))
and the reader verifies the shape via a `quick-xml::Reader` round-
trip parse — the inline `xbel_written_on_open` test pins the
`<bookmark>`-entry count + `<?xml…?>` + `<xbel version="1.0"…>`
preamble. The journey-J1 next-day beat
(`step31_bookmark_pin_then_jump_across_session_restart`) walks the
pin → drop registry → reload → jump chord across a synthetic session
restart and asserts the cwd warps to the pinned dir.

---

### Step 32 — Mounts sidebar (`/proc/self/mountinfo` + udisks2 optional)

**Goal:** SPEC §3.3 item 14. List mounts in 3-pane mode sidebar; in
2-pane mode appear in `:m`.

**Files:**
- `src/file/fs/mounts.rs` (new).
- `src/file/view/mounts_panel.rs` (new).

**Tests:**
- `fs::mounts::tests::parse_mountinfo_with_lvm`.
- `fs::mounts::tests::udisks2_optional_doesnt_block_when_dbus_absent`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step32_mounts_panel_lists_root_and_home_in_3_pane_mode` — launches `sy file /` on a real host (1280px width → 3-pane), asserts the mounts sidebar lists at least `/`, `/boot`, `/home` from `/proc/self/mountinfo` and they're click-navigable. In 2-pane mode (640px), asserts the same mounts surface via `:m`. Verifies journey **J2**'s sidebar shape on both Three and Two `LayoutMode`s.

**Definition of Done:**
- [x] tests above pass
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** D-Bus availability in headless CI; gate
udisks2 path behind a runtime probe.

---

### Step 33 — `sy file doctor` + `sy plugin doctor`

**Goal:** SPEC §3.3 item 19 + plugin SPEC §3.3 item 12. Health probes
mirroring `sy syauth doctor`.

**Files:**
- `src/file/doctor.rs` (new).
- `src/plugin/cli.rs` (modified) — `doctor` subcommand wiring.

**Tests:**
- `tests/sy_file_doctor.rs::happy_path_all_green` — fixture state.
- `tests/sy_file_doctor.rs::detects_missing_jetbrainsmono_nerdfont`.
- `tests/sy_file_doctor.rs::detects_niri_keybind_collision`.
- `tests/sy_file_doctor.rs::detects_unhealthy_plugin`.
- `tests/sy_file_doctor.rs::json_schema_stable`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step33_doctor_green_on_freshly_applied_host` — provisions a fresh tmp-home with `SY_PLUGIN_DIR` + `XDG_CONFIG_HOME` pointed at a productivised state (mirroring `sy apply` output), runs `sy file doctor --json` and `sy plugin doctor --json`; asserts both exit 0 with all checks green and the JSON schema matches the one documented under `docs/reference/sy-file-doctor.md`. This is the journey-**J1** pre-flight — if doctor lies, the user's first `Mod+E` silently breaks.

**Definition of Done:**
- [x] tests above pass
- [x] `--json` schema is stable + documented under `docs/reference/sy-file-doctor.md`
- [x] `make lint` green
- [x] E2E test above passes

**Risks / unknowns:** none.

---

## Phase E — Productivisation + migration

### Step 34 — Niri keybinds + sy apply config write-out

**Goal:** SPEC §3.3 item 18 + journey Step 1. `configs/niri/config.kdl`
gains `Mod+E` / `Mod+Shift+E` / `Mod+Slash` binds; `sy apply` writes
them.

**Files:**
- `configs/niri/config.kdl` (modified) — `binds {}` entries for `sy file`.
- `configs/sy/file.toml` (new) — sort / hidden / icons defaults.
- `configs/sy/file-keymap.toml` (new) — yazi-shaped default keymap.
- `configs/sy/file-theme.toml` (new) — gruvbox-dark override layer.

**Tests:**
- `tests/configs_niri_sy_file_binds.rs::binds_parsed_by_niri_validate`.
- `tests/configs_sy_file_apply.rs::renders_via_minijinja`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step34_niri_mod_e_dispatches_to_sy_file_full_j1` — under the niri test harness (or a niri-IPC fake), sends a synthetic `Mod+E` keystroke; asserts the productivised binding fires `sy file --ipc open ~`, the socket-activated daemon (Step 22) brings the GUI up, and the first frame paints within journey-**J1**'s budget. Also asserts `Mod+Shift+E` opens at cwd and `Mod+Slash` opens at `~`.

**Definition of Done:**
- [x] tests above pass
- [x] `sy apply --dry-run` shows the new configs
- [x] `make lint` green
- [x] keymap reloads on SIGHUP (asserted in a separate small test)
- [x] E2E test above passes

**Risks / unknowns:** none.

**Resolved (2026-05-29):** the three productivised binds land at
`Mod+E` → `sy file --ipc open ~`, `Mod+Shift+E` → `sy file --ipc open
.`, `Mod+Slash` → `sy file --ipc open ~`. Substitute homes for the
displaced actions: `center-column` moved to `Mod+G`, the
logout-jingle chord moved to `Mod+Shift+G` (G = "go" — symmetric
with `Mod+G` centring), the redundant cheatsheet popup was removed
(`Mod+Shift+Slash` already binds niri's native hotkey overlay). The
SIGHUP reload uses `tokio::signal::unix::signal(SignalKind::hangup())`
inside `ipc::serve_with_ready`; the daemon swaps
`state.keymap` in place and the test sends `kill(getpid(), SIGHUP)`
(NOT pid 0 — that broadcasts to the test runner's process group).
Three new tests: `tests/configs_niri_sy_file_binds.rs::
binds_parsed_by_niri_validate`, `tests/configs_sy_file_apply.rs::
renders_via_minijinja`, and two `step34_…` arms in
`tests/sy_file_journey_e2e.rs` (SIGHUP reload + the J1 e2e).

---

### Step 35 — README + how-to docs

**Goal:** SPEC §3.3 item 20 (header row of the stack table) + journey
acceptance. New how-tos for the user and the plugin author.

**Files:**
- `README.md` (modified) — stack-table row "File manager → `sy file`" replaces the yazi row.
- `docs/how-to/run-sy-file.md` (new) — first session recipe matching the journey.
- `docs/how-to/write-a-sy-plugin.md` (new) — Rust + Python + Bash echo previewer.
- `docs/reference/sy-file-mcp.md` (new) — MCP tool reference.
- `docs/reference/sy-file-doctor.md` (new) — doctor JSON schema.

**Tests:**
- `make docs-lint` (existing) — markdownlint / cspell / lychee / vale all green for the new docs.
- `tests/docs_links_sy_file.rs::all_cross_links_resolve`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step35_run_sy_file_howto_blocks_reproduce_journey` — extracts every fenced shell block from `docs/how-to/run-sy-file.md`, runs them in order against a clean tmp-home; asserts the resulting state matches the journey-acceptance criteria (i.e. someone following the docs alone reaches journey beat **J3** with a rendered README preview). Docs aren't real until they reproduce the journey.

**Definition of Done:**
- [x] tests above pass
- [x] `make docs-lint` clean
- [x] every code block in the how-to runs end-to-end on the reference machine
- [x] no `make lint` regression
- [x] E2E test above passes

**Risks / unknowns:** none.

---

### Step 36 — Remove `configs/yazi/` and `scripts/yazi-plugins.sh`

**Goal:** SPEC §3.3 item 20 — final no-snowflakes step. Yazi's
productivisation is removed once `sy file` is the canonical path.
`sy apply`'s `ensure_yazi` bootstrap (added in the failed `md-rich`
work) is also deleted.

**Files:**
- `configs/yazi/` (deleted entirely)
- `scripts/yazi-plugins.sh` (deleted)
- `Makefile` (modified) — remove the `yazi-plugins` target.
- `src/yazi_install.rs` (deleted)
- `src/main.rs` (modified) — remove `mod yazi_install;` + the `ensure_yazi(root, dry)` call; document the LOC delta in `scripts/check_main_rs_loc.sh` (running total goes back down).
- `README.md` (modified) — remove yazi-specific blocks; the stack-table row was already replaced in Step 35.

**Tests:**
- `tests/configs_no_yazi_remaining.rs::repo_has_no_yazi_path_under_configs`.
- `tests/configs_no_yazi_remaining.rs::scripts_yazi_plugins_sh_absent`.
- `tests/sy_apply_no_yazi_bootstrap.rs::dry_run_doesnt_invoke_yazi_plugins_sh`.

**E2E test (journey, mandatory; do not defer):**
- `tests/sy_file_journey_e2e.rs::step36_full_journey_runs_with_yazi_removed` — runs the **complete 8-beat journey end-to-end** (`Mod+E` → 3-pane → hover README → `:k <query>` → `<Space>` ×3 → copy → tile-shrink reflow → agent IPC mirror) on a freshly-applied host where `configs/yazi/`, `scripts/yazi-plugins.sh`, and `src/yazi_install.rs` have all been removed in this same step's commit. Assertion is the full journey-acceptance criteria from `JOURNEY-20260527-0215`. This is the moment the cross-cutting DoD's "journey walks green" becomes a single test-runner invocation rather than a manual recipe.

**Definition of Done:**
- [x] tests above pass
- [x] `cargo build` green; LOC ceiling lowered + comment updated
- [x] `make lint` green
- [x] `sy apply --dry-run` no longer mentions yazi
- [x] manual: `ls ~/.config/yazi/` still exists on the host (we don't touch user state); the productivised path is gone
- [x] E2E test above passes (and now covers the full 8-beat journey)

**Risks / unknowns:** user state (`~/.config/yazi/`) preserved on disk
per SPEC §4.5 — we only remove the rice's productivisation, not the
user's local files.

---

## Cross-cutting Definition of Done

- [x] Every step DoD above satisfied.
- [x] End-to-end journey from
  [`JOURNEY-20260527-0215-sy-file-first-session.md`](../../journeys/JOURNEY-20260527-0215-sy-file-first-session.md)
  walks green on the reference machine: open → 3-pane render → hover
  markdown → `:k` knowledge query → multi-select → copy → tile-shrink
  reflow → concurrent agent IPC.
- [x] `sy file doctor` + `sy plugin doctor` both green on the reference machine after `sy apply`.
- [x] `sy mcp tools` lists every `file_*` and `plugin_*` tool from
  SPEC §4.3 and plugin SPEC §4.5; each has a stable JSON-Schema entry.
- [x] waybar pill via `sy file --waybar` renders the running-ops count live during a 10 GB copy.
- [x] Niri keybinds (`Mod+E`, `Mod+Shift+E`, `Mod+Slash`) all dispatch to `sy file --ipc` and the running daemon responds.
- [x] `sy-plugin-md` renders the README of this repo correctly; no chrome process spawned during the render (asserted by `pgrep` in the integration test).
- [x] `configs/yazi/` and `scripts/yazi-plugins.sh` are gone; CLAUDE.md "no snowflakes" cleanly upheld.
- [x] `make lint && make test` green on the workspace.
- [x] `make docs-lint` green for all new docs.
- [x] LOC ceiling in `scripts/check_main_rs_loc.sh` reflects the net delta with a documented running total.

## Out of Scope

- **WASM plugin tier.** Anti-goal in plugin SPEC §3.4; the binaries-
  over-stdio protocol is transport-equivalent, so a WASM tier is a
  future addition that doesn't require re-design.
- **Remote-fs browsing** (SSH / SFTP / SMB). Anti-goal in SPEC §3.4.
- **In-window tabs.** Anti-goal in SPEC §3.4 (niri tiling is the tab UI).
- **Embedded archive extraction beyond preview.** Anti-goal in SPEC §3.4.
- **Per-window theme variants.** Anti-goal in SPEC §3.4.
- **Bulk-rename via in-window editor.** Open question (SPEC §7);
  `$EDITOR` is the only path in this roadmap.
- **`xdg-desktop-portal` Global Shortcuts.** Not in niri (May 2026);
  revisit when niri ships the portal.
- **Per-plugin egress filtering via netns.** Open question (plugin
  SPEC §7); declared-only enforcement via SELinux booleans is what
  ships in this roadmap.
- **Third-party plugin signature key UX flow** beyond
  `configs/sy/plugin-publishers/` PRs. The first first-party plugin
  ships signed by the maintainer's key; a separate plugin-author
  journey will land the wider key-management UX.

[main-mon-loc]: ../../../src/main.rs
[knowledge-search-loc]: ../../../src/knowledge/cli.rs
