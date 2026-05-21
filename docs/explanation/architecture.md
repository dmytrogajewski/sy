<!-- Template source: Good Docs Project explanation template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/explanation. Diátaxis quadrant: explanation. -->

# How the planes fit together

This page is for readers who already know *what* `sy` does and want
to understand *why* it is shaped the way it is. It does not teach
you any new command. If you are looking for steps, start with
[the getting-started tutorial](../tutorials/getting-started.md);
if you are looking for flags and exit codes, start with
[the CLI reference](../reference/cli.md).

## Why this exists

`sy` is an agentic OS layer for Fedora 43. Its job is to make a
stock laptop behave like an agent-first workstation: one place
that owns the NPU, one place that runs sandboxed agents, one place
that indexes your files, one place that picks a power profile,
one place that renders the desktop. The shape of `sy` follows from
two commitments the project refuses to walk back.

The first commitment is **no snowflakes**. Every change to your
machine is encoded as a declarative artefact under `configs/` or
as code inside `sy` itself. A fresh laptop plus `cargo build
--release` plus `sy apply` must reproduce the entire system. If
the only way to make something work is "now manually edit
`~/.bashrc`", the design is wrong and the design changes.

The second commitment is **agent-first ergonomics**. An agent has
to drive `sy` the same way a human does: through a small set of
discoverable subcommands with machine-readable output, stable
exit codes, and no hidden interactive prompts. That, in turn,
demands a single CLI surface, a single configuration surface, and
a single supervision surface.

Both commitments push the project toward one binary and a small
number of long-running services that all speak the same wire
format. That is what `sy` is, and the rest of this page explains
why that shape beats the obvious alternatives.

## How it works

### One binary, many planes

`sy` ships as a single Rust binary. Every capability — NPU
inference, semantic search, agent sandboxing, power management,
the layer-shell bar, phone-as-key sudo — is a subcommand of that
binary. A *plane* is the long-running service behind one of those
capabilities. The `aiplane` is the NPU plane. The `knowledge`
plane is the semantic-search plane. The `agt` plane is the agent
runner. The `power` plane is the adaptive governor. The `stack`
plane is the bar.

A plane is not a separate program. It is a mode the single
binary runs in when invoked with the right subcommand
(`sy aiplane daemon`, `sy knowledge daemon`, and so on). The
binary knows how to be every plane; the systemd unit decides
which plane it actually becomes.

Going single-binary keeps the user model small. There is one
artefact to build, one artefact to install, one artefact to
upgrade, one artefact to package, one `--help` to learn the
shape of. It also keeps the contract between planes inside one
repository, so changes to a wire format land alongside the
producers and consumers in the same commit.

### Planes talk to each other over local sockets

The planes do not call each other in-process. They talk over Unix
domain sockets in `$XDG_RUNTIME_DIR`, exchanging JSON envelopes.
The knowledge plane does not link the aiplane crate; it sends an
`embed` request to the aiplane's socket and waits for the
response. The agent runner does the same when it wants a search.

A socket boundary between planes seems heavier than a function
call, and at one level it is. The payoff is that every plane
boundary is also the boundary an agent can speak to. The same
socket the knowledge daemon uses to ask the aiplane for an
embedding is the socket you can drive from `socat` to debug a
stalled run, the socket an MCP server proxies tool calls onto,
and the socket an external script can hit when it wants the
exact same answer the daemon would give itself. The architecture
has no "internal API that bypasses the wire" — there is only the
wire.

Because every payload is JSON, the wire is legible. Because every
plane carries a request identifier through to its logs, a single
trace stitches together the chain across planes. Because every
plane gates connections on the caller's user ID, no other user
on the host can reach a plane by guessing a path.

### The NPU plane is a single owner

The NPU is a special piece of hardware. AMD's XDNA driver
exposes `/dev/accel/accel0` to user space, and the runtime that
sits on top — ONNX Runtime with the VitisAI execution provider —
does not survive two processes (or two ORT sessions in the same
process) racing for the device. The way to keep the NPU healthy
is to give it exactly one owner.

That owner is the `aiplane` daemon. Every other plane that wants
NPU work — the knowledge plane embedding a chunk, the agent
runner calling a workload, a one-shot CLI invocation — sends the
request to the aiplane's socket. The aiplane queues the request,
admits it, dispatches it to a warm worker holding the model, and
returns the result. There is no "just open another session for
this one call" path, and there never will be.

A consequence of that ownership is that the aiplane has to be a
real scheduler, not a mutex around the device. It admits work in
priority classes (an audio frame for voice activity detection
cannot wait behind a bulk-embedding pass), it keeps the
expensive models warm in process-per-workload workers, and it
times out work that overruns its deadline. The user-visible
surface — `sy aiplane run --workload …` — hides all of that, but
the reason it is fast on a warm path and graceful under pressure
is that scheduler.

