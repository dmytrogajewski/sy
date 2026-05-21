<!-- Template source: Good Docs Project glossary template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/glossary. Diátaxis quadrant: reference. -->

# Glossary

One-line definitions for terms `sy` uses in a project-specific way.
If a term needs more than one line, the entry links to the canonical
explanation, reference, or architectural source. Entries are
alphabetised by the headword; case and punctuation in the headword
are ignored for sort order.

Use this page when a `sy` doc, log line, or `--help` blurb uses a
word you have not seen before. For the user-facing mental model of
how the named things fit together, see
[`How the planes fit together`](../explanation/architecture.md). For
the wire-level contract, see
[`specs/research/architecture-refactor/SPEC.md`](../../specs/research/architecture-refactor/SPEC.md)
in the repository.

## Terms

### agt

The sandboxed agent runner plane: `sy agt` executes coding or
inference agents under an intent-whitelisted Landlock + seccomp
sandbox driven by per-profile policy files. See
[`How the planes fit together` — The agent runner is a sandbox](../explanation/architecture.md#the-agent-runner-is-a-sandbox).

### aiplane

The privileged NPU inference plane: the single-owner daemon that
holds `/dev/accel/accel0`, hosts every on-device ML workload, and
serves requests over the local IPC socket. See
[`How the planes fit together` — The NPU plane is a single owner](../explanation/architecture.md#the-npu-plane-is-a-single-owner).

### BF16

The bfloat16 floating-point format AMD's Quark uses to quantise
ONNX weights for the NPU embed workload; trades FP32 dynamic range
for half the bytes so the `multilingual-e5-base` graph fits under
VitisAI EP's 2 GiB ModelProto cap. Referenced from
[`README.md` — NPU one-time setup](../../README.md#npu-one-time-setup).

### IPC v1

The canonical JSON-over-Unix-socket envelope every plane speaks
(envelope shape, `LengthDelimitedCodec` framing, cancellation token,
reserved `describe`/`health`/`cancel` methods); see
[`specs/research/architecture-refactor/SPEC.md` §4.2](../../specs/research/architecture-refactor/SPEC.md).

### knowledge (plane)

The local semantic-search plane: an embedded qdrant vector index
plus a scheduler that ingests registered file trees, embeds them by
delegating to the aiplane, and answers searches over IPC v1 or via
its MCP server. See
[`How the planes fit together` — The knowledge plane is a consumer, not a peer](../explanation/architecture.md#the-knowledge-plane-is-a-consumer-not-a-peer).

### MCP

[Model Context Protocol](https://modelcontextprotocol.io) — the
stdio JSON-RPC dialect agents (Claude, Cursor, Codex, Gemini) speak
to discover and call tools; `sy` exposes `sy knowledge mcp` and
`sy power mcp` servers and `sy auto` plumbs them into each agent's
config.

### plane

A long-running service hosted by the single `sy` binary, identified
by its top-level subcommand (`sy aiplane`, `sy knowledge`,
`sy power`, `sy agt`, `sy stack`); all planes share one CLIG +
JSON-over-stdio surface so agents drive any plane the same way a
human does. See
[`How the planes fit together` — One binary, many planes](../explanation/architecture.md#one-binary-many-planes).

### power (plane)

The `sy-powerd` user daemon: a power-profiles-daemon shim layered
with a contextual bandit that picks cpufreq governor, EPP, turbo,
and the `net.hadess.PowerProfiles` D-Bus profile from
[`configs/sy/power.toml`](../../configs/sy/power.toml). Reachable
from CLI (`sy power status`) and via MCP (`sy power mcp`).

### Quark

[AMD Quark](https://quark.docs.amd.com/) — the quantisation toolkit
the `scripts/prep_npu_workload.py` pipeline uses to BF16-quantise
the exported ONNX graph before VitisAI compiles it for the NPU.

### re-exec dance

The startup ritual `aiplane::reexec` performs before any thread
spawns: detects the AMD venv, sets `LD_LIBRARY_PATH`,
`ORT_DYLIB_PATH`, and the `RYZEN_AI_*` env to AMD's bundled
`libonnxruntime.so` plus `voe`/`flexml`/`vaimlpl_be`/`flexmlrt`/
`xrt` directories, then execs itself. Required because `setcap` on
the binary would set `AT_SECURE` and the dynamic linker would drop
`LD_LIBRARY_PATH`; see [`AGENTS.md` — NPU-specific norms](../../AGENTS.md#npu-specific-norms).

### rice

The desktop look-and-feel produced by rendering
[`configs/`](../../configs/) (niri, waybar, mako, fuzzel, foot,
swaylock, yazi, …) through minijinja templates with the active
palette from `themes/<name>.toml`; running `sy apply` is what
materialises the rice into `~/.config/`. See
[`README.md` — Rice](../../README.md#rice--niri--waybar--).

### session pool

The aiplane component that owns the warm process-per-workload
workers, decides at start-up which execution provider (`Vitisai` or
`Cpu`) each workload loads, and keeps expensive ONNX sessions warm
between requests so a one-shot CLI invocation does not pay
cold-start cost. See
[`specs/research/architecture-refactor/SPEC.md` §Zone 3 — Aiplane scheduler](../../specs/research/architecture-refactor/SPEC.md).

### snowflake

A one-off manual change to the host that lives outside the
repository: a hand-edited dotfile, an ad-hoc `systemctl enable`, a
bespoke env var. Snowflakes are prohibited; every environment
change ships under [`configs/`](../../configs/) or inside the `sy`
binary so `cargo build --release && sy apply` on a fresh machine
reproduces the system. See [`CLAUDE.md` — Core rule: no snowflakes](../../CLAUDE.md#core-rule-no-snowflakes).

### stack-bar

The layer-shell waybar replacement (`sy stack bar`) that renders
the bar tiles (NPU, GPU, network, battery, syauth, …) as a single
wayland layer-shell client driven by the same `sy` binary that
hosts every other plane.

### sy.target

The user-level systemd target that groups every `sy` plane
(`sy-aiplane.service`, `sy-knowledge.service`, `sy-powerd.service`,
`sy-stack-bar.service`, `sy-agentd.service`); enabling
`sy.target` brings every plane up at login, stopping it tears them
down. Units live under [`configs/systemd/user/`](../../configs/systemd/user/).
See [`How the planes fit together` — `sy.target` is the supervisor](../explanation/architecture.md#sytarget-is-the-supervisor).

### syauth

[syauth](https://github.com/dmytrogajewski/syauth) — phone-as-key
sudo: a PAM module plus a user daemon plus an Android app, wrapped
by `sy syauth` (install-pam, doctor, status) and rendered as a
waybar pill by the stack-bar.

### VitisAI EP

The [AMD VitisAI execution provider](https://onnxruntime.ai/docs/execution-providers/Vitis-AI-ExecutionProvider.html)
for ONNX Runtime; the aiplane prefers it for NPU workloads and
falls back only to CPU on workloads that declare
`EP::Cpu`. Notable constraint: VitisAI EP 1.7.1 caps internal
ModelProto serialisation at 2 GiB, which is why the embed workload
uses `multilingual-e5-base` (768-dim) instead of `-large`. See
[`README.md` — Why `multilingual-e5-base`, not `-large`?](../../README.md#why-multilingual-e5-base-not--large).

### workload

A `Workload` trait impl under `src/aiplane/workloads/` (`embed`,
`rerank`, `vad`, `stt`, `ocr`, `fake`) that declares an EP
preference (`Vitisai | Cpu`), an input/output shape, and the ORT
session bring-up logic; the aiplane registers each workload at
start-up and the session pool dispatches requests to the matching
warm worker. End-to-end coverage lives in
`tests/` per the [`AGENTS.md` non-negotiable](../../AGENTS.md#non_negotiables).

### XDNA

The [AMD XDNA architecture](https://www.amd.com/en/technologies/xdna.html)
and DKMS kernel module that exposes the Ryzen AI NPU as
`/dev/accel/accel0`; the kernel module ships from the
[`ryzenai-rpm`](https://github.com/dmytrogajewski/ryzenai-rpm)
companion repository as documented in
[`README.md` — NPU one-time setup](../../README.md#npu-one-time-setup).

## See also

- [Explanation: how the planes fit together](../explanation/architecture.md)
- [Reference: `sy` CLI](cli.md)
- [Reference: syauth PAM module](syauth-pam-module.md)
- [`AGENTS.md`](../../AGENTS.md) for the coding-agent persona, the
  NPU-specific norms, and the file layout.
- [`CLAUDE.md`](../../CLAUDE.md) for the no-snowflakes rule and the
  CLIG + agent-friendly CLI contract.
