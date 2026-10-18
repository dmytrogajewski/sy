<!-- Template source: Good Docs Project glossary template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/glossary. Diátaxis quadrant: reference. -->

# Glossary

One-line definitions for terms `sy` uses in a project-specific way.
If a term needs more than one line, the entry links to the canonical
explanation, reference, or architectural source. Entries are
alphabetised by the headword; case and punctuation in the headword
are ignored for sort order.

Use this page when a `sy` doc, log line, or `--help` blurb uses a
word you have not seen before. For what `sy` is and what you can
do with it, see [`What sy is`](../explanation/what-sy-is.md). For
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
[Agent-first CLI](../explanation/agent-first-cli.md).

### aiplane

The privileged NPU inference plane: the single-owner daemon that
holds `/dev/accel/accel0`, hosts every on-device ML workload, and
serves requests over the local IPC socket. See
[Architecture: communication between features](../explanation/architecture.md#communication-between-features).

### BF16

The bfloat16 floating-point format AMD's Quark uses to quantise
ONNX weights for the NPU embed workload; trades FP32 dynamic range
for half the bytes so the `multilingual-e5-base` graph fits under
VitisAI EP's 2 GiB ModelProto cap. Referenced from
[NPU setup guide](../how-to/set-up-npu.md).

### doctor

Cross-plane health probes: `sy doctor` walks a linear check list
and exits `0` (all pass), `1` (any fail), `2` (usage), or `3`
(warn-only drift). JSON document is `sy.doctor/v1`. See
[how to run sy doctor](../how-to/run-doctor.md).

### file (plane)

The in-tree iced file manager (`sy file`): window, plugin
previewers, MCP mirror, and its own doctor schema.
Opens from `Mod+E` on niri. See
[browse files with sy file](../tutorials/browse-your-files.md).

### IPC v1

The canonical JSON-over-Unix-socket envelope every plane speaks
(envelope shape, `LengthDelimitedCodec` framing, cancellation token,
reserved `describe`/`health`/`cancel` methods); see
[`specs/research/architecture-refactor/SPEC.md` §4.2](../../specs/research/architecture-refactor/SPEC.md).

### knowledge (plane)

The local semantic-search plane: an embedded qdrant vector index
plus a scheduler that ingests registered file trees, embeds them by
delegating to the AI service, and answers searches over its CLI and
MCP interfaces. See
[Architecture](../explanation/architecture.md#communication-between-features).

### MCP

[Model Context Protocol](https://modelcontextprotocol.io) — the
stdio JSON-RPC dialect agents (Claude, Cursor, Codex, Gemini) speak
to discover and call tools. `sy` exposes `sy knowledge mcp`,
`sy mon mcp`, and `sy file mcp`. `sy auto` writes
those servers into each agent's config.

### mon

On-demand Wayland layer-shell health dashboard (`sy mon`), backed
by a 1 Hz aggregator. `Super+m` toggles the popup;
`sy mon snapshot --json` is the machine document. See
[`docs/agents/mon-schema.md`](../agents/mon-schema.md).

### plane

A feature area in sy, such as power, AI inference, knowledge search,
or file management. A feature may provide commands, a user service,
or both. Feature executables are packaged separately and routed
through the `sy` launcher. See
[Architecture](../explanation/architecture.md#shared-entry-point-separate-features).

### plugin

A small program `sy file` starts to preview a MIME type. Installed
with `sy plugin install`, checked with `sy plugin doctor`. See
[how to write a sy plugin](../how-to/write-a-sy-plugin.md).

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
swaylock, …) through minijinja templates with the active
palette from `themes/<name>.toml`. Running `sy apply` is what
writes that look into `~/.config/`. See
[Architecture: configuration and supervision](../explanation/architecture.md#configuration-and-supervision).

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
change ships under [`configs/`](../../configs/) or in the relevant
feature crate and RPM package, so package installation and `sy apply`
can reproduce the system. See [`CLAUDE.md` — Core rule: no snowflakes](../../CLAUDE.md#core-rule-no-snowflakes).

### spark

Remote DGX Spark model-serving plane: `sy spark <host>` talks to an
authenticated agent that owns HTTPS and desired state; a root
executor owns Docker. Install with
[How to install the Spark agent](../how-to/install-spark.md).
Engines run on an internal bridge; the
gateway publishes OpenAI- and Anthropic-compatible routes after
health and identity probes. See
[Spark reference](spark.md).

### stack-bar

The optional layer-shell bar (`sy stack bar`) that renders status
tiles as a Wayland layer-shell client. The `sy-desktop-shell` package
provides its executable and related applets.

### sy.target

The user-level systemd target that groups enabled sy services.
Feature packages provide their own units; apply links and enables the
units for installed features. Units live under
[`configs/systemd/user/`](../../configs/systemd/user/). See
[Architecture: configuration and supervision](../explanation/architecture.md#configuration-and-supervision).

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
[NPU setup guide](../how-to/set-up-npu.md).

### workload

A `Workload` trait impl under `crates/sy-aiplane/src/aiplane/workloads/` (`embed`,
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
[NPU setup guide](../how-to/set-up-npu.md).

## See also

- [Explanation: how the planes fit together](../explanation/architecture.md)
- [Reference: `sy` CLI](cli.md)
- [Reference: syauth PAM module](syauth-pam-module.md)
- [`AGENTS.md`](../../AGENTS.md) for the coding-agent persona, the
  NPU-specific norms, and the file layout.
- [`CLAUDE.md`](../../CLAUDE.md) for the no-snowflakes rule and the
  CLIG + agent-friendly CLI contract.