### The agent runner is a sandbox

The agent runner exists because an agent on your machine is a
program you partly trust to do useful work and partly do not
trust to do harmful work. Both halves are real. The runner takes
that split seriously by treating "what a tool may read", "what a
tool may write", "what a tool may exec", and "what a tool may
reach over the network" as policy, not as honour-system
documentation.

Policy lives in declarative profile files. A `strict` profile
locks a tool down to its repository and denies the network. A
`normal` profile is the day-to-day default for interactive use.
A `trusted` profile is opt-in and requires a confirmation from a
real terminal. Inside the runner, the policy turns into a
Landlock ruleset, a seccomp filter, a `NoNewPrivileges` bit, a
scrubbed environment, and a `systemd-run --user --scope` for
cgroup caps. Every decision the runner makes is mirrored to the
journal so you can replay it with `journalctl`.

Sandboxing belongs in the host, not the model. The Model Context
Protocol specification is explicit about that: the host is
responsible for confining what its servers can do. `sy` is the
host, so `sy` is what enforces.

### The knowledge plane is a consumer, not a peer

The knowledge plane runs a local vector index (qdrant under the
hood) and a daemon that ingests files from registered roots,
embeds them, and answers searches. It is presented to you as a
peer of the aiplane, but architecturally it is a *consumer*.
Knowledge depends on the aiplane to turn chunks of text into
vectors; the aiplane does not depend on knowledge at all.

Keeping the dependency one-directional matters. It means the
aiplane can ship without ever knowing what a "source" or a
"manifest" or a "scheduled sync" is. It means you can run the
aiplane on its own for unrelated NPU workloads. It means a
future second consumer of the embeddings — say, a tag-aware
photo grouper — does not have to learn the knowledge plane's
vocabulary. The wire between knowledge and aiplane is the same
JSON envelope every other plane uses; nothing about knowledge is
privileged.

### `sy.target` is the supervisor

Every plane that needs to be running for `sy` to feel like a
system runs as a user-level systemd unit, grouped under a single
unit called `sy.target`. Enable the target and the planes come
up at login. Stop the target and they go away. Restart one plane
and the rest are unaffected. Crash one plane and systemd brings
it back. Inspect one plane and `journalctl --user -u <unit>`
already knows the answer.

Reaching for systemd, instead of writing a `sy-supervisord`, is
deliberate. systemd already solves restart policies, watchdogs,
socket activation, ordering, cgroup resource limits, log
collection, and crash dump handling. Reusing it costs `sy` a few
unit files in `configs/systemd/user/` and gives back decades of
operational maturity. The price is that everything `sy`
supervises has to run as a user unit, which is true and
intentional: `sy` is a single-host single-user tool.

### Configuration is rendered, not edited

The whole `configs/` tree is a set of minijinja templates. The
templates take their palette and their host-shaped knobs from a
small `sy.toml` and from per-theme files under `themes/`. Running
`sy apply` walks the tree, renders each template, writes it into
the right destination under your home directory, and runs
`systemctl --user daemon-reload`. Running `sy apply --dry-run`
shows you the diff first.

Render-not-edit is what makes the no-snowflakes rule liveable.
You never hand-edit a waybar style or a niri keybinding directly
in `~/.config/`; you edit it in the repository and re-apply. The
repository is the single source of truth and the only thing you
need to back up to recreate the system.

## Trade-offs

Every architecture pays for what it does well. The shape above
makes a small number of decisions clearly, and those decisions
have visible downsides.

- **Single binary trades modularity for cohesion.** Every plane
  ships and upgrades together, every plane shares one set of
  dependencies, every plane lives in one repository. The cost
  is that the binary is larger than any one plane would be on
  its own and a change in any plane recompiles the whole tree.
  The win is that there is exactly one version of `sy` on your
  machine and no skew between planes is possible. For a
  single-user OS layer, cohesion is the better trade.

- **Plane-by-socket trades latency for legibility.** A function
  call would be faster than a socket round-trip and would avoid
  the JSON encode and decode. The win is that every plane
  boundary is debuggable from the shell, replayable from a
  script, and reachable from an external agent without a custom
  bridge. On the local socket the latency cost is in
  microseconds; the legibility win is durable.

- **Hard NPU ownership trades sharing for stability.** The
  aiplane will not let a second process touch
  `/dev/accel/accel0` while it is running. A program that
  expected to "just open ORT directly" cannot, even if its
  intent is benign. The win is that the NPU stays alive instead
  of getting wedged by a session race. Given how expensive a
  recovery is (cache invalidation, model reload, lost warm
  state), the stricter contract is worth the lost flexibility.

