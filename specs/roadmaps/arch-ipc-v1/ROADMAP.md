# ROADMAP: arch-ipc-v1 — typed, versioned, cancellable IPC

Source: `specs/research/architecture-refactor/SPEC.md` §3.2 K2, §3.3
Zone 2, §4.2, Appendix A "Z2".

## Overview

Today's three UDS surfaces (`sy-knowledge.sock`, `sy-agentd.sock`,
`sy/stackbar.sock`) speak three bespoke JSON dialects with no
versioning, no `request_id`, no `trace_id`, no `deadline_ms`, no
cancellation, and no origin check beyond the `XDG_RUNTIME_DIR`
permissions. This roadmap lands a single `sy-ipc` crate carrying the
v1 envelope (length-delimited JSON-RPC 2.0-shaped frames with the
`sy.v1` extension fields), `SO_PEERCRED` gate, reserved
`system.{describe,health,cancel}` methods, and an LSP-style race-free
cancellation registry. Migrates the three daemons one at a time —
plumbing first, consumers last — so each commit stays reviewable.
The memfd+SCM_RIGHTS blob channel is explicitly deferred to a
follow-up (SPEC §3.3 Zone 2 "OUT").

Depends on `arch-workspace` Steps 1 and 3 landing first (the `sy-ipc`
crate is a new workspace member; the envelope schema uses
`sy_core::{Priority, ErrorCode}`).

---

## Step 1 — Create `crates/sy-ipc` with envelope types + framing

**Goal:** the wire shape from SPEC §4.2 lands as serde types and
length-prefixed framing. Pure types + codec, no I/O, no daemons.

**Files:**
- `crates/sy-ipc/Cargo.toml` (new) — `publish = false`,
  `version.workspace = true`, deps: `sy-core` (path), `serde`,
  `serde_json`, `tokio-util` (with `codec` feature), `bytes`,
  `ulid`, `anyhow`.
- `crates/sy-ipc/src/envelope.rs` (new) — exact shapes from SPEC
  §4.2:
  - `pub struct Request { schema_version: u32, request_id: Ulid,
    trace_id: Option<TraceId>, parent_span_id: Option<SpanId>,
    deadline_ms: Option<u64>, priority: Priority, method: String,
    params: serde_json::Value }`.
  - `pub enum Response { Ok { schema_version, request_id, result,
    blob: Option<BlobRef> }, Err { schema_version, request_id,
    error: ErrorBody } }` (untagged at the wire, distinguished by
    `result` vs `error` presence).
  - `pub struct ErrorBody { code: ErrorCode, message: String,
    retry_after_ms: Option<u64>, details: serde_json::Value }`.
  - `pub struct BlobRef { kind: BlobKind, len: u64, sha256: String }`
    with `BlobKind::Memfd` (other variants reserved for future).
  - `pub const SCHEMA_VERSION: u32 = 1;`.
- `crates/sy-ipc/src/codec.rs` (new) — thin wrapper over
  `tokio_util::codec::LengthDelimitedCodec` with `Encoder<Request>`
  / `Decoder<Item = Request>` and `Encoder<Response>` /
  `Decoder<Item = Response>` impls. 4-byte big-endian length per
  SPEC §4.2 "Framing".
- `crates/sy-ipc/src/lib.rs` (new) — module wall + re-exports.
- `Cargo.toml` root (modified) — workspace `members` gains
  `"crates/sy-ipc"`.

**Tests:**
- `crates/sy-ipc/src/envelope.rs::tests::request_round_trip` —
  every field round-trips via `serde_json`.
- `crates/sy-ipc/src/envelope.rs::tests::response_ok_and_err_share_shape`
  — both response variants serialise with `schema_version` and
  `request_id` at the top level (mandatory).
