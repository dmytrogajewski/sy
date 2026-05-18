# SPEC: Architecture refactor — six-zone hardening of `sy`

> Umbrella spec covering the six zones flagged in a friend's high-level
> review of `sy`'s architecture (2026-05-14). Each zone is
> independently shippable; the spec exists so that `/journey →
> /roadmap → /implement` can land them one at a time without losing
> the cross-cutting invariants.

## 1. Summary

`sy` is at the right scale to formalise contracts before the
single-binary control plane ossifies into a god-object. This spec
proposes six concurrent (but independently shippable) refactors:

1. **Workspace decomposition** — split the single crate into ~7
   internal crates (binary `sy` + library crates), lockstep-versioned,
   `publish = false`, with a small `sy-core` for shared vocabulary.
2. **Typed/versioned IPC** — replace the ad-hoc JSON envelopes on
   three UDS surfaces with a single JSON-RPC 2.0 framing carrying
   `schema_version`, `request_id`, `trace_id`, `deadline_ms`,
   `priority`, plus `SO_PEERCRED` gate and `memfd+SCM_RIGHTS` for
   payloads ≥ 256 KiB.
3. **`sy-aiplane` as the sole NPU admission controller** — four
   priority classes (`Realtime | Interactive | Background | Batch`),
   per-class bounded queues with Triton-style timeout actions,
   process-per-workload warm pool, `RunOptions::SetTerminate`-based
   cancellation, CPU fallback for Background/Batch only.
4. **`sy-agent` sandbox + policy layer** — Landlock + seccomp +
   `PR_SET_NO_NEW_PRIVS` + env scrub, wrapped in `systemd-run --user
   --scope` for cgroup caps, governed by KDL profile files
   (`strict`/`normal`/`trusted`) under `configs/policy/`, audited to
   journald + JSONL mirror.
5. **Supervision via `systemd --user`** — `Type=notify` units for
   long-running daemons (aiplane, knowledge+qdrant grouped via
   `BindsTo=`, stack-bar), a `sy.target` group root, socket activation
   for the knowledge facade, plus `sy service` / `sy doctor` UX
   wrappers around `systemctl`/`journalctl`.
6. **Observability** — `tracing` Registry composed of
   journald + rolling JSONL + stderr layers; OTel-shaped log schema;
   `trace_id` propagated end-to-end via the IPC envelope; `metrics` +
   `metrics-exporter-prometheus` exposition over a UDS;
   `sy doctor`/`sy crash` subcommands.