- **Policy-driven sandboxing trades convenience for blast
  radius.** Adding a new tool means writing or extending a
  profile. A tool that is not in any profile cannot run. The
  win is that an agent cannot exfiltrate something it was never
  authorised to read or call a binary it was never authorised
  to exec. For a host that an LLM may drive, that is the only
  acceptable default.

- **`systemd --user` trades portability for power.** `sy` runs
  on Linux distributions that ship a current systemd; it does
  not run on macOS or on a Linux without a user manager. The
  win is that the supervision story is `Restart=on-failure`,
  `WatchdogSec=`, and `journalctl --user -u …`, all of which
  the platform already understands. For a Fedora-shaped target,
  the lost portability is theoretical.

- **Render-not-edit trades immediate feedback for
  reproducibility.** Changing a single colour is a template
  edit plus an apply, not a one-line tweak. The win is that
  your machine is always the apply of your repository. There
  is no drift to forget about, nothing to commit later, nothing
  to lose when the laptop dies.

## Alternatives we considered

These are the shapes the project looked at and rejected, with
the reason the rejection still holds.

- **A per-plane process tree of independent binaries**
  (`sy-aiplane`, `sy-knowledge`, `sy-stack-bar`, `sy-agentd` as
  separate crates installed separately). It is the obvious
  microservices framing for a Linux desktop layer. The
  rejection is upgrade skew. With multiple binaries on multiple
  release cycles, a wire-format change has to ship a
  compatibility window, and every user is one missed upgrade
  away from a broken plane. With one binary, the wire format
  changes once and every consumer changes with it.

- **A pure-systemd composition with no custom supervisor at
  all**, where each plane is its own package and the only thing
  `sy` ships is a meta-target. This is the closest "do less"
  answer. The reason it does not work is that the planes need
  to share a wire-format crate, a policy crate, an observability
  story, and an apply-from-source story. Without a single
  binary to host all of that, the shared code has nowhere to
  live except as published libraries each package vendors at a
  different version. Single binary wins again.

- **A custom supervisor (`sy-supervisord`) that owns the planes
  directly.** This was attractive in early sketches because it
  would let `sy` express plane-shaped concepts (a "degraded"
  state, a "needs-warming" state) natively. The rejection is
  duplication. Every concept the supervisor would invent is
  already in systemd, and reinventing them in worse versions
  trades operational maturity for a small expressiveness gain.
  Instead, the planes lean on systemd primitives
  (`Type=notify`, `BindsTo=`, `WatchdogSec=`) and add their
  status vocabulary on top via the `system.health` envelope
  method.

- **Cap'n Proto or gRPC on the local socket** instead of JSON.
  Both would be faster and would carry richer schema evolution.
  The rejection is debuggability. A frame you can hand-write
  with `socat` and pretty-print with `jq` is a frame an agent
  can also synthesise without bespoke client tooling. On a
  local socket where the round-trip is microseconds, the
  encoding cost does not move the needle; the loss of legibility
  would. JSON-RPC stays.

- **NVIDIA CUDA as the embedding backend** instead of (or
  alongside) the AMD NPU. The early version of `sy` did exactly
  this, using `fastembed` on a discrete GPU. The rejection is
  that the GPU should be free for the LLMs you actually want
  running on it. Putting an embedding pass on the NPU costs
  some MTEB quality (the move to `multilingual-e5-base` from
  `-large` loses about 6 % on average) and buys back the GPU
  for inference work where the speed difference is real.

- **Allowing an agent to flag its own calls as auto-approved**
  (the pattern other agent runners ship under names like
  `requires_approval`). The rejection is that the flag is set
  by the same model the approval is supposed to protect against.
  Other projects have shown the field is exploitable through
  prompt injection in seemingly trustworthy files. Consent in
  `sy` is either a pre-issued grant the human created from a
  real terminal or a per-call confirmation the human handles
  live. The model is never asked whether it should be trusted.

- **A bash-string command surface** for the agent runner. It
  would make policy easier (`allow tool to run rg foo`). The
  rejection is that allowlisting shell strings is in practice
  an open door, because the shell's word splitting, environment
  expansion, and chained redirection mean the allowlist's
  intent does not survive contact with real inputs. The runner
  exposes exec as an explicit `(binary, argv[])` and the
  policy gates that, not a quoted string.

## See also

- [Tutorial: bring up sy on a fresh Fedora 43 laptop](../tutorials/getting-started.md)
- [Reference: the sy CLI](../reference/cli.md)
- The internal design document this page is grounded in lives at
  `specs/research/architecture-refactor/SPEC.md` in the
  repository. That document is for contributors who need the
  exact wire format, the queue depths, the cancellation
  protocol, and the migration plan. This page is for everyone
  else.