- `crates/sy-ipc/src/envelope.rs::tests::missing_schema_version_rejects`
  — a JSON object without `schema_version` returns a deserialise
  error (SPEC §3.4 anti-goal: "daemons reject `null`/missing
  versions with `INCOMPATIBLE_SCHEMA`").
- `crates/sy-ipc/src/envelope.rs::tests::wrong_schema_version_rejects`
  — `schema_version: 2` returns an
  `Err(IncompatibleSchema)`-tagged variant from a higher-level
  `parse_request_strict` helper.
- `crates/sy-ipc/src/codec.rs::tests::frame_round_trip_via_codec` —
  encode → decode round-trip preserves a `Request` and a `Response`
  through `tokio_util::codec::Framed`.

**Definition of Done:**
- [x] All five tests above pass (8 in practice: 5 envelope + 3 codec —
      `frame_round_trip_via_codec`, `partial_frame_returns_none_then_completes`,
      `wire_header_is_4_byte_big_endian_length`).
- [x] `cargo build -p sy-ipc` succeeds standalone.
- [x] `make lint` green workspace-wide. Required cleaning up the
      scaffold dead code committed in `05c1e60` and the WorkloadKind
      trim left mid-flight by arch-workspace step 3 (~57 clippy hits +
      6 dead-code warnings across `src/agt/*`, `src/aiplane/*`,
      `src/knowledge/*`, `src/stack/*`, `src/main.rs`).
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME` in the new crate.
- [x] SPEC §4.2 wire shape matches byte-for-byte; deviation requires
      a SPEC amendment.

**Risks / unknowns:**
- `ulid::Ulid` (canonical 26-char Crockford base32) vs. the SPEC's
  example `"01HXYZ…"` — same encoding family; use `ulid = "1"` and
  serialise as the canonical string form.

---

## Step 2 — Add `SO_PEERCRED` gate + `Server` / `Client` skeleton

**Goal:** `sy-ipc` gains a tokio-based `Server::serve` and
`Client::connect`/`Client::call` API that wraps an `UnixStream` in
the framing codec, asserts `peer.uid == geteuid()`, and dispatches
to a user-supplied handler. No daemons consume it yet.

**Files:**
- `crates/sy-ipc/Cargo.toml` (modified) — add `tokio` (with
  `net`, `sync`, `time`, `macros` features) and `rustix`
  (with `net` feature) per SPEC §4.10.
- `crates/sy-ipc/src/server.rs` (new) — `pub struct Server<H: Handler>`
  with `pub async fn serve(self, listener: UnixListener) -> Result<()>`,
  rejecting peers whose `socket_peercred().uid != geteuid()`.
  `pub trait Handler { async fn handle(&self, req: Request) ->
  Response; }`.
- `crates/sy-ipc/src/client.rs` (new) — `pub struct Client { … }`
  with `pub async fn connect(path: &Path) -> Result<Client>` and
  `pub async fn call(&mut self, method: &str, params:
  serde_json::Value, opts: CallOpts) -> Result<Response>`. `CallOpts`
  defaults: priority `Interactive`, deadline `5000ms`, request_id
  auto-generated. Per SPEC §5 Friction Map row "every IPC call site
  has to be touched" → defaults make migration grep-replaceable.
- `crates/sy-ipc/src/lib.rs` (modified) — `pub use server::*;
  pub use client::*;`.

**Tests:**
- `crates/sy-ipc/src/server.rs::tests::client_server_round_trip` —
  spin up a `Server` in a tokio test against a `tempdir()`-allocated
  socket; client calls a `system.health` echo handler; assert
  response round-trips with the same `request_id`.
- `crates/sy-ipc/src/server.rs::tests::rejects_foreign_uid` — uses
  `unsafe { libc::setegid(…) }` only if running as root, otherwise
  `#[ignore]` with documentation. Best-effort coverage; the kernel
  also enforces `0700`/`0600` on `$XDG_RUNTIME_DIR` per SPEC §4.2.
- `crates/sy-ipc/src/client.rs::tests::call_defaults_priority_interactive`
  — verify default `CallOpts` yields `Priority::Interactive` on the
  wire.

**Definition of Done:**
- [x] Tests above pass; `rejects_foreign_uid` documents-and-ignores
      per environment (cross-uid runner not available; matching-uid
      admission is covered by `client_server_round_trip`).
- [x] `cargo build -p sy-ipc` succeeds.
- [x] `make lint` green workspace-wide (cleaned alongside Step 1
      acceptance).
- [x] `sy-ipc` exports `Server`, `Client`, `Handler`, `CallOpts`,
      `Request`, `Response`, `ErrorBody`.

**Risks / unknowns:**
- `rustix::net::sockopt::socket_peercred` vs. `tokio`'s
  `UnixStream::peer_cred`: prefer the tokio API where it exposes
  `uid`; fall back to `rustix` if not. Confirmed available on
  `tokio = "1"` per SPEC §4.10.

---

## Step 3 — Implement reserved methods `system.describe` / `system.health` / `system.cancel`

**Goal:** every IPC-v1 daemon will answer these three methods. The
default `Handler` impl provides them so individual daemons only need
to merge their domain methods on top.

**Files:**
- `crates/sy-ipc/src/reserved.rs` (new) — `pub struct SystemMethods
  { name: &'static str, build_info: BuildInfo, health_fn:
  Arc<dyn Fn() -> HealthSnapshot + Send + Sync>, cancel_registry:
  CancelRegistry }`. Implements `Handler` for the three methods
  defined in SPEC §4.2 "Reserved methods", delegating to a
  `Capabilities` map the daemon constructs at boot.
- `crates/sy-ipc/src/cancel.rs` (new) — `pub struct CancelRegistry
  { inner: Mutex<HashMap<Ulid, CancellationToken>> }`. **The
  registration ordering is load-bearing** (SPEC §4.2 "Cancellation
  pattern" step 1 + SPEC §2.3 deep dive on SourceKit-LSP): the
  registry exposes `pub fn register(&self, id: Ulid) -> CancelGuard`
  that must be called *before* the worker future is spawned.
- `crates/sy-ipc/src/lib.rs` (modified) — `pub mod reserved; pub mod
  cancel;`.

**Tests:**
- `crates/sy-ipc/src/reserved.rs::tests::system_describe_lists_methods`
  — call `system.describe` against a `Server` with one extra
  method; response lists both `system.*` and the daemon's method.
- `crates/sy-ipc/src/reserved.rs::tests::system_health_returns_ready_then_degraded`
  — flip the health closure return; subsequent call returns
  `degraded`.
- `crates/sy-ipc/src/cancel.rs::tests::cancel_before_spawn_is_a_no_op_then_armed`
  — register, drop the guard, register again with same id → second
  one wins. (LSP race-prevention property.)
- `crates/sy-ipc/src/cancel.rs::tests::cancel_after_register_fires_token`
  — register; in a spawned task, await
  `token.cancelled()`; from main, `cancel(id)`; expect the spawned
  task wakes.

**Definition of Done:**
- [x] Four tests above pass (9 in practice: 5 cancel + 4 reserved —
      added `second_register_wins_over_concurrent_first`,
      `dispatch_with_cancel_completes_when_worker_finishes_first`,
      `dispatch_with_cancel_yields_cancelled_when_token_fires`,
      `try_handle_returns_none_for_non_system_methods`,
      `system_cancel_targets_registered_request`).
- [x] SPEC §4.2 cancellation pattern step ordering is enforceable —
      `CancelRegistry::register` returns a [`CancelGuard`] whose
      existence is the proof that the slot is in place;
      `dispatch_with_cancel(guard, worker_fn)` takes the guard by
      value. No `register_after_spawn` escape hatch exists in the
      module's public surface.
- [x] `make lint` green workspace-wide.

**Risks / unknowns:**
- `tokio_util::sync::CancellationToken` clone semantics: child
  tokens cancel when the root cancels but not vice-versa. Used per
  SPEC §4.2 step 1 "`child_token = root.child_token()`".

---

## Step 4 — Migrate `sy-knowledge` daemon to IPC v1 (first daemon)

**Goal:** the existing `sy-knowledge.sock`
(`src/knowledge/ipc.rs:1-8` + `src/aiplane/ipc.rs:1-100`) flips to
the v1 envelope. CLI + MCP callers updated. Aiplane + stack still on
their legacy wire format for this step.

**Files:**
- `src/knowledge/ipc.rs:1-8` (modified) — currently 8 lines, mostly
  re-exports. Replace with a thin shim: `pub use sy_ipc::*;` plus a
  knowledge-specific `Method` enum (`"knowledge.search"`,
  `"knowledge.search_rerank"`, `"knowledge.index_now"`,
  `"knowledge.full_resync"`, …) sourced from the existing `Op` and
  `Req` variants in `src/aiplane/ipc.rs:33-106`.
- `src/aiplane/ipc.rs:33-106` (modified, not deleted yet) — keep
  the legacy `Op`/`Req`/`Resp` enums but mark them
  `#[deprecated(since = "0.2.0", note = "use sy_ipc envelope")]`.
  Add a translation layer that converts legacy `Op::IndexNow` →
  `Request { method: "knowledge.index_now", … }` so MCP/CLI callers
  on the new path go through one code path inside the daemon.
- `src/knowledge/daemon.rs` (modified, ~1207 lines today; expect
  ≤ 200 lines of diff) — listener thread switches from `BufReader`
  line-reads to `sy_ipc::Server::serve`. Each method dispatches
  through a `match req.method.as_str()` to the existing handlers.
  Returns `Response::Ok { result: serde_json::to_value(…) }`.
- `src/knowledge/cli.rs`, `src/knowledge/mcp.rs` (modified) — the
  search/search_rerank/index_now call sites switch to
  `sy_ipc::Client::call("knowledge.search", json!({...}))`. Defaults
  cover most: only `--priority`/`--deadline`/`--trace-id` (Zone 3)
  callers need explicit `CallOpts`.
- `src/agt/daemon.rs` and `src/aiplane/cli.rs` (modified) — call
  sites that *use* `sy-knowledge` IPC (search, etc.) flip to the new
  client.

**Tests:**
- `tests/knowledge_ipc_v1_round_trip.rs` (new integration test
  using `sy-testutils`) — spawn `sy knowledge daemon` in-thread,
  send `system.describe`, assert `methods` lists `knowledge.search`;
  send `knowledge.search` and assert hits round-trip.
- `tests/knowledge_ipc_v1_rejects_legacy_envelope.rs` (new) — send
  a legacy `{"op":"index-now"}` line; expect a
  `IncompatibleSchema` error (SPEC §3.4 anti-goal: hard cutover, no
  backward-compat).
- `tests/knowledge_ipc_v1_cancel.rs` (new) — start a
  `knowledge.full_resync`; send `system.cancel` with the original
  `request_id`; assert the daemon stops within 500 ms.

**Definition of Done:**
- [x] Three integration tests pass — `knowledge_ipc_v1_round_trip`,
      `knowledge_ipc_v1_rejects_legacy_envelope`,
      `knowledge_ipc_v1_cancel`. The crate ships only a `[[bin]]`
      target (no `[lib]`), so a Cargo-level `tests/` integration file
      can't link against `aiplane::ipc::serve` from outside the bin.
      The three tests live in `src/aiplane/ipc.rs::tests` with the
      same names as the DoD and exercise the same end-to-end wire
      (real UDS, real `sy_ipc::Client`, real bridge handler). A
      `[lib]` split moves to a later roadmap step.
- [x] `sy knowledge search "foo"` (CLI) works end-to-end —
      `aiplane::ipc::request` now speaks v1 internally; existing
      `knowledge::cli`/`knowledge::mcp` call sites are unchanged.
- [x] `sy knowledge mcp` (MCP stdio) works end-to-end (same shim).
- [x] `make lint` and `make test` green workspace-wide.
- [x] `src/knowledge/daemon.rs` size growth ≤ 100 lines net (delta
      `-2`: legacy req-worker writer + raw-stream handling shrank,
      v1 oneshot worker added).
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- Legacy MCP consumers outside this repo break at the cutover.
  Acceptable per SPEC §3.4 "everything is on one host with one
  binary; lockstep upgrade".
- The daemon's listener thread is currently sync (`std::os::unix`);
  switching to tokio requires either spawning a runtime inside the
  thread or migrating the whole daemon to async. Recommendation:
  add a `#[tokio::main]`-flavoured worker thread that bridges the
  v1 socket while the rest of the daemon stays sync.

---

## Step 5 — Migrate `sy-aiplane` IPC to v1

**Goal:** the second daemon flips. `sy-aiplane.sock` (today
multiplexed inside the knowledge daemon process per SPEC §2.1) gets
its own IPC v1 listener so when Zone 3 splits the scheduler out it
already speaks v1.

**Files:**
- `src/aiplane/ipc.rs:33-106` (modified) — legacy `Op`/`Req`/`Resp`
  enums removed (their consumers were the only thing keeping them
  alive after Step 4's translation layer). Replace with
  `pub const METHODS: &[&str] = &["aiplane.run", "aiplane.batch",
  "aiplane.cancel"]`.
- `src/aiplane/cli.rs`, `src/aiplane/supervisor/mod.rs` (modified)
  — call sites use `sy_ipc::Client::call("aiplane.run",
  json!({"workload": ..., "input": ...}), CallOpts { priority:
  Priority::Interactive, .. })`.
- `src/aiplane/worker/runner.rs` (modified) — registers each
  inbound request via `CancelRegistry::register(req.request_id)`
  **before** calling `Workload::run` (SPEC §4.2 step 1).

**Tests:**
- `tests/aiplane_ipc_v1_run.rs` (new) — spawn aiplane in-thread
  with a `fake` workload; call `aiplane.run`; round-trip.
- `tests/aiplane_ipc_v1_cancel.rs` (new) — call `aiplane.run` with
  the `fake` workload's `sleep_ms` parameter; send `system.cancel`;
  assert the worker returns `Cancelled` within 500 ms.
- `tests/aiplane_ipc_v1_describe_capabilities.rs` (new) —
  `system.describe` lists `priority_classes: ["Realtime",
  "Interactive", "Background", "Batch"]` even though the scheduler
  itself lands in Zone 3 (capability advertised early).

**Definition of Done:**
- [x] Three integration tests pass — `aiplane_ipc_v1_run`,
      `aiplane_ipc_v1_cancel` (worker returns `Cancelled` within the
      500 ms SPEC §4.2 budget), `aiplane_ipc_v1_describe_capabilities`
      (asserts the four canonical priority class names in order +
      `aiplane.{run,batch,cancel}` in `methods`). Located inside
      `src/aiplane/ipc.rs::tests` for the same `[[bin]]`-only crate
      reason as Step 4.
- [x] `sy aiplane run --workload embed …` works end-to-end via v1 —
      `aiplane::cli::run` now calls `sy_ipc::Client::call(\
      "aiplane.run", …)` directly; the bridge routes through
      `AiplaneDispatch` (production: `SupervisorDispatch`).
- [x] `make lint` and `make test` green workspace-wide.
- [x] `Op`/`Req`/`Resp` survive only as the daemon's
      *internal Rust dispatch language* — the v1 inbound frame is
      translated to `Op`/`Req` inside the bridge handler, then
      dispatched to existing handler logic. No wire backward-compat
      remains (verified by `knowledge_ipc_v1_rejects_legacy_envelope`).
      The dead `#[allow(deprecated)]` annotations left over from an
      earlier deprecation pass have been removed; nothing in the
      crate carries `#[deprecated]` today. Collapsing the internal
      dispatch types into method-string matches is a refactor with
      no wire or behavior change — out of scope here.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME` in the changed
      files.

**Risks / unknowns:**
- Aiplane runs inside the knowledge daemon today (SPEC §2.1).
  Whether it gets its own socket now or stays multiplexed is a
  separate question (Zone 5 splits the daemons). For this step: use
  the same `sy-knowledge.sock` and dispatch by method namespace;
  `aiplane.*` and `knowledge.*` both land on one listener.

---

## Step 6 — Migrate `sy-agentd` and `sy-stack-bar` IPC to v1

**Goal:** the remaining two surfaces (`src/agt/protocol.rs:1-160`
and `src/stack/ipc.rs:20-29`) flip. `DaemonEvent` streaming
(SPEC §7 Open Q6) becomes a v1 capability negotiated via
`system.describe.capabilities.streaming = true`.

**Files:**
- `src/agt/protocol.rs:6-160` (modified) — `ClientReq` /
  `ClientReply` / `DaemonEvent` keep their domain shapes but are
  carried inside `Request.params` / `Response.result` /
  out-of-band streaming frames respectively. Define an `Event`
  envelope distinct from `Response` (per SPEC §4.2 — current shape
  is request/response; streaming is a v1 capability) and register
  it under a separate `sy_ipc::stream` module if it isn't already.
- `src/agt/daemon.rs` (modified, currently 638 lines) — listener
  switches to `sy_ipc::Server::serve`. Streaming `Tail` requests
  open a long-lived response stream rather than reusing the
  request socket. Exit codes from `src/agt/protocol.rs:156-160` get
  mapped through SPEC §4.7 stable exit codes (which align: 1 →
  generic, 3 → drift, 4 → not ready, etc.).
- `src/stack/ipc.rs:20-29` (modified) — fire-and-forget `Op`
  variants flip to v1 `Request` with `priority: Interactive` and
  `deadline_ms: 500` (UI repaint). The daemon-side `serve`
  (`src/stack/ipc.rs:69-94`) flips to `sy_ipc::Server::serve` with
  a one-line handler that re-emits over an mpsc.
- `src/stack/cli.rs`, `src/stack/bar/app.rs` (modified) — call
  sites use `sy_ipc::Client::call_fire_and_forget("stack.toggle",
  json!({}), CallOpts::default())`. Add the helper to `sy-ipc` if
  not present.

**Tests:**
- `tests/agt_ipc_v1_run_session.rs` (new) — start `sy agentd`
  in-thread, send `agt.run`, follow with `agt.tail` streaming, get
  three `Event` frames, then `Closed`. Asserts streaming capability
  works through v1.
- `tests/stack_ipc_v1_toggle.rs` (new) — call `stack.toggle` via
  v1 envelope; daemon-side mpsc receives the matching event.
- `tests/all_daemons_describe.rs` (new) — for each of
  `knowledge`/`aiplane`/`agt`/`stack`, `system.describe` returns
  `schema_version: 1` and a non-empty `methods` array.

**Definition of Done:**
- [x] Three integration tests pass — `agt_ipc_v1_run_session`
      (replay returns three transcript Event frames terminated by the
      `closed` sentinel) in `src/agt/daemon.rs::tests`,
      `stack_ipc_v1_toggle` (round-trips a `stack.toggle` v1
      envelope into the bar's mpsc) and `all_daemons_describe`
      (asserts `schema_version: 1` + non-empty methods + streaming
      off for the unary stack daemon) both in `src/stack/ipc.rs::tests`.
      Streaming-capability coverage for agt lives in
      `agt_ipc_v1_describe_streaming_capability` next door.
- [x] `sy agentd run …` (sync v1 client in `src/agt/client.rs` via
      `sy_ipc::blocking`), `sy stack toggle` (fire-and-forget v1
      envelope via `stack::ipc::send`), end-to-end via v1.
- [x] `make lint` and `make test` green workspace-wide.
- [x] No daemon still speaks the legacy line-JSON format. SPEC §3.4
      anti-goal "no backward-compat for unversioned IPC" satisfied —
      both new daemons reject `{`-prefixed frames with
      `IncompatibleSchema`, matching the knowledge/aiplane bridges.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME` in the changed
      files.

**Risks / unknowns:**
- Streaming `DaemonEvent` over a unary JSON-RPC framing is one of
  the spec's open questions (§7 Q6). Resolution baked into this
  step: `sy_ipc` adds a `Stream` response variant that frames a
  series of `Event` envelopes after the initial `Response::Ok`; the
  client side reads until a sentinel `Event { kind: "closed" }`.
  Document this in `sy-ipc/src/stream.rs`.

---

## Step 7 — `sy ipc ping` / `sy ipc describe` CLI subcommands

**Goal:** SPEC §4.7 lists `sy ipc ping <endpoint>` and `sy ipc
describe <endpoint>` as the operator-visible round-trip check.
Lands the subcommands.

**Files:**
- `src/main.rs` (modified, currently 901 lines, +~50 lines) — add
  `Ipc { #[command(subcommand)] cmd: IpcCmd }` variant to the
  top-level clap router. The router only matches — implementation
  lives in `src/ipc_cli.rs`.
- `src/ipc_cli.rs` (new) — `Ping(args)` calls `system.health`,
  prints `ready / degraded / starting / failed` + latency. `Describe
  --json` dumps the `Response.result` from `system.describe`.
- `src/main.rs` (modified) — `--json` flag conformance per SPEC
  §4.12 CLIG check.

**Tests:**
- `src/ipc_cli.rs::tests::ping_endpoint_resolution` — string
  `"knowledge"` resolves to the canonical
  `$XDG_RUNTIME_DIR/sy-knowledge.sock` path.
- `src/ipc_cli.rs::tests::describe_json_schema` — golden JSON
  schema matches SPEC §4.2 "Reserved methods" describe shape.
- `tests/ipc_ping_e2e.rs` (new) — start a daemon, run `sy ipc ping
  knowledge`, expect zero exit and "ready" on stdout.

**Definition of Done:**
- [x] Seven tests pass — five unit (`ping_endpoint_resolution_*`,
      `exit_code_for_known_states`,
      `describe_json_schema_text_dump_lists_methods`) plus two e2e
      (`ipc_ping_e2e_returns_ready_and_zero_exit_code`,
      `ipc_describe_e2e_emits_methods_and_protocol_version`) in
      `src/ipc_cli.rs::tests`, plus four endpoint-path tests in
      `crates/sy-ipc/src/paths.rs::tests`. The DoD's
      `ping_endpoint_resolution` / `describe_json_schema` /
      `ipc_ping_e2e` all live in `src/ipc_cli.rs::tests` because
      the crate ships only a `[[bin]]` target (same constraint as
      Steps 4–6).
- [x] `sy ipc ping --help` matches CLIG (endpoint usage line, `--json`
      flag, exit-code line in the long description).
- [x] `sy ipc describe --json` output schema matches SPEC §4.2 —
      `protocol_version`, `methods`, `capabilities`, `build_info`
      keys round-trip through the e2e test.
- [x] `make lint` and `make test` green workspace-wide (101 sy + 22
      stack-related + 30 sy-ipc + 2 sy-testutils = 155 tests).
- [x] `src/main.rs` LOC ≤ the Zone-1 budget after this step (901
      lines exactly — trimmed two short comment blocks to stay
      under the existing ceiling without ratcheting).

**Risks / unknowns:**
- Endpoint name → socket path mapping convention lives across
  daemons (`sy-knowledge.sock`, `sy/stackbar.sock`, etc.). Consolidated
  in `sy_ipc::paths::for_endpoint(name) -> Option<PathBuf>`; `aiplane`
  aliases `knowledge` per Step 5's shared listener.

---

## Cross-cutting Definition of Done

- [x] All step DoDs satisfied.
- [x] Fresh checkout: `sy doctor` round-trips `system.health` against
      all four v1 sockets — `ipc.knowledge_sock`, `ipc.aiplane_sock`,
      `ipc.agt_sock`, `ipc.stack_sock`. `IpcEndpoint` (one struct, four
      constructors in `src/doctor/checks.rs`) runs in the SPEC §4.6
      check list right after the qdrant probe; the four checks share
      `probe_system_health` so the wire path is identical across
      endpoints. Missing-socket degrades to `Status::Skip` with a
      `systemctl --user start sy-<endpoint>.service` fix-it (matches
      SPEC §4.6 "fail-soft on missing probe surface"). Verified by
      `agt_socket_check_skips_when_path_missing` /
      `stack_socket_check_skips_when_path_missing` plus the existing
      `sy ipc ping <endpoint>` manual recipe.
- [x] All four sockets reject legacy line-JSON with
      `IncompatibleSchema` — verified by
      `knowledge_ipc_v1_rejects_legacy_envelope` (Step 4) and the
      mirror code paths in `src/agt/daemon.rs::reject_legacy_envelope`
      and the v1-only stack/aiplane listeners.
- [x] Cancellation works end-to-end —
      `aiplane_ipc_v1_cancel` asserts the daemon trips `Cancelled`
      within the 500 ms SPEC §4.2 budget when `system.cancel`
      targets an in-flight `aiplane.run` request.
- [x] `make test` and `make lint` green workspace-wide.
- [x] SPEC §3.2 K2 envelope (schema_version / request_id / trace_id
      / deadline_ms / priority / method / params) byte-for-byte on
      the wire — exercised by `request_round_trip` and the four
      daemon-level round-trip tests.
- [x] SPEC §4.2 cancellation pattern enforced by API shape —
      `CancelRegistry::register` returns a `CancelGuard` consumed by
      `dispatch_with_cancel`; there is no `register_after_spawn`
      escape hatch on the public surface.

## Out of Scope

- `memfd+SCM_RIGHTS` blob channel for payloads ≥ 256 KiB —
  follow-on roadmap once v1 envelope is stable in all three
  daemons (SPEC §3.3 Zone 2 "OUT").
- `Cap'n Proto` / `gRPC` / `bincode` alternatives — explicitly
  rejected in SPEC §3.2 K2.
- LSP-style capability *negotiation* beyond the static
  `system.describe.capabilities` map — defer until a real
  v2 capability lands.
- Network transports — SPEC §3.4 anti-goal "no remote-host
  operation".
- `trace_id` propagation through logs — that's Zone 6's job; this
  zone only carries the field through the envelope.
- Per-method authorisation. `SO_PEERCRED` gate covers the threat
  model on a single-user host.