> ⚠️ **Calibration note.** The friend's review contains two
> simplifications worth flagging up front:
> - **"XDna2: one HW context per process"** is too strict.
>   Phoenix/Hawk NPUs expose **6 HW contexts**, Strix **16**, with
>   mixed spatial + temporal scheduling driven by the in-firmware
>   Resource Solver
>   ([kernel.org/accel/amdxdna/amdnpu.html][k1], [DeepWiki
>   xdna-driver/architecture][dw1]). The architectural rule that
>   *does* hold is process-per-workload, because RyzenAI-SW issue
>   #223 documents that multiple ORT sessions in one process crash
>   silently on VitisAI EP ([amd/RyzenAI-SW#223][rai223]).
> - **"`sy-stack-bar` hosts an MCP side server"** is incorrect. The
>   stack MCP server lives in `src/stack/mcp.rs:1-+` and runs as a
>   separate stdio subcommand (`sy stack mcp`); the bar UI in
>   `src/stack/bar/app.rs` does not embed it. The friend's "split MCP
>   out" recommendation is therefore already satisfied — we should
>   document it, not refactor it.
>
> Everything else in the review survives contact with the codebase.

[k1]: https://docs.kernel.org/accel/amdxdna/amdnpu.html
[dw1]: https://deepwiki.com/amd/xdna-driver/2-architecture
[rai223]: https://github.com/amd/RyzenAI-SW/issues/223

## 2. Background & Research

### 2.1 Current sy state (grounded in repo, 2026-05-14)

**Crate layout.** Single crate, single `[[bin]] sy`. Modules cluster
into four logical subsystems plus rice utility modules:

| Subsystem | Path | Status |
|---|---|---|
| Agent / MCP bridge | `src/agt/{daemon,protocol,acp,permission}.rs` | running daemon, notify-send permission UX, **no sandbox, no audit log** |
| NPU plane | `src/aiplane/{ipc,registry,supervisor,worker,session}.rs` | supervisor + per-kind workers exist; **global NPU `Mutex<()>`, FIFO, no priority** |
| Knowledge plane | `src/knowledge/{daemon,ipc,mcp,qdrant}.rs` | daemon + qdrant child + IPC, **JSON-line wire, no envelope** |
| Stack bar | `src/stack/{bar/app,ipc,mcp}.rs` | iced layer-shell UI; MCP is a *separate* stdio subcommand, not embedded |

**IPC surfaces** (three sockets, all bespoke JSON):
- `$XDG_RUNTIME_DIR/sy-knowledge.sock` — `Op` (fire-and-forget) +
  `Req`/`Resp` (request-response), no `schema_version`/`request_id`/
  `deadline_ms` (`src/aiplane/ipc.rs:33-106`).
- `$XDG_RUNTIME_DIR/sy-agentd.sock` — `ClientReq`/`ClientReply`/
  `DaemonEvent` with streaming over the same socket
  (`src/agt/protocol.rs:8-82`).
- `$XDG_RUNTIME_DIR/sy/stackbar.sock` — fire-and-forget
  (`src/stack/ipc.rs:20-29`).

**Workload registry.** `Workload` trait in `src/aiplane/registry.rs`
already has `load/run/run_batch/unload`. **No `priority` field**;
dispatch is FIFO behind a global `SessionPool` `Mutex<()>`.

**Supervision.** One systemd user unit:
`configs/systemd/system/sy-knowledge.service` (Type=simple,
`AmbientCapabilities=CAP_IPC_LOCK`, `LimitMEMLOCK=infinity`,
`MemoryHigh=12G`). aiplane runs *inside* the knowledge daemon
process; **no separate aiplane unit, no `Type=notify`**.

**Observability.** No `tracing` in `Cargo.toml`. All logs are
`eprintln!`/`println!`. No `sy doctor`.

**Specs in flight.** `specs/research/stack-bar-ux/SPEC.md`,
`specs/roadmaps/stack-bar-ux/ROADMAP.md`,
`specs/bugs/BUG-20260513-2336.md`. Empty `specs/journeys/`,
`specs/roadmaps/` (except stack-bar).

### 2.2 Market context (selected highlights)

**Workspace decomp** — jujutsu (5 crates), helix (14 crates with
`helix-stdx → helix-core → helix-view → helix-term`), nushell (38+
crates with strict `nu-protocol → nu-engine → nu-command → nu-cli`),
zellij (`zellij-utils` shared + `zellij-client` + `zellij-server` +
binary), cargo (`cargo-util*` vocabulary crates), ripgrep (8 crates,
only project where internals have real semver). All except ripgrep
lockstep-version internal crates and `publish = false` — the
"private monorepo" pattern, exactly what fits a single-binary tool.
Sources: [matklad — Large Rust Workspaces][m1], [matklad — Fast Rust
Builds][m2], [helix architecture.md][hx1], [nushell DeepWiki][nu1].

**IPC** — LSP uses JSON-RPC over stdio with capability negotiation
via `initialize`; PipeWire uses object-oriented versioned interfaces
with `memfd` over `SCM_RIGHTS` for media buffers ([PipeWire native
protocol][pw1]); Wayland's `wl_shm` is the canonical
SCM_RIGHTS+mmap pattern ([wayland-book/surfaces/shared-memory][wb1]).
LSP `$/cancelRequest` is the canonical cancellation pattern; the
race-free implementation requires registering `request_id →
CancellationToken` before spawning the work ([SourceKit-LSP
cancellation pattern][skl1]).

**NPU/inference admission** — Triton has the most production-mature
model: numeric priority, per-level `ModelQueuePolicy` with
`max_queue_size` / `default_timeout_microseconds` / `timeout_action ∈
{REJECT, DELAY}`, plus `ModelWarmup` ([Triton schedule-policy][t1]).
vLLM's V1 scheduler uses lower-value-first priority, ties broken by
arrival, with `RECOMPUTE` preemption ([vLLM scheduler API][v1]). On
the hardware side, the amdxdna kernel doc describes Phoenix=6 /
Strix=16 HW contexts, with Resource-Solver-driven spatial + temporal
placement; **no documented userspace priority ioctl** ([kernel
amdxdna][k1]). VitisAI EP cold compile = minutes; cached load =
sub-second ([Ryzen AI compile + cache][rai1]) — warm-pool is
non-optional for interactive workloads.

**Agent sandboxing** — MCP spec explicitly delegates sandboxing to
the host ([MCP Security Best Practices §Local server compromise][m1])
. Claude Code's sandbox is opt-in, defaults to
`allowUnsandboxedCommands=true` (researchers showed bash allowlists
are leaky — [joinformal allowlist analysis][af1]). Cursor compiles a
per-workspace sandbox profile across the entire subprocess tree
([Cursor agent security][cs1]). Cline's `requires_approval` is LLM-
decided and has been exploited via prompt injection in `.clinerules`
([Mindgard Cline vulnerabilities][m2]). Aider runs unsandboxed by
default. Tauri v2 retired its v1 allowlist as unscalable and replaced
it with capability + scope objects ([Tauri v2 capabilities][tv2]).
Landlock 6.7+ adds per-port TCP gate (`LANDLOCK_ACCESS_NET_CONNECT_TCP`
— [landlock.io news/4][ll1]). firejail is suid-root with a steady
CVE drumbeat ([CVE-2022-31214][fj1]) — reject for `sy`.

**systemd --user** — PipeWire / WirePlumber / mako / foot-server /
niri / gnome-keyring / xwayland-satellite all use `Type=notify` with
`sd_notify(READY=1)` and `BindsTo=` for grouped lifecycles
([sd_notify(3)][sd1], [Poettering socket activation][p1]). foot-server
is the textbook CLI-wakes-daemon socket-activation pattern. The Rust
crate of choice is `sd-notify` (pure Rust, libc only — [crates.io/
sd-notify][sn1]).

**Observability** — convergence target for log schema is the
[OpenTelemetry Logs Data Model][otel1], which Elastic Common Schema
is also migrating toward ([OTEP-0199][otep199]). `tracing-journald`
silently drops on non-EMSGSIZE write errors — mitigate with a
redundant rolling JSONL appender ([tracing-journald source][tj1]).
`metrics-exporter-prometheus` has built-in UDS support behind feature
`uds-listener` ([PrometheusBuilder docs][pb1]) — no HTTP server
needed.

[m1]: https://matklad.github.io/2021/08/22/large-rust-workspaces.html
[m2]: https://matklad.github.io/2021/09/04/fast-rust-builds.html
[hx1]: https://github.com/helix-editor/helix/blob/master/docs/architecture.md
[nu1]: https://deepwiki.com/nushell/nushell
[pw1]: https://docs.pipewire.org/page_native_protocol.html
[wb1]: https://wayland-book.com/surfaces/shared-memory.html
[skl1]: https://deepwiki.com/swiftlang/sourcekit-lsp/3.4-request-handling-and-cancellation
[t1]: https://docs.nvidia.com/deeplearning/triton-inference-server/user-guide/docs/protocol/extension_schedule_policy.html
[v1]: https://docs.vllm.ai/en/stable/api/vllm/v1/core/sched/scheduler/
[rai1]: https://ryzenai.docs.amd.com/en/latest/modelrun.html
[af1]: https://www.joinformal.com/blog/allowlisting-some-bash-commands-is-often-the-same-as-allowlisting-all-with-claude-code/
[cs1]: https://cursor.com/docs/agent/security
[tv2]: https://v2.tauri.app/security/capabilities/
[ll1]: https://landlock.io/news/4/
[fj1]: https://www.openwall.com/lists/oss-security/2022/06/08/10
[sd1]: https://man7.org/linux/man-pages/man3/sd_notify.3.html
[p1]: http://0pointer.de/blog/projects/socket-activation.html
[sn1]: https://crates.io/crates/sd-notify
[otel1]: https://opentelemetry.io/docs/specs/otel/logs/data-model/
[otep199]: https://github.com/open-telemetry/oteps/blob/main/text/0199-support-elastic-common-schema-in-opentelemetry.md
[tj1]: https://docs.rs/tracing-journald/latest/src/tracing_journald/lib.rs.html
[pb1]: https://docs.rs/metrics-exporter-prometheus/latest/metrics_exporter_prometheus/struct.PrometheusBuilder.html

### 2.3 Deep dives

- **matklad on hub crates** — *"the most important property of a
  crate is which crates it doesn't (transitively) depend on"* ([Fast
  Rust Builds][m2]). Implication: `sy-core` must stay small or every
  rebuild touches everything.
- **AMD XDNA scheduling** — DRM-misc 7.2 changed the default driver
  scheduling policy from FIFO to "fair" ([Phoronix][ph1]). NVIDIA
  MPS' `set_default_client_priority` is documented as a *hint*; no
  AMD analog. Application-level QoS is the right enforcement layer.
- **vLLM cancellation** — `engine.abort(request_id)` is the canonical
  API, driven from `request.is_disconnected()` in the OpenAI server
  loop ([vLLM #4240][vl4240]). ORT's `RunOptions::SetTerminate(true)`
  works across language bindings and can be called from another
  thread, but is **not documented** to interrupt a VAIP-partitioned
  subgraph mid-run on Ryzen AI ([ORT C API][ort1], no relevant issue
  on amd/RyzenAI-SW).
- **MCP host responsibility** — the spec explicitly says clients
  *SHOULD* "Execute MCP server commands in a sandboxed environment
  with minimal default privileges … Launch MCP servers with
  restricted access to the file system, network, and other system
  resources." `sy` is the host; this is on us.
- **`tracing-journald` drop bug** — non-`EMSGSIZE` write errors are
  silently swallowed. Two-sink design (journald + JSONL appender)
  is defence in depth.

[ph1]: https://www.phoronix.com/news/Linux-7.2-Initial-DRM-Misc-Next
[vl4240]: https://github.com/vllm-project/vllm/issues/4240
[ort1]: https://onnxruntime.ai/docs/api/c/struct_ort_1_1_run_options.html

## 3. Proposal

### 3.1 Approach

Six concurrent refactors, each independently shippable, ordered so
later zones consume earlier zones' invariants:

```
Zone 0 (now)  : Status quo — single crate, ad-hoc JSON IPC,
                FIFO NPU, no sandbox, eprintln logs, one systemd unit.
Zone 1        : Workspace split (sy + 6 lib crates + sy-testutils).
Zone 2        : sy-ipc v1 envelope + cancellation + memfd blobs.
Zone 3        : sy-aiplane scheduler (4 classes, queues, warm pool).
Zone 4        : sy-agent sandbox (Landlock + seccomp + systemd-run scope).
Zone 5        : systemd --user unit set + sy service + sy doctor.
Zone 6        : Observability (tracing/metrics/crash hooks).
```

Zones 1, 2, 6 are **enablers** and should land first because they
unblock the others. Zones 3, 4, 5 carry the load-bearing
correctness/security wins.

### 3.2 Key decisions

| # | Decision | Choice | Reasoning | Alternatives considered |
|---|---|---|---|---|
| K1 | Workspace shape | Thin `sy` binary + `sy-core` + `sy-aiplane` + `sy-knowledge` + `sy-stack` + `sy-agt` + `sy-ipc` + `sy-testutils`. Lockstep-versioned, `publish = false`. | Matches matklad's flat-workspace pattern, jj/helix/zellij convention. Keeps `main.rs` parsing-only. `sy-core` deliberately minimal to avoid hub-rebuild penalty. | (a) Keep single crate — preserves the god-binary anti-pattern. (b) Three crates (`sy`/`sy-core`/`sy-engine`) — smaller blast radius but doesn't separate aiplane from knowledge, which the friend's review correctly flags as the highest-risk coupling. (c) Ripgrep-style public semver per crate — over-engineered for a single-maintainer rice. |
| K2 | IPC wire format | JSON-RPC 2.0 frames over length-prefixed UDS with a `sy.v1` envelope; memfd+SCM_RIGHTS for blobs ≥ 256 KiB. | Debuggable with `socat`; agents can hand-craft frames (matches MCP / CLAUDE.md ethos); LSP-style capability negotiation is well-understood. Schema versioning via `schema_version` + capability map. | (a) Cap'n Proto — best schema evolution + free streaming/cancellation, but opaque to `socat` and Rust ergonomics rough ([Swatinem][sw1]). Revisit if/when frame volume dominates. (b) gRPC/tonic — HTTP/2 framing overkill on UDS. (c) bincode — fastest, worst evolvability. (d) Status quo — no versioning, no cancellation, blobs gone through JSON. |
| K3 | NPU scheduler shape | 4 priority classes (`Realtime`/`Interactive`/`Background`/`Batch`), per-class bounded queues with Triton-style `ModelQueuePolicy`; no in-class preemption; cross-class via "don't dispatch lower while higher is queued"; hard escape via `RunOptions::SetTerminate` + SIGKILL fallback. Process-per-workload warm pool capped at `n_hw_contexts - 2`. | Triton is the mature production reference. 4 classes (not 3) because VAD frame budgets cannot share latency goals with general interactive work. Process-per-workload sidesteps RyzenAI-SW #223. | (a) 3 classes (friend's proposal) — no realtime budget for VAD/audio. (b) MLFQ with auto-demotion — too clever; caller-declared class is honest. (c) Compositor-driven priority bumping — not documented on niri or amdxdna; not implementable today. |
| K4 | Sandbox architecture | Layered in-process (Landlock + seccompiler + `PR_SET_NO_NEW_PRIVS` + env scrub) wrapped in `systemd-run --user --scope` for cgroup caps. Optional `bwrap` second layer for `strict` profile only. Policy in KDL under `configs/policy/`. | Landlock+seccomp+cgroup cover the threat model without bwrap as a hard dep. systemd-run --user --scope ties into the existing supervision story. KDL matches `sy`'s existing convention. firejail rejected (suid-root, CVE history). | (a) bwrap-mandatory — adds binary dep; user-ns may be disabled on hardened kernels. (b) Pure systemd directives — many namespacing options unavailable in `--user` scopes; insufficient. (c) WASM/Deno-style runtime sandbox — wrong abstraction; we run native binaries (ripgrep, formatters). |
| K5 | Supervision | `systemd --user` for long-running daemons (`sy-aiplane`, `sy-knowledge`+`sy-qdrant` grouped via `BindsTo=`, `sy-stack-bar`); `sy.target` group root; socket activation for knowledge facade; aiplane warm-always (`WantedBy=graphical-session.target`). | Existing infra, no extra supervisor binary, integrates with `journalctl`/`coredumpctl`. Matches PipeWire/WirePlumber/foot-server patterns. Per-request NPU worker spawn stays under aiplane (`systemd-run` too heavy at sub-ms cadence). | (a) `sy-supervisord` custom binary — duplicates well-tested systemd behaviour. (b) Pure shell scripts — no health distinction. (c) systemd generators in `/etc/systemd/user-generators/` — overkill, breaks user-installable property. |
| K6 | Observability stack | `tracing` Registry: `fmt::Layer` (stderr, JSON/pretty) + `tracing_journald::Layer` + `tracing_appender::rolling` (`non_blocking`) + `tracing_error::ErrorLayer` + `tracing-panic` hook. `metrics` + `metrics-exporter-prometheus` UDS. OTel-shaped log schema with `trace_id` propagated through IPC envelope (W3C `traceparent`). `sy doctor` linear-checks shape. | Zero networked deps, fits single-host rice. journald is primary store (matches systemd-everything choice); rolling JSONL is the redundancy for journald's silent-drop bug. OTel-shaped schema means future `--otlp` is one Layer. | (a) `tracing-opentelemetry` from day 1 — overkill; pulls collector deps; SpanTrace crash bug ([tracing#763][tr763]). (b) Bunyan formatter — keys don't match OTel. (c) Custom log store — duplicates journald rotation/retention; clear snowflake violation. |
| K7 | Stack-bar MCP placement | **Status quo wins** — stack MCP server is already a separate stdio subcommand (`src/stack/mcp.rs`), not embedded in the bar UI. Document this; no refactor. | The friend's recommendation is already satisfied. Moving it again would be churn. | Split into a standalone binary `sy-mcp-server` — premature, and it's still ergonomic to invoke `sy stack mcp` per subprocess convention. |

[sw1]: https://swatinem.de/blog/rust-grpc-capnp/
[tr763]: https://github.com/tokio-rs/tracing/issues/763

### 3.3 Minimum Loveable scope (per zone)

A rice user should *feel* each zone land without the whole refactor
being done first. Per-zone ML:

**Zone 1 — Workspace.**
- IN: cargo workspace at the root; `sy-core` extracted with
  `WorkloadKind`, `Priority`, `ErrorCode`; `sy` becomes a thin binary
  that depends on `sy-core` only at first. Existing modules stay in
  place under `sy/src/` until later moves.
- OUT: moving aiplane/knowledge/stack/agt to their own crates (do
  that in a follow-on roadmap step once `sy-core` is stable).

**Zone 2 — IPC v1.**
- IN: `sy-ipc` crate with the v1 envelope, `LengthDelimitedCodec`
  framing, cancellation via `CancellationToken`, `SO_PEERCRED` gate,
  `describe`/`health`/`cancel` reserved methods.
- OUT: memfd+SCM_RIGHTS blob channel (land after v1 envelope is
  stable in all three daemons).

**Zone 3 — Aiplane scheduler.**
- IN: `Request` struct in `sy-core`; four priority queues
  (`crossbeam_channel` bounded); admission with timeout actions;
  process-per-workload warm pool with always-warm VAD/EyeTrack and
  LRU max-3 for the rest; `RunOptions::SetTerminate`-driven
  cancellation; supervisor restart from VitisAI compile cache.
- OUT: CPU fallback path (land in a second pass; default = reject
  Realtime if NPU unavailable).

**Zone 4 — Sandbox.**
- IN: `configs/policy/{strict,normal,trusted}.kdl`; per-tool overlay
  `configs/policy/tools/*.kdl`; in-process Landlock + seccompiler +
  `PR_SET_NO_NEW_PRIVS` + env scrub before exec; `systemd-run --user
  --scope` with `MemoryMax`/`CPUQuota`/`TasksMax`/`RuntimeMaxSec`/
  `PrivateNetwork`; audit log dual-sink (journald + JSONL); `sy
  approve <token>` TTY consent flow.
- OUT: `bwrap` second layer (Zone 4.2); `xdg-desktop-portal`
  Notification action-button consent UX (Zone 4.3).

**Zone 5 — Supervision.**
- IN: `configs/systemd/user/sy-{aiplane,knowledge,qdrant,stack-bar,
  agentd}.{service,socket}`, `sy.target` group root, `sd_notify`
  integration; `sy service start|stop|restart|status` wrapping
  `systemctl --user`; `sy apply` symlinks units + `daemon-reload`.
- OUT: socket activation (start with always-on; flip the
  knowledge facade to socket-activated once cold-start latency is
  measured).

**Zone 6 — Observability.**
- IN: tracing Registry, OTel-shaped JSON schema, `trace_id` in IPC
  envelope, `sy doctor`/`sy doctor --json`, `sy service logs`,
  panic hook → crash JSONL, the ~10 metrics listed in §4.
- OUT: `metrics-exporter-prometheus` UDS (Zone 6.2; can ship
  without it).

### 3.4 Anti-goals

- **No remote-host operation.** `sy` is single-host single-user.
  Refuse to add TCP listeners, remote qdrant by default, remote
  rerank providers, or "team-shared" agent policy.
- **No custom supervisor binary.** Reuse `systemd --user`. The
  friend's `sy-supervisord` option is explicitly rejected.
- **No OpenTelemetry collector on the desktop.** Logs and metrics
  stay on-host; OTel-shaped schema is the future hedge, not a
  current dep.
- **No protobuf/gRPC/Cap'n Proto on the local IPC.** Picked
  JSON-RPC for `socat`-debuggability and agent ergonomics.
- **No LLM-inferred auto-approval for agent tools** (Cline
  anti-pattern). Consent is human-in-the-loop or pre-granted via TTY.
- **No bash-string command interface** in agent policy. `exec` is
  `(binary, argv[])`; never `/bin/sh -c …`.
- **No backward-compat for unversioned IPC.** Bump
  `schema_version=1` at the cutover; daemons reject `null`/missing
  versions with `INCOMPATIBLE_SCHEMA`. Acceptable because everything
  is on one host with one binary.
- **No firejail dep.** suid-root history is disqualifying.
- **No premature crate publication.** All internal crates
  `publish = false`; lockstep version via `version.workspace = true`.
- **No tinkering with `amdxdna` ioctl priorities.** Not documented
  upstream; userspace QoS is the right layer.

## 4. Technical Design

### 4.1 Architecture (top-down)

```
                       ┌──────────────────────────┐
                       │   sy (thin binary)       │   src/main.rs (clap router only)
                       └────────────┬─────────────┘
                                    │ depends on
       ┌───────────┬────────────────┼────────────────┬──────────┐
       ▼           ▼                ▼                ▼          ▼
   sy-core    sy-ipc           sy-aiplane      sy-knowledge   sy-stack
   types,     transport,       scheduler,      daemon,        bar UI,
   errors,    envelope,        worker pool,    qdrant,        clip,
   policy     SO_PEERCRED,     workloads       embed,         onto
   schema     blob channel                     mcp
                                                                │
                                          ┌─────────────────────┘
                                          ▼
                                      sy-agt
                                      agent client, ACP,
                                      policy eval, sandbox

   (sy-testutils, publish=false, devdeps only — daemon-in-thread harness)
```

Public-API discipline: `sy-core` exports `Priority`, `WorkloadKind`,
`Request`, `Response`, `ErrorCode`, IPC envelope types. Other crates
depend on `sy-core` + their own deps + maybe `sy-ipc`. **No
direct dep from `sy-knowledge` to `sy-aiplane`** — they talk through
`sy-ipc`. This is the load-bearing decoupling.

### 4.2 IPC v1 envelope (canonical)

Request:
```json
{
  "schema_version": 1,
  "request_id": "01HXYZ…",          // ULID
  "trace_id": "0af7651916cd43dd…",  // 16-byte hex, W3C traceparent compat
  "parent_span_id": "b7ad6b7169203331",
  "deadline_ms": 5000,
  "priority": "Interactive",        // Realtime | Interactive | Background | Batch
  "method": "aiplane.run",
  "params": { "workload": "embed", "input": …, "blob": null }
}
```

Response (success):
```json
{
  "schema_version": 1,
  "request_id": "01HXYZ…",
  "result": { … },
  "blob": { "kind": "memfd", "len": 4194304, "sha256": "…" }
}
```

Error:
```json
{
  "schema_version": 1,
  "request_id": "01HXYZ…",
  "error": {
    "code": "Overloaded",            // structured code enum
    "message": "queue full for class=Background",
    "retry_after_ms": 200,
    "details": { "class": "Background", "queue_depth": 256 }
  }
}
```

**Reserved methods** (every daemon):
- `system.describe` — returns
  `{protocol_version, methods:[…], capabilities:{…}, build_info:{…}}`.
- `system.health` — returns
  `{state: ready|degraded|starting|failed, status_line, queue_depth, warm_models:[…]}`.
- `system.cancel` — `params: {target_request_id}`; returns ACK after
  the worker has yielded.

**Framing**: 4-byte big-endian length + JSON bytes (via
`tokio_util::codec::LengthDelimitedCodec`).

**Origin check**: every accept calls
`UnixStream::peer_cred()` (or `rustix::net::sockopt::socket_peercred`)
and asserts `peer.uid == geteuid()`; rejects otherwise. The kernel
also enforces `0700`/`0600` via `$XDG_RUNTIME_DIR`; this is defence
in depth.

**Blob channel**: when `blob.kind == "memfd"`, the actual fd is
passed alongside the response via `SCM_RIGHTS` on the same UDS
(out-of-band). Receiver verifies `F_GET_SEALS` includes
`F_SEAL_WRITE | F_SEAL_SHRINK | F_SEAL_GROW` before mmap.

**Cancellation pattern**:
1. Server registers `request_id → child_token = root.child_token()`
   **before** spawning the work future.
2. `system.cancel{request_id}` triggers `child_token.cancel()`.
3. Worker hot path uses `tokio::select! { _ = work => …, _ =
   child_token.cancelled() => emit `Cancelled` and stop }`.
4. NPU worker holds `RunOptions::SetTerminate(true)` handle to abort
   in-flight ORT calls; if no yield within 500 ms, supervisor
   SIGKILLs the worker and restarts from VitisAI compile cache.

### 4.3 Aiplane scheduler

`sy-aiplane::scheduler::Request`:
```rust
pub struct Request {
    pub id: ulid::Ulid,
    pub workload: WorkloadKind,
    pub input: WorkloadInput,
    pub class: Priority,           // 4-tier
    pub queued_at: Instant,
    pub deadline: Option<Instant>,
    pub cancel: CancellationToken,
    pub respond: oneshot::Sender<Result<WorkloadOutput, AiplaneError>>,
}
```

Four bounded `crossbeam_channel` queues, one per class. Dispatcher
pulls strict-priority highest-class-with-work, falling through to
lower classes. Within a class: FIFO.

Per-class `ModelQueuePolicy` (Triton-shaped):

| Class | Cap | Soft-deadline | Timeout action | Notes |
|---|---|---|---|---|
| Realtime | 4 | 50 ms | REJECT | VAD, eye-track; reject rather than queue audio |
| Interactive | 32 | 500 ms | REJECT | STT live, foreground search |
| Background | 256 | 30 s | DELAY | Embed pass, rerank, OCR |
| Batch | 4096 | none | DELAY | KB rebuild, bulk ingestion |

Soft caps: `inflight ≤ n_hw_contexts - 2` per NPU (Phoenix=4 inflight
cap, Strix=14). Excess → `Overloaded{retry_after_ms}`.

**Warm pool** (process-per-workload):
- Always warm: VAD, EyeTrack.
- Warm-on-activity, idle TTL 15 min: STT, Embed.
- LRU, max-3-concurrent-warm: Rerank, OCR, CLIP, TTS, Densify.
- Capped at `n_hw_contexts - 2`. Eviction via existing
  `Workload::unload()`
  (`src/aiplane/registry.rs:222`).

**Cancellation/preemption**:
- No in-class preemption (FIFO).
- Cross-class: dispatcher never starts a lower-class request while a
  higher-class request is queued.
- Hard escape: if `Interactive` has waited >200 ms behind a `Batch`
  run, scheduler calls `RunOptions::SetTerminate(true)` on the worker;
  if no yield in 500 ms, SIGKILL + restart from compile cache.

**CPU fallback**:
- Trigger when worker `Failed`, queue >80 % cap, or `Unavailable`.
- Realtime: refuse fallback — return `NpuUnavailable`. (Audio with
  CPU embed is worse than no audio.)
- Interactive: best-effort fallback with a `degraded=true` flag in
  the response.
- Background/Batch: fallback by default.

### 4.4 Agent sandbox + policy

**Policy schema** (`configs/policy/profiles/normal.kdl`):
```kdl
profile "normal" {
    read_paths "/home/dmitriy/sources" "/home/dmitriy/.cache" "/usr"
    write_paths "/home/dmitriy/sources"
    exec_allowlist {
        bin "/usr/bin/rg" argv "*"
        bin "/usr/bin/cargo" argv "test*" "build*" "check*"
        bin "/usr/bin/git" argv "status" "diff*" "log*" "show*"
    }
    net_outbound_allowlist {
        host "github.com" port=443
        host "crates.io" port=443
    }
    env_passthrough_allowlist "PATH" "HOME" "LANG" "TERM"
    max_runtime_seconds 60
    max_stdout_bytes 16777216
    max_memory_mb 1024
    max_pids 256
    deny_network false
    require_consent "once_per_session"
}
```

Three default profiles ship in `configs/policy/profiles/`:
- `strict` — MCP default; read=$REPO, write=∅, deny_network=true,
  consent=every_call.
- `normal` — interactive CLI default.
- `trusted` — opt-in only, requires
  `sy policy trust --confirm` from a TTY.

Per-tool overlay: `configs/policy/tools/<tool>.kdl` overrides the
profile fields for that tool.

**Resolution + enforcement pipeline** (in-process in `sy-agt`):
1. Load profile (`strict | normal | trusted`) + per-tool overlay.
2. Compute SHA-256 of resolved policy; log fingerprint at startup.
3. Pre-exec: fork; in child, `prctl(PR_SET_NO_NEW_PRIVS, 1)`;
   `landlock::RulesetBuilder` from `read_paths` / `write_paths` /
   `net_outbound_allowlist`; `seccompiler` filter with
   curated allowlist + arg matching for high-risk syscalls
   (`execveat`, `unlinkat`, `mount`, …); scrub env vars; `execve` the
   target binary with explicit argv array.
4. Wrap step 3 in `systemd-run --user --scope --collect
   -p MemoryMax=… -p CPUQuota=… -p TasksMax=… -p RuntimeMaxSec=…
   -p ProtectSystem=strict -p ReadWritePaths=…
   -p NoNewPrivileges=yes [-p PrivateNetwork=yes]` for the cgroup
   resource caps + namespace knobs.

**Audit log** (dual sink, fire-and-forget):
- journald via `libsystemd::journal::send` with structured fields:
  `SY_TOOL`, `SY_POLICY_SHA`, `SY_DECISION ∈ {allow,deny,consent}`,
  `SY_ARGV`, `SY_REQUEST_ID`, `SY_TRACE_ID`, `MESSAGE_ID`.
- `$XDG_STATE_HOME/sy/audit.jsonl` — append-only, rotated at 64 MiB
  with zstd compression (`audit.jsonl.1.zst`, …).

**Consent UX**:
1. Default `strict` policy returns
   `{error.code: "ConsentRequired", details:{token, policy_diff, expires_at}}`.
2. The user approves via either:
   - `sy approve <token>` from a TTY.
   - `sy policy grant --tool=<name> --scope=<path> --ttl=15m`
     pre-issued grant (writes `$XDG_RUNTIME_DIR/sy/grants/`).
   - xdg-desktop-portal Notification action-button (Zone 4.3
     follow-on; mako already handles action buttons on this host).
3. No auto-approval based on LLM intent flags.

### 4.5 Supervision (systemd --user)

Unit files in `configs/systemd/user/`, symlinked into
`~/.config/systemd/user/` by `sy apply`.

`sy.target` — group root:
```ini
[Unit]
Description=sy desktop AI plane
PartOf=graphical-session.target
After=graphical-session.target
```

`sy-aiplane.service` (warm-always):
```ini
[Unit]
Description=sy aiplane supervisor
After=graphical-session.target
PartOf=sy.target
[Service]
Type=notify
NotifyAccess=main
ExecStart=/usr/bin/sy aiplane daemon --foreground
Restart=on-failure
WatchdogSec=30s
AmbientCapabilities=CAP_IPC_LOCK
LimitMEMLOCK=infinity
MemoryHigh=12G
Nice=10
[Install]
WantedBy=sy.target
```

`sy-qdrant.service` (Type=simple, qdrant doesn't sd_notify):
```ini
[Unit]
Description=sy local qdrant
PartOf=sy.target
[Service]
Type=simple
ExecStart=/usr/bin/qdrant --config-path %h/.config/sy/qdrant.yaml
Restart=on-failure
[Install]
WantedBy=sy.target
```

`sy-knowledge.service` (BindsTo qdrant, socket-activated):
```ini
[Unit]
Description=sy knowledge daemon
After=sy-qdrant.service
BindsTo=sy-qdrant.service
Requires=sy-knowledge.socket
PartOf=sy.target
[Service]
Type=notify
ExecStart=/usr/bin/sy knowledge daemon --foreground
Restart=on-failure
WatchdogSec=30s
UnsetEnvironment=LISTEN_PID LISTEN_FDS LISTEN_FDNAMES
```

`sy-knowledge.socket`:
```ini
[Unit]
Description=sy knowledge socket
PartOf=sy.target
[Socket]
ListenStream=%t/sy-knowledge.sock
DirectoryMode=0700
SocketMode=0600
[Install]
WantedBy=sockets.target
```

`sy-stack-bar.service`, `sy-agentd.service` follow the same shape.

**Rust integration**: `sd-notify` crate. After bind:
`sd_notify::notify(false, &[NotifyState::Ready, NotifyState::Status("ready")])`.
On SIGTERM: `Stopping + Status("draining")`. Watchdog:
read `WATCHDOG_USEC` via `sd_notify::watchdog_enabled()` and
`sd_notify::notify(false, &[NotifyState::Watchdog])` at half-interval.

**State mapping**:

| sy logical state | systemd encoding |
|---|---|
| not installed | unit file absent |
| stopped | `ActiveState=inactive`, `SubState=dead` |
| starting | `ActiveState=activating` (before `READY=1`) |
| ready | `ActiveState=active`, `SubState=running` |
| degraded | `ActiveState=active` + `STATUS="degraded: <reason>"` (sy-level concept; systemd has no native "degraded") |
| failed | `ActiveState=failed` + `Result=` distinguishes exit-code/watchdog/oom |

### 4.6 Observability stack

**Subscriber** (single Registry, per-process configuration):
- CLI mode: `fmt::Layer` to stderr; JSON when `!isatty(stderr)` or
  `--log-format=json`, else compact human; ANSI gated on `NO_COLOR`.
- Daemon mode: `tracing_journald::Layer::new().with_field_prefix(None)`
  + `tracing_appender::rolling::daily($XDG_STATE_HOME/sy/logs,
  "sy-<name>.jsonl")` wrapped in `non_blocking` + same `fmt::Layer`
  to stderr for `journalctl -f` legibility.
- Common: `EnvFilter` (`RUST_LOG`), `tracing_error::ErrorLayer`,
  `tracing_panic::panic_hook`.

**Log schema** (one JSON object per line, OTel-aligned):
```json
{
  "v": 1,
  "ts": "2026-05-14T18:22:01.123Z",
  "severity_text": "INFO",
  "severity_number": 9,
  "target": "sy::aiplane::worker",
  "span": "embed",
  "trace_id": "0af7651916cd43dd…",
  "span_id": "b7ad6b7169203331",
  "resource": { "service.name": "sy-aiplane", "host.name": "…" },
  "attributes": { "workload": "embed", "batch": 32, "latency_ms": 18.4 },
  "body": "workload completed"
}
```

`trace_id` is set at the CLI/MCP edge, carried through the IPC
envelope, and stamped on every log line — `journalctl --user -u
'sy-*' SY_TRACE_ID=<id> -o json` stitches the entire call chain.

**Metrics** (via `metrics` + `metrics-exporter-prometheus` UDS at
`$XDG_RUNTIME_DIR/sy/metrics.sock`):
- Counters: `sy_workload_completed_total{kind}`,
  `sy_workload_errors_total{kind,reason}`,
  `sy_policy_denials_total{tool}`,
  `sy_ipc_errors_total{endpoint,kind}`.
- Gauges: `sy_models_warm{kind}`, `sy_queue_depth{class,kind}`,
  `sy_npu_temp_celsius` (if exposed).
- Histograms: `sy_workload_latency_seconds{kind}` with explicit
  buckets per workload kind.

`sy stats` `curl --unix-socket`s the exposition; waybar formatter
does the same.

**`sy doctor`** linear-checks schema:
```json
{
  "version": 1,
  "summary": { "pass": 8, "warn": 1, "fail": 0, "skip": 0 },
  "checks": [
    {
      "id": "aiplane.npu.device",
      "subsystem": "aiplane",
      "status": "pass",          // pass | warn | fail | skip
      "message": "VitisAI EP loaded; /dev/accel/accel0 present",
      "remediation": null,
      "duration_ms": 12
    }, …
  ]
}
```
Exit codes: 0 all-pass, 1 any-fail, 2 usage error, 3 warn-only
(drift). Default TTY view = colored linear list grouped by subsystem;
`--json` is the canonical schema above.

**Crash records**:
- `panic::set_hook` emits a `tracing::error!` with `SpanTrace` and
  writes JSONL to `$XDG_STATE_HOME/sy/crash/<rfc3339>-<pid>.json`.
- Native crashes via systemd-coredump (Fedora default on); `sy doctor`
  surfaces "N cores in last 24 h" by parsing
  `coredumpctl list --json=pretty`.
- `sy crash list` / `sy crash show <ts>` subcommand merges both
  sources with `--json`.

### 4.7 CLI / MCP surface (cross-cutting)

New subcommands:
```
sy service start|stop|restart|status|enable|disable [<name>]
sy service logs <name> [-f] [-n N] [--since …] [--trace <id>] [--json]
sy doctor [--json] [--only=<id-prefix>]
sy stats [--json]                           # metrics snapshot
sy crash list|show <ts> [--json]
sy policy show [--profile=<n>] [--json]
sy policy trust --confirm                   # opts into `trusted`
sy policy grant --tool=<n> --scope=<p> --ttl=<dur>
sy approve <token>
sy ipc ping <endpoint>                      # round-trip check
sy ipc describe <endpoint> [--json]         # capability dump
```

Existing aiplane/knowledge subcommands gain:
- `--priority=Realtime|Interactive|Background|Batch` flag
  (`SY_PRIORITY` env var; default `Interactive`).
- `--deadline=<dur>` (`SY_DEADLINE`).
- `--trace-id=<id>` (`SY_TRACE_ID`); auto-generated if missing.

Exit codes (stable):
```
0 success
1 generic error
2 usage error
3 drift / warning
4 not ready (daemon starting)
5 overloaded / rate-limited
6 consent required
7 policy denied
```

MCP surface: existing `sy stack mcp` and `sy knowledge mcp` (via
`src/knowledge/mcp.rs`) get the envelope upgrade transparently — MCP
tool calls translate into IPC v1 calls under the hood. New MCP tool
`sy_policy_status` returns the resolved policy fingerprint + active
profile (read-only).

### 4.8 Testing strategy

- **Unit**:
  - `sy-ipc` envelope round-trip, version mismatch handling,
    cancel-before-spawn race (LSP-style serial registration).
  - Scheduler admission decisions per class × queue-state.
  - Policy resolver: profile inheritance, per-tool overlay,
    realpath/scope check.
  - Landlock+seccomp filter construction (no syscall; just build the
    ruleset and assert structure).
- **Integration** (daemon-in-thread harness, extending the pattern in
  `scripts/prep_npu_workload.py` and the recent
  `9bd8ba5 prep_npu_workload.py + daemon-in-thread integration test`):
  - End-to-end: CLI → IPC → aiplane → workload, asserting
    `trace_id` propagation through the log stream.
  - Cancellation: issue a long-running request, send `system.cancel`,
    assert worker stops within the 500 ms hard cap.
  - Priority: enqueue Background, then Interactive; assert
    Interactive runs first.
  - Sandbox: spawn a sandboxed `cat /etc/shadow`, assert
    `policy_denied` + audit log line.
- **E2E manual recipe** (in spec):
  - `sy apply` on a fresh user, expect all `sy.target` units active.
  - `sy doctor --json` returns the canonical schema.
  - Kill `sy-qdrant.service` manually; `sy-knowledge.service` is
    torn down by `BindsTo=` and restarted by `Restart=on-failure`.

### 4.9 Migration & compatibility

- **IPC**: hard cutover. `schema_version=1` mandatory; daemons reject
  missing/`null` versions. Acceptable because all daemons + CLI ship
  as one binary; lockstep upgrade.
- **Disk schema**: no qdrant collection schema change in this spec.
  Embedding-model versioning is **out of scope** (separate spec).
- **Existing systemd unit**:
  `configs/systemd/system/sy-knowledge.service` (system-level) is
  moved to `configs/systemd/user/sy-knowledge.service` (user-level).
  `sy apply` removes the old system unit (if present) and installs
  the user unit. Old behaviour preserved via `MemoryHigh=12G`,
  `LimitMEMLOCK=infinity`, `AmbientCapabilities=CAP_IPC_LOCK`.
- **Config files**: existing `configs/sy/agents.toml` stays. New
  `configs/policy/` directory; default `normal` profile applies if
  no per-tool overlay exists.

### 4.10 Dependencies

New (all maintained, audit-clean as of 2026-05):
- `tracing` 0.1, `tracing-subscriber` 0.3, `tracing-journald` 0.3,
  `tracing-appender` 0.2, `tracing-error` 0.2, `tracing-panic` 0.1.
- `metrics` 0.23, `metrics-exporter-prometheus` 0.15 (feature
  `uds-listener`).
- `sd-notify` 0.4 (pure Rust).
- `landlock` 0.4 (kernel 6.7+ for TCP gate; Fedora 43 ships ≥6.11).
- `seccompiler` 0.4 (rust-vmm).
- `rustix` 0.38 — for `SO_PEERCRED`, `openat2`, `prctl`,
  `memfd_create`, fd-sealing.
- `tokio-util` 0.7 — `LengthDelimitedCodec`, `CancellationToken`.
- `crossbeam-channel` 0.5 — scheduler queues.
- `knus` 3.x — KDL parser (only if `configs/policy/` switches to KDL;
  TOML is acceptable too).

System libs already present on the Fedora rice: `systemd`
(`libsystemd` for journal), no extra packages required. `bwrap`
remains *optional* (Zone 4.2 only).

### 4.11 "No snowflakes" check

Every change is repo-resident:
- Unit files under `configs/systemd/user/`.
- Policy under `configs/policy/`.
- `sy apply` symlinks + `systemctl --user daemon-reload`.
- No `~/.bashrc` or manual `systemctl --user enable` outside the repo.
- All Cargo workspace metadata lives in `Cargo.toml` files in the
  repo.

PASS.

### 4.12 CLIG + agent-friendly check

- Every new subcommand has `--help` with examples and `--json`.
- Stable exit codes (0/1/2/3/4/5/6/7), documented in §4.7.
- Non-interactive default when stdin is not a TTY (`sy approve`
  refuses outside a TTY without `--yes` and `--token-from-stdin`).
- Env-var parity (`SY_PRIORITY`, `SY_DEADLINE`, `SY_TRACE_ID`,
  `SY_LOG_FORMAT`, …).
- `--dry-run` on `sy apply` and `sy policy grant`.
- Logs go to stderr; tool output to stdout; primary output stable
  schema with `--json`.

PASS.

## 5. User Journey Sketches

### 5.1 Rice user — first install

1. **Trigger**: clones `sy`, runs `cargo install --path . && sy apply`.
2. `sy apply` renders templates, writes
   `~/.config/systemd/user/sy-{aiplane,knowledge,qdrant,stack-bar,
   agentd}.{service,socket}` + `sy.target`,
   `systemctl --user daemon-reload`,
   `systemctl --user enable --now sy.target`.
3. `sy doctor` returns all-green; aiplane warm-pool is populated for
   VAD/EyeTrack.
4. User opens a popup: `sy stack toggle` works instantly (no
   cold-start because socket-activation already warmed the daemon
   on first connect).

### 5.2 MCP agent — sandboxed tool call

1. **Trigger**: an external LLM (via Claude Code MCP) calls
   `sy_knowledge_search(query=…)`.
2. `sy-knowledge` receives IPC v1 request with `priority=Interactive`,
   stamps `trace_id`, calls aiplane `embed` with the same trace.
3. Aiplane scheduler admits Interactive request; warm embed worker
   runs; trace stays in journal under `SY_TRACE_ID=…`.
4. Result returns within deadline; MCP response shape preserved.

### 5.3 MCP agent — denied destructive command

1. **Trigger**: external LLM tries `sy_run_shell(cmd="rm -rf
   ~/sources/sy")` (hypothetical destructive tool).
2. `sy-agt` resolves policy → `strict` profile (MCP default), `rm`
   not in `exec_allowlist`.
3. Returns `{error.code: "PolicyDenied", details: {...}}`. Audit log
   gets `SY_DECISION=deny` in journald and `audit.jsonl`.
4. LLM sees structured error; user sees `journalctl --user -t
   sy-agent SY_DECISION=deny -o cat` if they look.

### 5.4 Debugging — NPU stalls

1. **Trigger**: user notices STT latency spike.
2. `sy doctor` flags `aiplane.queue.background: WARN (depth=180/256)`.
3. `sy service logs aiplane --trace <id> -f` shows the embed run
   blocking the worker.
4. User runs `sy aiplane cancel <request_id>`; scheduler calls
   `SetTerminate`, worker yields, Interactive STT resumes.

### 5.5 Friction Map

| Friction | Phase | Opportunity |
|---|---|---|
| Migrating one big crate into a workspace will produce a noisy diff; reviewers can't track behaviour changes vs reshuffles. | Zone 1 | Land Zone 1 as **two commits**: (a) introduce workspace shell + `sy-core` + binary, no module moves; (b) move modules one at a time in follow-on PRs. |
| Every IPC call site has to be touched to add `request_id`/`trace_id`/`deadline`. | Zone 2 | Provide `sy-ipc::Client::call(method, params)` that defaults all three; call sites only set what they need. |
| NPU priority is a new mental model for callers — they'll forget to set it. | Zone 3 | Sensible defaults: interactive surfaces (CLI, MCP) default `Interactive`; daemon background tasks default `Background`. Lint at CLI parse time. |
| Sandbox profiles are a new dependency for tool authors. | Zone 4 | Ship a `sy policy lint <tool>` command + `--explain` mode that prints which policy fields would be denied for a hypothetical exec. |
| systemd-user units fail silently if `sy apply` isn't re-run. | Zone 5 | `sy doctor` flags drift between repo units and `~/.config/systemd/user/`; `sy apply --diff` shows pending unit changes. |
| Adding tracing infrastructure can mask perf regressions. | Zone 6 | Build with `tracing` `release_max_level_info` feature; `EnvFilter` defaults to `WARN` outside debug builds. |

## 6. Risks & Mitigation

| Risk | Impact | Likelihood | Mitigation |
|---|---|---|---|
| `RunOptions::SetTerminate` does not actually abort a VAIP-partitioned subgraph mid-run on Ryzen AI. | Interactive starvation: a long Batch run can't be preempted; the SIGKILL fallback restarts the worker but loses warm state. | Medium (no documented support; not confirmed broken). | Land Zone 3 with the SIGKILL fallback always armed; measure VitisAI compile-cache reload latency to size the budget; treat SetTerminate as best-effort. |
| `tracing-journald` silently drops on high-throughput stress. | Lost debug evidence right when you need it. | Low day-to-day; medium during incidents. | Dual-sink (journald + rolling JSONL). Counter `sy_log_journal_drops_total` if we add a wrapper. |
| Cargo workspace explodes incremental rebuild times via `sy-core` hub. | Slower dev loop, contributor friction. | Medium if `sy-core` grows. | Keep `sy-core` to types/errors/policy schema only. Use `cargo-deps` and a CI check to fail if it gains heavyweight deps. |
| Landlock not enforced on older kernels (<6.7 for TCP gate). | Net policy not enforced on stale machines. | Low (Fedora 43 ships 6.11+). | `sy doctor` reports kernel Landlock version; `strict` profile refuses to admit a network rule if kernel doesn't support it. |
| Socket activation hides cold-start latency that surprises waybar. | Waybar tile flicker on first click after idle. | Medium. | Keep `sy-aiplane` warm-always (don't socket-activate it). Knowledge facade can be socket-activated since 1-3 s cold-start is acceptable for a `sy knowledge search`. |
| KDL policy authoring is unfamiliar; users write subtly wrong policies. | False sense of security. | Medium. | `sy policy lint` + `sy policy explain --tool=… --argv='…'` simulator that prints the would-be decision. |
| amdxdna NPU does not expose userspace priority — strict-priority queues are *application-level only*. | Two `sy` instances (or a misbehaving third-party app using the NPU) bypass scheduler. | Low on a single-user rice; medium if shared. | Document the limitation; `sy doctor` flags external NPU consumers via `/proc/*/fd` scan. |
| Workspace split lands but `main.rs` still grows business logic. | God-binary risk re-emerges. | Medium without discipline. | CI lint: `src/main.rs` LOC budget < 400; `main.rs` may only `match` subcommands and delegate. |
| Consent UX via `sy approve <token>` is too friction-heavy and users default to `trusted`. | Sandbox becomes theatre. | Medium. | Make `normal` profile actually usable for common tools (rg, cargo, git); reserve `trusted` for an explicit "I'm yolo-ing" flag with a TTY confirmation. |

## 7. Open Questions

1. **Cold-reload latency from VitisAI compile cache** — needs
   measurement on Phoenix and Strix. Determines warm-pool eviction
   thresholds and SIGKILL-restart budget.
2. **Does `SetTerminate` actually unblock a VAIP-partitioned subgraph
   mid-run?** Needs a probe test before relying on it.
3. **Strix Point fairness** — does the firmware Resource Solver
   actually give a foreground (Interactive) request priority over a
   background process holding a HW context? May need an experiment.
4. **KDL vs TOML for `configs/policy/`** — TOML is already used
   in the rest of `configs/sy/`. Switching policy to KDL adds a parser.
   Recommend TOML for consistency unless KDL's nested-block syntax is
   really preferred for readability.
5. **Renaming `sy stack mcp` → `sy mcp stack`?** Group all MCP
   surfaces under `sy mcp <subsystem>` for discoverability. Defer
   until other MCP surfaces exist.
6. **Should `sy-ipc` support streaming responses** (current
   `DaemonEvent` shape from agent) or only request/response?
   Recommend streaming as a v1 capability negotiated via `describe`
   (some daemons opt out).
7. **socket-activation vs warm-always per daemon** — knowledge facade
   is the only obvious candidate; everything else stays warm. Confirm
   once cold-start numbers are in hand.

## 8. Hand-off

- **Journey**: run `/journey` against this spec → produce
  `specs/journeys/JOURNEY-<dt>-architecture-refactor.md`.
  Because this is six concurrent zones, consider one journey per
  zone (six files) rather than a megajourney.
- **Roadmap**: run `/roadmap` per journey → six roadmap directories
  under `specs/roadmaps/arch-{workspace,ipc-v1,aiplane-scheduler,
  agent-sandbox,supervision,observability}/`.
- **Implement**: `/implement` per roadmap step; the existing
  daemon-in-thread integration-test pattern is reused everywhere.
- **NPU model work** is unchanged; the existing `/npu-prep` skill
  still applies to any new model added under the new scheduler.
- **Workload scaffolding** continues via `/workload`; the new
  `Request`/`Priority` fields in `sy-core` extend (not replace) the
  current `Workload` trait.

---

### Appendix A — Per-zone "first commit" sketch

Just so the orchestrator has a concrete starting cut for each zone:

**Z1 (workspace)** — first commit:
- Convert root `Cargo.toml` to `[workspace] members = [".",
  "crates/sy-core"]` virtual + member layout.
- Create `crates/sy-core/` with `Priority`, `WorkloadKind`,
  `ErrorCode`, IPC envelope types (cut/pasted from existing
  `src/aiplane/ipc.rs` + `src/aiplane/registry.rs`).
- `src/` stays under root; root crate depends on `sy-core`.
- No behaviour change. CI green.

**Z2 (IPC v1)** — first commit:
- Add `crates/sy-ipc/` with envelope serde types, framing codec,
  `Client::call`, `Server::serve`, `SO_PEERCRED` gate.
- One daemon (`sy-knowledge`) gets migrated to v1; others stay on
  legacy temporarily.
- Add `system.describe` and `system.health` to the migrated daemon.

**Z3 (scheduler)** — first commit:
- `Priority` enum (already in `sy-core`).
- `sy-aiplane::scheduler` with 4 queues + admission rules + the
  Triton-style `ModelQueuePolicy`.
- Replace the global `Mutex<()>` `SessionPool` lock with the
  scheduler's dispatcher.
- Existing FIFO behaviour preserved when all callers pass
  `priority=Interactive` (compat shim).

**Z4 (sandbox)** — first commit:
- `configs/policy/profiles/{strict,normal,trusted}.kdl` (or .toml).
- `sy-agt::policy::Resolver` (no enforcement yet, just parses +
  exposes `decide(tool, argv)`).
- `sy policy show --json` and `sy policy lint`.
- Enforcement (Landlock + seccomp + systemd-run scope) lands in the
  *second* commit so the first is reviewable as policy-only.

**Z5 (supervision)** — first commit:
- `configs/systemd/user/sy-{aiplane,knowledge,qdrant,stack-bar,
  agentd}.service` + `sy-knowledge.socket` + `sy.target`.
- `sy apply` learns to symlink + `daemon-reload`.
- `sy service start|stop|status` wrapping `systemctl --user`.
- Keep `Type=simple` initially; flip to `Type=notify` once `sd-notify`
  is wired in daemon main loops (second commit).

**Z6 (observability)** — first commit:
- Add `tracing` + `tracing-subscriber` + `tracing-journald` +
  `tracing-appender` + `tracing-error` to top-level deps.
- One subscriber-init function in `sy-core::obs::init(mode: Cli |
  Daemon { name })`.
- Replace `eprintln!` with `tracing::{info,warn,error}!` in
  `aiplane::supervisor` and `knowledge::daemon`.
- `sy doctor` skeleton with one or two checks. Metrics + crash
  hooks follow in subsequent commits.

### Appendix B — Glossary

- **HW context** (XDNA): firmware-tracked workload context on the
  NPU. Phoenix=6, Strix=16. Multi-process is OK; multi-ORT-session in
  one process is broken (RyzenAI-SW #223).
- **Priority class**: caller-declared QoS tier
  (`Realtime|Interactive|Background|Batch`). Maps to per-class
  bounded queue and timeout action.
- **Warm worker**: spawned process holding a loaded model with hot
  weights; its NPU HW context is bound. Warm-always vs idle-TTL vs
  LRU-capped per workload kind.
- **trace_id**: 16-byte hex identifier carried in the IPC envelope
  (W3C `traceparent` compatible). Stamped on every log line so
  `journalctl SY_TRACE_ID=<id>` stitches the chain.
- **Profile**: KDL/TOML file under `configs/policy/profiles/`
  declaring the default capability set for an actor (sy-agent
  invocation). Three ship: `strict`, `normal`, `trusted`.
