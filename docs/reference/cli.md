# `sy` CLI reference

Flags, exit codes, environment variables, and examples for the `sy`
planes and applets. The clap `--help` text in the binary is the
source of truth if this page and the code disagree.

For walkthroughs, see [tutorials](../tutorials/) and
[how-to guides](../how-to/). For the cross-cutting contract
(`--json` / `--dry-run` / exit codes / `NO_COLOR` / `SY_*`), start
with [global conventions](#global-conventions) below.

> Template: Good Docs Project reference. Source attribution and the
> Diátaxis reference quadrant are described in
> [`.agents/skills/documenter/SKILL.md`](../../.agents/skills/documenter/SKILL.md).

## Global conventions

`sy` follows the [CLIG](https://clig.dev/) + agent-friendly contract
documented in [`CLAUDE.md`](../../CLAUDE.md) and
[`AGENTS.md`](../../AGENTS.md). The rules below describe the contract;
which subcommands actually honour each rule is called out per
subcommand.

### Global flags

These flags are accepted by **every** subcommand because they are
declared with `global = true` on the top-level [`Cli`] struct in
`src/main.rs`:

| Flag       | Type   | Default                                    | Env var   | Description                                                                                       |
|------------|--------|--------------------------------------------|-----------|---------------------------------------------------------------------------------------------------|
| `--root`   | path   | walk up from cwd for `configs/` + `themes/`| `SY_ROOT` | Override the repo root that template-rendering subcommands resolve against.                       |
| `--target` | path   | `$XDG_CONFIG_HOME` or `~/.config`          | —         | Override the target directory `sy apply` writes into.                                             |

`--help` (`-h`) and `--version` are added automatically by clap on
every subcommand.

### Output streams

- Primary output: stdout.
- Logs and diagnostics: stderr (the tracing subscriber installed by
  `sy_core::obs::init(Mode::Cli)` in `src/main.rs`).
- Errors that map to a typed exit code (see below) print a single line
  on stderr formatted as `error: <message>` before exiting.

### `--json`

`--json` is opt-in per subcommand. Coverage by plane (verified against
source):

- **`sy apply`** — `--json` and `--diff` (alias for `--dry-run --json`).
- **`sy aiplane`** — `status`, `list`, `run`, `cancel`.
- **`sy knowledge`** — `list`, `index`, `search`, `manifests`, `status`,
  `bench`, `mcp-enable`, `mcp-disable`, `mcp-status`.
- **`sy power`** — `status`, `log`, `explain`, `show`, `list-profiles`.
- **`sy doctor`** — emits the SPEC §4.6 `sy.doctor/v1` schema.
- **`sy crash`** — `list`, `show`.
- **`sy ipc`** — `ping`, `describe` (default for `describe`).
- **`sy service`** — `status`.
- **`sy policy`** — `show`, `lint`, `explain`, `grant`.
- **`sy approve`** — JSON response envelope.
- **`sy agt`** — `list`, `diag`.
- **`sy auto`** — `configure`, `list-detectors`.
- **`sy stack`** — `push --json`, `list --json`.
- **`sy spark`** — `install`, `status`, `doctor`, `operations`, `token`,
  `download`, `serve`, `launch`, `ps`, `logs`, `stop`, `ls`, `show`,
  `rm`, `client-config`, `cert status`.
- **`sy file`** — `doctor`. `sy file ipc <op>` prints the JSON
  response envelope by default.
- **`sy plugin`** — `list`, `doctor`.
- **`sy mon`** — `snapshot`, `doctor`.
- **`sy wwan`** — `status`.

Subcommands not listed above do not currently accept `--json`. Adding
the flag to them is a backwards-compatible extension.

### `--dry-run`

State-changing subcommands honour `--dry-run` by printing the planned
diff without applying. Honoured by:

- `sy apply` (and the `--diff` alias).
- `sy power apply`.
- `sy auto configure` (dry-run is the **default**; `--apply` opts in).
- `sy knowledge mcp-enable` / `sy knowledge mcp-disable` (dry-run is
  the default; `--apply` opts in).
- `sy stack push` (`--dry-run` prints the planned push).
- `sy spark <host> install` (inspect without installing).
- `sy spark <host> serve` / `download` / `stop` / `rm` / `token` /
  `operations cancel` (admission or mutation preview; no Docker or
  GPU side effects on serve dry-run).

Knowledge `mcp-enable` / `mcp-disable` and `auto configure` invert the
usual default: they refuse to mutate disk unless `--apply` is set, so
agent callers cannot accidentally rewrite an agent's MCP config.

### Exit codes

`sy` uses **stable, documented** exit codes per SPEC §4.7. The
constants live in the source under each plane's `exit` module
(`src/doctor/mod.rs`, `src/knowledge/mod.rs`, `src/supervision/service.rs`,
`src/power/mod.rs`, `src/ipc_cli.rs`, `src/crash/mod.rs`,
`src/agt/protocol.rs`).

| Code | Meaning                  | Source of truth                                                           |
|------|--------------------------|---------------------------------------------------------------------------|
| `0`  | success                  | every plane                                                               |
| `1`  | generic failure          | every plane                                                               |
| `2`  | usage error              | clap (`ErrorKind::ValueValidation`), `policy`, `power`, `service`, `crash`|
| `3`  | drift / warn-only / lint-fail | `doctor` (warn-only), `ipc` (degraded), `service` (status drift), `power` (ADWIN drift alarm), `policy lint`, `spark` (remote policy/state rejection), `mon snapshot` (aggregator unreachable), `file ipc` (daemon unreachable) |
| `4`  | not-ready / daemon-unreachable / not-found | `ipc` (starting/failed), `service` (not-ready), `power` (daemon-unreachable), `crash` (record not found), `agt` (daemon unavailable), `knowledge` (qdrant-unreachable), `spark` (OpenSSH/TLS/auth unreachable), `file ipc` (op refused) |
| `5`  | embedding-failed / polkit-denied / plugin-error | `knowledge`, `power`, `file ipc` (plugin error)                |
| `6`  | unsupported hardware     | `power`                                                                   |
| `7`  | onboarding-not-complete  | `power` (`sy power show` with insufficient audit window)                  |

Each subcommand's reference entry lists the codes it can return.
Codes not listed for a subcommand are not emitted by that subcommand.

### Environment variables

Every flag that accepts `env = "..."` in the source is also settable
via the named variable. CLIG precedence applies: flag > env > config
file > default. The full set is enumerated per subcommand below; the
load-bearing globals are:

- `SY_ROOT` — global; same as `--root`.
- `XDG_CONFIG_HOME` — read by `sy apply` to compute the default target.
- `XDG_STATE_HOME` — read by `sy crash`, `sy knowledge`, `sy power`
  for per-plane state directories.
- `XDG_RUNTIME_DIR` — read by `sy agt`, `sy ipc`, `sy power` to locate
  daemon Unix sockets.
- `SY_PRIORITY` — QoS class for aiplane scheduler admission
  (`Realtime | Interactive | Background | Batch`).
- `SY_DEADLINE` — soft deadline for `sy aiplane run` (e.g. `200ms`).
- `SY_TRACE_ID` — propagate a caller-supplied trace id end-to-end.
- `SY_DISK_THRESHOLD_GIB` — override the `sy disk` low-space threshold.
- `SY_AGT_CWD`, `SY_AGT_AGENT`, `SY_AGT_SANDBOX_PROFILE`,
  `SY_AGT_REQUEST_ID` — `sy agt` sandbox + launcher overrides.
- `SY_SYSFS_ROOT` — `sy power status` sysfs probe root override (test
  hermeticity).
- `SY_SPARK_*` — Spark client flags (`JSON`, `DRY_RUN`, `YES`,
  `CONFIG_DIR`, `PROBE`, `LISTEN_ADDRESS`, `LISTEN_PORT`,
  `RELEASE_SIGNATURE`, `RELEASE_PUBLIC_KEY`, and per-command
  `REVISION` / `ALIAS` / `INSTANCE_NAME` / …). See [`sy spark`](#sy-spark).
- `SY_FILE_SOCK` — override `$XDG_RUNTIME_DIR/sy-file.sock` for
  `sy file ipc`.
- `SY_MON_HISTORY_SIZE`, `SY_MON_TICK_MS`, `SY_MON_BIND`,
  `SY_MON_HISTORY_PATH` — aggregator ring and socket for `sy mon collect`.
- `NO_COLOR` and `TERM=dumb` — honoured by the tracing subscriber on
  stderr; agent stdout output is plain text regardless.

### TTY behaviour

Subcommands that are interactive by default refuse to prompt when
stdin is not a TTY, per CLIG. The pairs that ship today:

- `sy approve <token>` — refuses without a TTY unless
  `--token-from-stdin --yes` is paired and the UUID is on stdin.
- `sy policy trust --confirm` — refuses without a TTY unless `--yes`
  is paired and `TRUST THIS PROFILE\n` is on stdin.
- `sy power show` — only spawns `xdg-open` on the resulting PDF when
  stdin is a TTY; `--no-open` skips the spawn unconditionally.

### Aliases and overloaded flags

- `sy apply --diff` is a documented alias for `sy apply --dry-run --json`.
- The bar-tile applets (`sy bat`, `sy bright`, `sy bt`, `sy gpu`,
  `sy npu`, `sy disk`, `sy pwr`, `sy silent`, `sy syauth`, `sy vol`)
  take `--waybar` instead of `--json` for the single-JSON-line
  waybar `custom/*` schema.

---

## `sy apply`

### Synopsis

```text
sy apply [--theme <NAME>] [--dry-run] [--diff] [--json] [--yes]
```

### Description

Renders every minijinja template under `configs/` with the active
theme, writes the result to the target directory, then symlinks
`configs/systemd/user/` units into `~/.config/systemd/user/` and runs
`systemctl --user daemon-reload`. Idempotent — re-running is a no-op
when nothing has drifted.

### Options

| Name        | Type   | Default | Env       | Description                                                                                                       |
|-------------|--------|---------|-----------|-------------------------------------------------------------------------------------------------------------------|
| `--theme`   | string | `gruvbox-material` (or `sy.toml` override) | —         | Theme to render against (resolved as `themes/<name>.toml`).                                                       |
| `--dry-run` | bool   | `false` | —         | Print the planned diff; do not write.                                                                             |
| `--diff`    | bool   | `false` | —         | Alias for `--dry-run --json`; preview pending unit changes as a JSON document on stdout.                          |
| `--json`    | bool   | `false` | —         | Emit the unit diff on stdout in the stable schema (see `src/supervision/apply.rs`).                               |
| `--yes`     | bool   | `false` | —         | Confirm destructive ops: overwriting regular files at target paths, removing the legacy system-level unit.        |

### Exit codes

- `0` — success (or clean dry-run).
- `1` — any I/O, render, or unit-link failure.

### Examples

```bash
sy apply --dry-run                  # preview every change
sy apply --diff                     # preview as JSON
sy apply --theme gruvbox-material   # apply with a specific theme
sy apply --yes                      # confirm destructive ops
```

### See also

- [tutorial: getting started](../tutorials/getting-started.md)
- [reference: themes](#sy-themes)

---

## `sy themes`

### Synopsis

```text
sy themes
```

### Description

Lists every `*.toml` file under `themes/` in the active repo root
(one stem per line, sorted).

### Options

None.

### Examples

```bash
sy themes
```

### See also

- [`sy apply`](#sy-apply)
- [`sy render`](#sy-render)

---

## `sy render`

### Synopsis

```text
sy render [--theme <NAME>] <PATH>
```

### Description

Renders a single template (path relative to `configs/`) to stdout
using the active theme. Useful for "show me what this would look
like" without writing to disk.

### Options

| Name      | Type   | Default                          | Env | Description                                                |
|-----------|--------|----------------------------------|-----|------------------------------------------------------------|
| `--theme` | string | `gruvbox-material` (or override) | —   | Theme to render against.                                   |
| `<PATH>`  | path   | required                         | —   | Template path relative to `configs/`, e.g. `waybar/style.css`. |

### Examples

```bash
sy render waybar/style.css
sy render --theme gruvbox-material niri/config.kdl
```

### See also

- [`sy apply`](#sy-apply)

---

## `sy aiplane`

The NPU inference plane. Owns `/dev/accel/accel0`. Source:
`src/aiplane/cli.rs`.

### Subcommands

- [`sy aiplane status`](#sy-aiplane-status)
- [`sy aiplane list`](#sy-aiplane-list)
- [`sy aiplane run`](#sy-aiplane-run)
- [`sy aiplane cancel`](#sy-aiplane-cancel)

`sy aiplane worker` is hidden — the daemon supervisor spawns it; it
is not for direct human use.

### `sy aiplane status`

Reads the daemon's status snapshot from
`$XDG_STATE_HOME/sy/aiplane/status.json` (or the legacy
`sy/knowledge/status.json` during the migration window).

| Name     | Type | Default | Env | Description                                            |
|----------|------|---------|-----|--------------------------------------------------------|
| `--json` | bool | `false` | —   | Emit the snapshot JSON instead of the human summary.   |

```bash
sy aiplane status
sy aiplane status --json
```

### `sy aiplane list`

Lists every workload kind the daemon would register on this host, with
the on-disk cache directory and a `prepared` flag.

| Name     | Type | Default | Env | Description                          |
|----------|------|---------|-----|--------------------------------------|
| `--json` | bool | `false` | —   | Emit the table as a JSON array.      |

### `sy aiplane run`

One-shot dispatch over IPC v1. Falls back to in-process registry when
the daemon is down so offline debug works.

| Name           | Type     | Default        | Env             | Description                                                                                                  |
|----------------|----------|----------------|-----------------|--------------------------------------------------------------------------------------------------------------|
| `--workload`   | string   | required       | —               | Workload kind: `embed | rerank | vad | stt | tts | ocr | clip | denoise | eye-track`.                       |
| `--input`      | string   | required       | —               | JSON `WorkloadInput` literal, e.g. `'{"kind":"text","text":"hello"}'`.                                       |
| `--priority`   | enum     | `Interactive`  | `SY_PRIORITY`   | QoS class (case-sensitive PascalCase): `Realtime | Interactive | Background | Batch`.                       |
| `--deadline`   | duration | none           | `SY_DEADLINE`   | Soft deadline (e.g. `200ms`, `5s`, `1m`, `1h`). Bare numbers are rejected with `EXIT_USAGE` (2).            |
| `--trace-id`   | string   | none           | `SY_TRACE_ID`   | Caller-supplied trace id propagated end-to-end so logs across `sy` + daemon + worker share a key.            |
| `--json`       | bool     | `false`        | —               | Emit the structured output.                                                                                  |

#### Exit codes

- `0` — success.
- `1` — wire error, daemon-reported error, or fallback-path failure.
- `2` — bad `--priority`, bad `--deadline`, or unknown `--workload`.

### `sy aiplane cancel`

Cooperatively aborts an inflight `aiplane.run` by request id (Ulid).
Daemon-side: the running request returns `ErrorCode::Cancelled` to
its original caller.

| Name           | Type   | Default  | Env | Description                                              |
|----------------|--------|----------|-----|----------------------------------------------------------|
| `<REQUEST_ID>` | string | required | —   | Ulid printed by `sy aiplane run` (or carried in the v1 envelope's `request_id`). |
| `--json`       | bool   | `false`  | —   | Emit the daemon's structured ACK.                        |

### Examples

```bash
sy aiplane status --json
sy aiplane list
sy aiplane run --workload embed \
  --input '{"kind":"text","text":"hello"}'
sy aiplane run --workload embed --priority Background --deadline 5s \
  --input '{"kind":"text","text":"queue this behind interactive load"}'
sy aiplane cancel 01HZ2R4F7C8X9PR3T7VFQ4M3Y2 --json
```

### See also

- [reference: knowledge](#sy-knowledge) — consumer of the embed workload
- [explanation: architecture](../explanation/architecture.md)

---

## `sy knowledge`

System-wide semantic-search plane: Qdrant + the aiplane `embed`
workload. Source: `src/knowledge/mod.rs`, `src/knowledge/cli.rs`.

### Subcommands

| Subcommand              | Purpose                                                                      |
|-------------------------|------------------------------------------------------------------------------|
| `daemon`                | Long-lived foreground daemon spawned by `sy.target`.                         |
| `add <PATH>`            | Register a path as an index source. Edits `sy.toml`.                         |
| `rm <PATH>`             | Remove a registered source (matches by path).                                |
| `list`                  | List registered sources + last-indexed timestamps.                           |
| `index`                 | One-shot incremental index.                                                  |
| `sync --yes`            | Drop the collection and re-index everything (schema-breaking).               |
| `schedule [<INTERVAL>]` | Show or set the daemon's incremental-sync interval (e.g. `30m`).             |
| `search <QUERY>`        | Semantic search; two-stage by default (embed → qdrant → rerank).             |
| `manifests`             | List active `qdr.toml` manifests.                                            |
| `waybar`                | Emit one JSON line for the waybar `custom/sy-knowledge` module.              |
| `status`                | Print the daemon's status snapshot.                                          |
| `pick`                  | Fuzzel-driven interactive search.                                            |
| `pause` / `resume` / `toggle-pause` / `cancel` | Daemon control plane.                                 |
| `bench`                 | Throughput probe.                                                            |
| `mcp`                   | Stdio JSON-RPC MCP server exposing knowledge tools to agents.                |
| `mcp-enable`            | Register `sy-knowledge` MCP in supported agent configs.                      |
| `mcp-disable`           | Remove `sy-knowledge` from supported agent configs.                          |
| `mcp-status`            | Show registration status per agent.                                          |

### Options per subcommand

#### `sy knowledge add`

| Name         | Type | Default | Env | Description                                                                                                  |
|--------------|------|---------|-----|--------------------------------------------------------------------------------------------------------------|
| `<PATH>`     | path | required | —  | File or directory to index.                                                                                  |
| `--disabled` | bool | `false` | —   | Insert as disabled.                                                                                          |
| `--discover` | bool | `false` | —   | Treat the path as a discovery root: walk for `qdr.toml` manifests; each manifested folder declares its own rules. |

#### `sy knowledge list` / `status` / `manifests` / `index` / `bench`

All accept `--json`. `index` additionally accepts:

| Name       | Type | Default | Env | Description                          |
|------------|------|---------|-----|--------------------------------------|
| `--source` | path | none    | —   | Restrict to a single registered source path. |

`bench` additionally accepts `--n <N>` (default `256`, minimum `8`)
for chunk count.

#### `sy knowledge sync`

| Name    | Type | Default | Env | Description                                              |
|---------|------|---------|-----|----------------------------------------------------------|
| `--yes` (`-y`) | bool | `false` | — | Confirms the destructive re-embed (drops the Qdrant collection). Required. |

#### `sy knowledge search`

| Name          | Type   | Default       | Env             | Description                                                                                                  |
|---------------|--------|---------------|-----------------|--------------------------------------------------------------------------------------------------------------|
| `<QUERY>`     | string | required      | —               | Search query.                                                                                                |
| `--limit` (`-k`) | usize | `8`        | —               | Top-K hits to return.                                                                                        |
| `--json`      | bool   | `false`       | —               | Emit hits as a JSON array.                                                                                   |
| `--source`    | path   | none          | —               | Restrict to a registered source path prefix.                                                                 |
| `--no-rerank` | bool   | `false`       | —               | Skip the cross-encoder rerank pass; lower-latency embed-only path.                                           |
| `--candidates`| usize  | `8`           | —               | Candidates pulled from qdrant before reranking (each adds ~350 ms on AMD NPU). Ignored with `--no-rerank`.   |
| `--priority`  | enum   | `Interactive` | `SY_PRIORITY`   | QoS class for the embed step's scheduler admission.                                                          |

#### `sy knowledge mcp-enable` / `mcp-disable` / `mcp-status`

| Name      | Type | Default | Env | Description                                                                  |
|-----------|------|---------|-----|------------------------------------------------------------------------------|
| `--apply` | bool | `false` | —   | `mcp-enable` / `mcp-disable` only. Without `--apply` the command is a dry-run; with it the agent's config is rewritten and `[knowledge].mcp_enabled` is flipped in `sy.toml`. |
| `--json`  | bool | `false` | —   | Emit the per-agent registration table as JSON.                               |

#### `sy knowledge schedule`

| Name          | Type   | Default | Env | Description                                                            |
|---------------|--------|---------|-----|------------------------------------------------------------------------|
| `<INTERVAL>`  | string | none    | —   | Optional. Without an arg, prints the current schedule (default `15m`). With an arg (`30m`, `2h`, …) writes it back to `sy.toml` and signals the daemon. |

### Exit codes

- `0` — success.
- `1` — generic failure.
- `3` — `SOURCE_NOT_FOUND` (e.g. `sy knowledge add /missing/path`).
- `4` — `QDRANT_UNREACHABLE`.
- `5` — `EMBEDDING_FAILED`.

(Defined in `src/knowledge/mod.rs::exit`.)

### Examples

```bash
sy knowledge add ~/Documents/notes
sy knowledge daemon
sy knowledge search "rust async cancellation"
sy knowledge search "rust async cancellation" --no-rerank --limit 16
sy knowledge status --json
sy knowledge schedule 30m
sy knowledge mcp-enable --apply
sy knowledge sync --yes        # destructive — drops the collection
```

### See also

- [how-to: add a knowledge source](../how-to/add-a-knowledge-source.md)
- [reference: aiplane](#sy-aiplane)

---

## `sy power`

Adaptive power orchestrator. Source: `src/power/mod.rs`, `src/power/cli.rs`.

### Subcommands

| Subcommand                   | Purpose                                                                  |
|------------------------------|--------------------------------------------------------------------------|
| `status`                     | Current state, profile, shield, reason.                                  |
| `daemon`                     | `sy-powerd` entrypoint (systemd user unit).                              |
| `apply`                      | Install polkit action, udev rule, systemd unit, waybar tile.             |
| `log`                        | Tail the NDJSON telemetry log.                                           |
| `profile <NAME>` / `--auto`  | Manual profile override; `--auto` clears it.                             |
| `explain`                    | Audit replay: which bandit arm fired and why.                            |
| `train`                      | Offline GRU retrain — reads telemetry, writes ONNX.                      |
| `show`                       | Render the offline `sy power` PDF report.                                |
| `list-profiles`              | Enumerate the bandit arm table from `configs/sy/power.toml`.             |
| `mcp`                        | MCP server entrypoint (stdio JSON-RPC).                                  |

### Options per subcommand

#### `sy power status`

| Name        | Type | Default | Env | Description                                                                                          |
|-------------|------|---------|-----|------------------------------------------------------------------------------------------------------|
| `--json`    | bool | `false` | —   | Emit the SPEC §4 `sy.power.status/v1` schema. Mutually exclusive with `--waybar`.                    |
| `--waybar`  | bool | `false` | —   | Emit the SPEC §5 waybar pill JSON (`{text, tooltip, class}`). Daemon-down renders as the `error` class and exits 0 so waybar keeps polling. |

#### `sy power apply`

| Name         | Type | Default | Env | Description                                                                                                              |
|--------------|------|---------|-----|--------------------------------------------------------------------------------------------------------------------------|
| `--dry-run`  | bool | `false` | —   | Print the planned changes without touching disk.                                                                         |
| `--yes`      | bool | `false` | —   | Gate destructive actions — currently the `systemctl --user mask power-profiles-daemon.service` path.                     |
| `--with-ppd` | bool | `false` | —   | Keep `power-profiles-daemon` active; run the `sy power` shim alongside it without binding `net.hadess.PowerProfiles`.    |

#### `sy power log`

| Name       | Type     | Default                       | Env | Description                                              |
|------------|----------|-------------------------------|-----|----------------------------------------------------------|
| `--since`  | duration | `DEFAULT_TAIL_WINDOW`         | —   | Filter to entries newer than this duration (`1h`, `30m`, `7d`). Bad value exits 2. |
| `--json`   | bool     | `false`                       | —   | Emit raw NDJSON (one JSON per line).                     |

#### `sy power profile`

| Name      | Type   | Default | Env | Description                                                                  |
|-----------|--------|---------|-----|------------------------------------------------------------------------------|
| `<NAME>`  | string | none    | —   | Profile name from the bandit arm table. Mutually exclusive with `--auto`.    |
| `--auto`  | bool   | `false` | —   | Clear any manual override and restore bandit control.                        |

#### `sy power explain`

| Name      | Type  | Default | Env | Description                                                                  |
|-----------|-------|---------|-----|------------------------------------------------------------------------------|
| `--last`  | usize | `10`    | —   | Show the last N decisions.                                                   |
| `--json`  | bool  | `false` | —   | Emit machine-readable JSON instead of a human summary.                       |

#### `sy power train`

| Name    | Type | Default                                                                | Env | Description                  |
|---------|------|------------------------------------------------------------------------|-----|------------------------------|
| `--in`  | path | `<state>/telemetry-<today>.ndjson`                                     | —   | Input NDJSON path.           |
| `--out` | path | `<state>/forecaster.onnx`                                              | —   | Output ONNX path.            |

#### `sy power show`

| Name           | Type     | Default                                                  | Env | Description                                                                                                  |
|----------------|----------|----------------------------------------------------------|-----|--------------------------------------------------------------------------------------------------------------|
| `--since`      | duration | `7d`                                                     | —   | Audit-log window. Bad value exits 2.                                                                         |
| `--out`        | path     | `<state>/reports/sy-power-<rfc3339>.pdf`                 | —   | PDF output path. Ignored with `--json`.                                                                      |
| `--no-open`    | bool     | `false`                                                  | —   | Do not invoke `xdg-open` on the PDF (set implicitly when stdin is not a TTY).                                |
| `--allow-thin` | bool     | `false`                                                  | —   | Skip the 24 h "thin window" gate (exit 7 by default).                                                        |
| `--json`       | bool     | `false`                                                  | —   | Emit JSON; skip PDF generation.                                                                              |

#### `sy power list-profiles`

| Name     | Type | Default | Env | Description                                          |
|----------|------|---------|-----|------------------------------------------------------|
| `--json` | bool | `false` | —   | Emit the SPEC §4 `sy.power.profiles/v1` schema.      |

### Exit codes

- `0` — success.
- `1` — generic failure.
- `2` — usage error (bad `--since`, unknown profile name, conflicting flags).
- `3` — `EXIT_DRIFT_ACTIVE` — ADWIN drift alarm reported by the daemon.
- `4` — `EXIT_DAEMON_UNREACHABLE` — socket missing or refused.
- `7` — `EXIT_ONBOARDING_NOT_COMPLETE` — `sy power show` window has fewer than 24 h of audit entries and `--allow-thin` was not set.

(Defined in `src/power/mod.rs` and `src/power/cli.rs`.)

### Examples

```bash
sy power status                          # human summary
sy power status --json                   # sy.power.status/v1 schema
sy power apply --dry-run                 # preview installer changes
sy power profile performance             # pin a profile
sy power profile --auto                  # release the pin
sy power log --since=1h --json
sy power explain --last=1 --json
sy power show --since=24h --no-open      # CI / headless
sy power show --json --since=1d          # agent path
```

### See also

- [reference: aiplane](#sy-aiplane) — power consumes the aiplane intent channel.

---

## `sy agt`

Sandboxed agent runner. Source: `src/agt/mod.rs`.

### Subcommands

| Subcommand     | Purpose                                                                  |
|----------------|--------------------------------------------------------------------------|
| `daemon`       | Run the long-lived foreground daemon.                                    |
| `run`          | Start a new agent session (the `Super+A` entry point).                   |
| `list`         | List managed sessions.                                                   |
| `prompt`       | Send a follow-up prompt to a running session.                            |
| `stop`         | Stop and remove a session.                                               |
| `tail`         | Stream the transcript of a session.                                      |
| `menu`         | Fuzzel session picker (waybar AGT left-click).                           |
| `waybar`       | Emit the waybar JSON tile.                                               |
| `inspect`      | Run the inspector TUI inside a foot popup.                               |
| `diag`         | Print the registry and ping each agent's `--version`.                    |

`sy agt sandbox-exec` and `sy agt sandbox-run` are hidden — they are
re-exec targets the daemon's `Decision::Allow` path invokes; not for
direct human use.

### Options per subcommand

#### `sy agt run`

| Name        | Type   | Default                         | Env             | Description                                                              |
|-------------|--------|---------------------------------|-----------------|--------------------------------------------------------------------------|
| `--cwd`     | path   | focused niri window's cwd       | `SY_AGT_CWD`    | Working directory the agent runs in.                                     |
| `--agent`   | string | (fuzzel picker)                 | `SY_AGT_AGENT`  | Agent name from `agents.toml`. Skips the picker.                         |
| `<PROMPT>`  | string | (fuzzel prompts)                | —               | Initial prompt. If omitted, fuzzel asks.                                 |
| `--editor`  | bool   | `false`                         | —               | Read prompt from `$EDITOR` instead of fuzzel.                            |

#### `sy agt list`

| Name     | Type | Default | Env | Description                          |
|----------|------|---------|-----|--------------------------------------|
| `--json` | bool | `false` | —   | Emit sessions as a JSON array.       |

#### `sy agt prompt` / `sy agt stop`

| Name           | Type   | Default | Env | Description           |
|----------------|--------|---------|-----|-----------------------|
| `<SESSION_ID>` | string | required | —  | Session id.           |
| `<TEXT>` (prompt only) | string | required | — | Follow-up prompt. |

#### `sy agt tail`

| Name           | Type   | Default | Env | Description                                                       |
|----------------|--------|---------|-----|-------------------------------------------------------------------|
| `<SESSION_ID>` | string | required | —  | Session id.                                                       |
| `--follow` (`-f`) | bool | `false` | —  | Follow the transcript.                                            |
| `--no-replay`  | bool   | `false` | —   | Suppress the recorded transcript replay; show only live events.   |

#### `sy agt diag`

| Name     | Type | Default | Env | Description                                                |
|----------|------|---------|-----|------------------------------------------------------------|
| `--json` | bool | `false` | —   | Emit the diagnostic table as JSON.                         |

### Exit codes

- `0` — success.
- `2` — `NO_SESSION` (session id not found).
- `4` — `DAEMON_UNAVAILABLE` (daemon socket down or unexpected reply).

(Defined in `src/agt/protocol.rs::exit`.)

### Examples

```bash
sy agt daemon
sy agt run --cwd ~/sources/sy "draft a CONTRIBUTING.md"
sy agt list --json
sy agt tail abcd1234 -f
sy agt diag --json
```

### See also

- [reference: policy](#sy-policy)
- [reference: approve](#sy-approve)

---

## `sy policy`

Operator surface for the SPEC §4.4 step 2 sandbox-policy resolver.
Source: `src/agt/policy/cli.rs`.

### Subcommands

| Subcommand | Purpose                                                  |
|------------|----------------------------------------------------------|
| `show`     | Print the resolved profile + active overlays.            |
| `lint`     | Static checks for risky policy settings.                 |
| `explain`  | Simulate the resolver for a `(tool, argv)` pair.         |
| `trust`    | Opt into the `trusted` profile (TTY-gated).              |
| `grant`    | Persist a pre-approved TTL grant under `$XDG_RUNTIME_DIR/sy`. |

### Options per subcommand

#### `sy policy show`

| Name        | Type   | Default  | Env | Description                                                  |
|-------------|--------|----------|-----|--------------------------------------------------------------|
| `--profile` | string | `normal` | —   | Profile name under `configs/policy/profiles/<name>.toml`.    |
| `--tool`    | string | none     | —   | Layer the per-tool overlay under `configs/policy/tools/<tool>.toml`. |
| `--json`    | bool   | `false`  | —   | Emit the documented JSON schema.                             |

#### `sy policy lint`

| Name        | Type   | Default  | Env | Description                                |
|-------------|--------|----------|-----|--------------------------------------------|
| `--profile` | string | `normal` | —   | Profile name.                              |
| `--json`    | bool   | `false`  | —   | Emit the lint report as JSON.              |

#### `sy policy explain`

| Name        | Type   | Default  | Env | Description                                              |
|-------------|--------|----------|-----|----------------------------------------------------------|
| `--tool`    | string | required | —   | Tool path (e.g. `/usr/bin/rg`).                          |
| `--argv`    | string | required | —   | argv as a single string; split on whitespace.            |
| `--profile` | string | `normal` | —   | Profile name.                                            |
| `--json`    | bool   | `false`  | —   | Emit the structured decision document as JSON.           |

#### `sy policy trust`

| Name        | Type | Default | Env | Description                                                                                          |
|-------------|------|---------|-----|------------------------------------------------------------------------------------------------------|
| `--confirm` | bool | `false` | —   | Required acknowledgement. Without it, exits 2 immediately.                                           |
| `--yes`     | bool | `false` | —   | Non-interactive override. stdin must contain `TRUST THIS PROFILE\n` exactly. Pair with `--confirm`.  |

#### `sy policy grant`

| Name      | Type     | Default | Env | Description                                              |
|-----------|----------|---------|-----|----------------------------------------------------------|
| `--tool`  | string   | required | —  | Tool name the grant covers.                              |
| `--scope` | path     | required | —  | Scope path under which the grant is honoured.            |
| `--ttl`   | duration | required | —  | TTL (`200ms | 5s | 1m | 2h`). Bare numbers exit 2.       |
| `--json`  | bool     | `false`  | —  | Emit the grant document as JSON.                         |

### Exit codes

- `0` — success.
- `2` — usage error (missing `--confirm`, conflicting flags, bad TTL).
- `3` — `EXIT_LINT_FAIL` — `lint` produced at least one `fail` row.

### Examples

```bash
sy policy show --profile strict --json
sy policy lint --profile trusted
sy policy explain --tool /usr/bin/curl --argv 'https://example.com'
sy policy trust --confirm
sy policy grant --tool rg --scope ~/sources/sy --ttl 15m --json
```

### See also

- [reference: approve](#sy-approve)
- [reference: agt](#sy-agt)

---

## `sy approve`

Approve a pending consent token issued by the agent daemon's
`ConsentRequired` reply. Refuses to run when stdin is not a TTY
unless `--token-from-stdin --yes` is paired and the UUID is piped on
stdin (CLIG: non-interactive defaults refuse).

### Synopsis

```text
sy approve [<TOKEN>] [--yes] [--token-from-stdin] [--json]
```

### Options

| Name                  | Type   | Default  | Env | Description                                                                |
|-----------------------|--------|----------|-----|----------------------------------------------------------------------------|
| `<TOKEN>`             | string | none     | —   | Consent token UUID. Omit when piping via `--token-from-stdin --yes`.       |
| `--yes`               | bool   | `false`  | —   | Acknowledge the non-TTY bypass. Pair with `--token-from-stdin`.            |
| `--token-from-stdin`  | bool   | `false`  | —   | Read the token from stdin instead of argv.                                 |
| `--json`              | bool   | `false`  | —   | Emit the daemon response as JSON.                                          |

### Exit codes

- `0` — success.
- `1` — token not a valid UUID, or daemon error.
- `2` — refusal: stdin is not a TTY and the `--token-from-stdin --yes` override wasn't paired correctly, or `<TOKEN>` was empty.

### Examples

```bash
sy approve 4f1d2c5b-aaaa-bbbb-cccc-1234567890ab
echo 4f1d2c5b-...| sy approve --token-from-stdin --yes --json
```

### See also

- [reference: policy](#sy-policy)

---

## `sy doctor`

Run the SPEC §4.6 linear health checks. Source: `src/doctor/mod.rs`.

### Synopsis

```text
sy doctor [--json] [--only <PREFIX>]
```

### Options

| Name     | Type   | Default | Env | Description                                                                  |
|----------|--------|---------|-----|------------------------------------------------------------------------------|
| `--json` | bool   | `false` | —   | Emit the SPEC §4.6 `sy.doctor/v1` schema on stdout (pretty-printed).         |
| `--only` | string | none    | —   | Run only checks whose name starts with the prefix (e.g. `ipc.`, `kernel.`). |

### Exit codes

- `0` — all checks passed.
- `1` — any check failed.
- `2` — usage error (e.g. `--only=<prefix>` matched no checks).
- `3` — drift: no `fail`, at least one `warn`.

`skip` does not influence the exit code.

### Examples

```bash
sy doctor
sy doctor --json
sy doctor --only=ipc.
```

### See also

- [reference: ipc](#sy-ipc)
- [reference: service](#sy-service)

---

## `sy crash`

List and show panic records (under `$XDG_STATE_HOME/sy/crash/*.json`)
and native coredumps (via `coredumpctl list --json=pretty --since=-1day`).
Source: `src/crash/mod.rs`.

### Subcommands

| Subcommand        | Purpose                                                  |
|-------------------|----------------------------------------------------------|
| `list`            | Time-sorted merge of panic JSONL + coredumpctl entries.  |
| `show <TS>`       | Show one record by RFC3339 timestamp.                    |

### Options

| Name          | Type   | Default | Env | Description                                              |
|---------------|--------|---------|-----|----------------------------------------------------------|
| `--json`      | bool   | `false` | —   | Emit the v1 JSON schema.                                 |
| `<TS>` (show) | string | required| —   | RFC3339 timestamp from `sy crash list`.                  |

### Exit codes

- `0` — success.
- `1` — generic I/O error.
- `4` — `show <ts>` matched no record.

### Examples

```bash
sy crash list
sy crash list --json
sy crash show 2026-05-22T11:30:00.000Z --json
```

### See also

- [reference: doctor](#sy-doctor)

---

## `sy ipc`

Operator-visible round-trip checks for the v1 IPC envelope. Source:
`src/ipc_cli.rs`.

### Subcommands

| Subcommand | Purpose                                                          |
|------------|------------------------------------------------------------------|
| `ping`     | Call `system.health` on an endpoint; print state + latency.      |
| `describe` | Call `system.describe`; emit the methods / capabilities document. |

### Options per subcommand

#### `sy ipc ping <ENDPOINT>`

| Name         | Type   | Default | Env | Description                                                                  |
|--------------|--------|---------|-----|------------------------------------------------------------------------------|
| `<ENDPOINT>` | string | required| —   | Endpoint name (`knowledge | aiplane | agt | stack`) or a raw socket path.    |
| `--json`     | bool   | `false` | —   | Emit a single JSON line on stdout.                                           |

#### `sy ipc describe <ENDPOINT>`

| Name         | Type   | Default | Env | Description                                                                  |
|--------------|--------|---------|-----|------------------------------------------------------------------------------|
| `<ENDPOINT>` | string | required| —   | Endpoint name or socket path (same resolution as `ping`).                    |
| `--json`     | bool   | `true`  | —   | Pretty-print the `result` object as JSON (default).                          |
| `--text`     | bool   | `false` | —   | Emit a short text summary instead of the JSON dump. Mutually exclusive with `--json`. |

### Exit codes (`ping`)

- `0` — `ready`.
- `1` — connect failure or non-`system.health` response.
- `2` — usage error (unknown endpoint, bad flags).
- `3` — `degraded`.
- `4` — `starting` or `failed`.

### Examples

```bash
sy ipc ping knowledge
sy ipc ping aiplane --json
sy ipc describe knowledge
sy ipc describe knowledge --text
sy ipc ping /run/user/1000/sy-knowledge.sock
```

### See also

- [reference: doctor](#sy-doctor)
- [reference: service](#sy-service)

---

## `sy service`

Wrapper for `systemctl --user` / `journalctl --user` per SPEC §4.7
(arch-supervision Step 3). Source: `src/supervision/service.rs`.

Canonical short names: `aiplane`, `knowledge`, `qdrant`, `stack-bar`,
`agentd`, `powerd`. Each resolves to `sy-<name>.service`. Full
`sy-<name>.service` / `sy-<name>.socket` / `sy-<name>.target` /
`sy.target` names are passed through verbatim. Anything else exits 2.

### Subcommands

| Subcommand                  | Purpose                                                                  |
|-----------------------------|--------------------------------------------------------------------------|
| `start <NAME>`              | `systemctl --user start sy-<name>.service` (idempotent).                 |
| `stop <NAME>`               | `systemctl --user stop sy-<name>.service`.                               |
| `restart <NAME>`            | `systemctl --user restart sy-<name>.service`.                            |
| `status <NAME>`             | Map systemd state → SPEC §4.5 logical state; `--json` for agents.        |
| `enable <NAME>`             | `systemctl --user enable sy-<name>.service` (idempotent).                |
| `disable <NAME>`            | `systemctl --user disable sy-<name>.service`.                            |
| `logs <NAME>`               | Stream `journalctl --user -u sy-<name>.service` with optional filters.   |

### Options

#### `sy service status`

| Name     | Type | Default | Env | Description                                              |
|----------|------|---------|-----|----------------------------------------------------------|
| `--json` | bool | `false` | —   | Emit the stable schema in `crate::supervision::status`.  |

#### `sy service logs`

| Name           | Type     | Default | Env | Description                                                              |
|----------------|----------|---------|-----|--------------------------------------------------------------------------|
| `--follow` (`-f`) | bool  | `false` | —   | Follow the log stream.                                                   |
| `--lines` (`-n`) | usize   | none    | —   | Show only the last N entries.                                            |
| `--since`      | string   | none    | —   | Passed verbatim to `journalctl`.                                         |
| `--trace`      | string   | none    | —   | Filter to entries with `SY_TRACE_ID=<id>`.                               |

### Exit codes

- `0` — success.
- `1` — generic failure (e.g. unit not running when start was requested).
- `2` — usage error (unknown short name).
- `3` — drift (state mismatch detected by `status`).
- `4` — not-ready (unit installed but `state != ready` when expected).

(Defined in `src/supervision/service.rs::exit`.)

### Examples

```bash
sy service start aiplane
sy service status knowledge --json
sy service logs aiplane -f -n 200
sy service logs agentd --trace 4f1d2c5b-aaaa-bbbb-cccc-1234567890ab
sy service enable sy.target
```

### See also

- [reference: doctor](#sy-doctor)

---

## `sy auto`

System probe + opinionated defaults for the knowledge plane.
Dry-run by default. Source: `src/auto.rs`.

### Subcommands

| Subcommand        | Purpose                                                  |
|-------------------|----------------------------------------------------------|
| `configure`       | Probe the system; print or apply opinionated defaults.   |
| `list-detectors`  | List built-in detectors and whether each is on by default. |

### Options per subcommand

#### `sy auto configure`

| Name      | Type     | Default | Env | Description                                                                  |
|-----------|----------|---------|-----|------------------------------------------------------------------------------|
| `--apply` | bool     | `false` | —   | Commit the plan (write to `sy.toml` + drop `qdr.toml` files). Without it, the command is a dry-run. |
| `--json`  | bool     | `false` | —   | Emit the plan as JSON.                                                       |
| `--only`  | csv      | empty   | —   | Restrict to comma-separated detector ids.                                    |
| `--skip`  | csv      | empty   | —   | Skip comma-separated detector ids.                                           |
| `--force` | bool     | `false` | —   | Overwrite existing `qdr.toml` files when `DropQdrManifest` fires.            |

#### `sy auto list-detectors`

| Name     | Type | Default | Env | Description                                              |
|----------|------|---------|-----|----------------------------------------------------------|
| `--json` | bool | `false` | —   | Emit the detector list as JSON.                          |

### Examples

```bash
sy auto configure                    # dry-run
sy auto configure --apply            # commit
sy auto configure --only mcp-claude,mcp-cursor --apply
sy auto list-detectors --json
```

### See also

- [reference: knowledge](#sy-knowledge)

---

## `sy stack`

Temporary-artifact stack bar with three pools (clip / app / user).
Source: `src/stack/mod.rs`, `src/stack/cli.rs`.

### Subcommands

| Subcommand                    | Purpose                                                          |
|-------------------------------|------------------------------------------------------------------|
| `push <ITEM>`                 | Push a path (or `-` to read stdin) onto a pool.                  |
| `pop`                         | Remove the most recent item from a pool and print its id.        |
| `list`                        | List items (default: human table; `--json` for machine output).  |
| `preview <ID>`                | Print an item's payload to stdout.                               |
| `remove <ID>`                 | Remove an item by id.                                            |
| `move <ID> <DEST>`            | Move an item's payload into a directory and pop it from the stack.|
| `link <ID>`                   | Print a stable filesystem path for an item (materialising into a temp file). |
| `onto <INTEGRATION> <ID>`     | Hand the item to a configured `[[stack.onto]]` integration.      |
| `action <ID> <ACTION>`        | Run a context-menu action on an item (called by the bar daemon). |
| `toggle`                      | Show or hide the bar.                                            |
| `bar`                         | Run the iced layer-shell bar daemon.                             |
| `mcp`                         | Stdio JSON-RPC MCP server.                                       |

### Options per subcommand

#### `sy stack push`

| Name        | Type   | Default | Env | Description                                                              |
|-------------|--------|---------|-----|--------------------------------------------------------------------------|
| `<ITEM>`    | string | required | —  | A filesystem path or `-` to read stdin as content.                       |
| `--kind`    | enum   | `user`   | —  | Pool: `app | user`.                                                      |
| `--name`    | string | derived  | —  | Human-readable name for the item.                                        |
| `--dry-run` | bool   | `false`  | —  | Print the planned push and exit without mutating state.                  |
| `--json`    | bool   | `false`  | —  | Print `{"id":"<id>"}` instead of the bare id.                            |

#### `sy stack pop`

| Name     | Type   | Default | Env | Description                                              |
|----------|--------|---------|-----|----------------------------------------------------------|
| `--kind` | enum   | `user`  | —   | Pool: `app | user`.                                      |
| `--id`   | string | none    | —   | Pop a specific id instead of the top of the pool.        |

#### `sy stack action`

| Name       | Type   | Default | Env | Description                                              |
|------------|--------|---------|-----|----------------------------------------------------------|
| `<ID>`     | string | required| —   | Item id.                                                 |
| `<ACTION>` | enum   | required| —   | `copy | preview | move | link | onto | agent | remove`. |
| `--source` | enum   | `stack` | —   | `stack` (items.json) or `clip` (cliphist).               |

### Examples

```bash
sy stack push ./screenshot.png --kind app --name "Build error"
echo "TODO note" | sy stack push - --kind user --name "TODO"
sy stack list --json
sy stack preview a1b2c3d4
sy stack move a1b2c3d4 ~/notes/
sy stack onto telegram a1b2c3d4
sy stack toggle
```

### See also

- [reference: knowledge](#sy-knowledge)

---

## `sy spark`

Inspect, install, and drive one configured DGX Spark appliance.
`<HOST>` is passed to OpenSSH as a single argument; there is no
arbitrary-command escape hatch. OpenSSH owns `known_hosts`, agents,
hardware tokens, and password prompts. Credentials are never accepted
as `sy` arguments and are never stored by `sy`.

Source: `src/spark/cli.rs`. Hidden unit entrypoints (`run-agent`,
`run-executor`, `activate`, `inspect`) are not part of the laptop
CLI.

### Synopsis

```text
sy spark <HOST> <COMMAND>
```

### Subcommands

| Subcommand | Purpose |
|------------|---------|
| `install` | Inspect the appliance (`--dry-run`) or apply the signed ARM64 install (`--yes`). |
| `upgrade` | Stage and verify a signed side-by-side release, preserve engines, and automatically roll back failed semantic health. |
| `rollback` | SSH-only exact rollback to the verified preceding control-plane release. |
| `status` | Compact authenticated agent/executor health over pinned HTTPS. |
| `doctor` | Authenticated, read-only compatibility and security checks. |
| `operations` | Inspect, `--follow`, or `cancel` durable operations. |
| `token` | `create` / `list` / `revoke` scoped bearer tokens. Create returns the secret once on stdout. |
| `download` | Acquire and verify one immutable Hugging Face model snapshot. |
| `serve` | Start a verified model with the root-configured engine after fail-closed admission. |
| `launch` | Run Codex, Claude Code, or OpenCode locally against an exact managed Spark model. |
| `ps` | Desired versus observed managed instances. Does not print the internal bridge address. |
| `logs` | Bounded, redacted logs for one instance. |
| `stop` | Persist stopped intent, drain, and remove one instance. An already-absent instance is an idempotent success. |
| `ls` | List complete verified local model snapshots. |
| `show` | Immutable identity, provenance, aliases, and references for one model. |
| `rm` | Preview or remove only unreferenced native-cache model data. |
| `client-config` | Render a user-level Codex or Claude Code projection. Names the token env var; does not read or write it. |
| `cert status` | Authenticated leaf-certificate identity. |
| `cert rotate` | SSH-only leaf rotation with overlap; `--ca` rotates and atomically re-pins the local CA. |

### Options (`install`)

| Name | Type | Default | Env | Description |
|------|------|---------|-----|-------------|
| `--dry-run` | bool | `false` | `SY_SPARK_DRY_RUN` | Upload a content-addressed probe, run `spark bootstrap inspect`, verify the hash, remove the probe. No install. |
| `--yes` | bool | `false` | `SY_SPARK_YES` | Apply the reviewed manifest. Requires `--release-signature` and `--release-public-key`. |
| `--json` | bool | `false` | `SY_SPARK_JSON` | Emit `sy.spark.install-manifest/v1`. |
| `--probe` | path | `/usr/libexec/sy/spark-bootstrap-aarch64` | `SY_SPARK_PROBE` | ARM64 feature-minimal probe artefact. |
| `--listen-address` | IP | none | `SY_SPARK_LISTEN_ADDRESS` | Explicit LAN address for the HTTPS listener. |
| `--listen-port` | u16 | `9843` | `SY_SPARK_LISTEN_PORT` | HTTPS listener port. |
| `--release-signature` | path | none | `SY_SPARK_RELEASE_SIGNATURE` | Minisign signature for the ARM64 release (required with `--yes`). |
| `--release-public-key` | path | none | `SY_SPARK_RELEASE_PUBLIC_KEY` | Pinned minisign public key (required with `--yes`). |
| `--config-dir` | path | Spark config root | `SY_SPARK_CONFIG_DIR` | Local Spark configuration root. |

`upgrade` accepts the same options. `rollback` and `cert rotate` accept
`--dry-run`, `--yes`, `--json`, and `--config-dir`; exactly one of `--dry-run`
and `--yes` is required. `cert rotate --ca` explicitly replaces the local CA.

### Options (`launch`)

```text
sy spark <HOST> launch <codex|claude|opencode> [OPTIONS] [-- <AGENT_ARGS>...]
```

| Name | Type | Env | Description |
|------|------|-----|-------------|
| `--model` | string | `SY_SPARK_LAUNCH_MODEL` | Exact installed model identity or alias. |
| `--config` | bool | `SY_SPARK_LAUNCH_CONFIG` | Configure launch-owned state and exit. |
| `--restore` | bool | `SY_SPARK_LAUNCH_RESTORE` | Remove only sy-owned Codex launch files. |
| `-y`, `--yes` | bool | `SY_SPARK_YES` | Approve a fixed missing-client installer. |
| `--dry-run` | bool | `SY_SPARK_DRY_RUN` | Resolve/reuse/admit without mutation. |
| `--json` | bool | `SY_SPARK_JSON` | Emit `sy.spark.launch-plan/v1`; requires `--dry-run` or `--config`. |
| `--config-dir` | path | `SY_SPARK_CONFIG_DIR` | Protected Spark configuration root. |

Arguments are accepted only after `--` and are passed directly without a
shell. The agent runs in the current directory with inherited terminal I/O and
receives a separate inference-only token. The Spark administrator credential is
never exposed. Exit codes `1..125` from the child are propagated.

### Token scopes (`token create --scope`)

`models:read`, `models:write`, `instances:read`, `instances:write`,
`inference`, `logs:read`, `operations:read`, `operations:cancel`,
`benchmarks:read`, `benchmarks:write`. Repeat `--scope` for each. The benchmark
scopes remain wire-compatible for pre-policy clients; the normal CLI has no
recipe, benchmark, or tuning commands.

### Exit codes

- `0` — success.
- `1` — unexpected failure.
- `2` — usage or local configuration.
- `3` — remote policy or state rejection (admission denied, invalid model
  intent, and similar).
- `4` — OpenSSH/SFTP/agent unreachable, TLS identity mismatch, or
  authentication failure.

### Examples

```bash
sy spark dgx-spark install --dry-run --json
sy spark dgx-spark install --yes --release-signature sy-aarch64.minisig \
  --release-public-key sy-release.pub
sy spark dgx-spark upgrade --dry-run --json
sy spark dgx-spark rollback --dry-run --json
sy spark dgx-spark cert rotate --dry-run --json
sy spark dgx-spark status --json
sy spark dgx-spark doctor --json
sy spark dgx-spark serve ornith-1.5:9b --dry-run --json
sy spark dgx-spark ps --json
sy spark dgx-spark token create --name reader --scope models:read \
  --scope operations:read --detach --json
sy spark dgx-spark client-config ornith --client codex
sy spark dgx-spark launch codex --model ornith-1.5:9b
sy spark dgx-spark launch claude --model ornith-1.5:9b -- --permission-mode plan
sy spark dgx-spark launch opencode --model ornith-1.5:9b
```

### See also

- [How to install the Spark agent](../how-to/install-spark.md)
- [How to serve a model on Spark](../how-to/serve-a-model-on-spark.md)
- [Spark reference](spark.md)

---

## `sy file`

Native niri-tiled file manager (iced). Bare `sy file [PATH]` opens
the window (prints `scaffold` then enters the GUI when stdout is a
TTY and `gui-iced` is on). Source: `src/file/cli.rs`.

### Synopsis

```text
sy file [PATH]
sy file doctor [--json]
sy file ipc <OP> …
sy file mcp
sy file waybar
```

### Subcommands

| Subcommand | Purpose |
|------------|---------|
| *(none)* | Open the manager on `PATH` (default: launch cwd / `$HOME` via niri binds). |
| `doctor` | Six health probes. Emits `sy.file.doctor/v1` with `--json`. |
| `ipc serve` | Run the daemon in-process on `$XDG_RUNTIME_DIR/sy-file.sock` (or `SY_FILE_SOCK` / `--sock`). |
| `ipc open` / `cd` / `select` / `copy` / `move` / `trash` / `restore` / `search` / `preview` / `ops-list` / `op-cancel` / `state` | One-shot JSON ops against the running daemon. |
| `mcp` | Stdio JSON-RPC MCP server for the `file_*` tools. |
| `waybar` | One-shot waybar custom-module tile (running-op count). Exits 0 even if the daemon is down. |

Doctor probes: `file.daemon.reachable`, `file.fonts.jetbrainsmono_nerd`,
`file.niri.binds`, `file.systemd.unit_installed`,
`file.bookmarks.writable`, `file.plugins.registry`.

### Exit codes (`ipc`)

- `0` — success.
- `1` — generic failure.
- `2` — usage error.
- `3` — daemon unreachable.
- `4` — op cancelled or refused.
- `5` — plugin error.

`sy file doctor` exits `0` when every probe passed, `1` when any
probe failed, and `2` when there are warnings only (not `3` — that
code is top-level `sy doctor` warn-only).

### Examples

```bash
sy file ~
sy file doctor --json
sy file ipc state
sy file mcp
sy file waybar
```

### See also

- [How to run sy file](../how-to/run-sy-file.md)
- [How to troubleshoot sy file](../how-to/troubleshoot-sy-file.md)
- [sy file doctor](sy-file-doctor.md)
- [sy file MCP](sy-file-mcp.md)

---

## `sy plugin`

Discover, install, and inspect previewer plugins for `sy file`.
Source: `src/plugin/cli.rs`.

### Synopsis

```text
sy plugin list [--json]
sy plugin doctor [--json]
sy plugin install <SOURCE> [--unsigned] [--rev <REF>]
sy plugin uninstall <ID>
sy plugin enable <ID>
sy plugin disable <ID>
sy plugin exec <ID> <METHOD> [--params <JSON>]
sy plugin cat-manifest <ID>
sy plugin validate <PATH>
sy plugin reload
```

`<SOURCE>` for `install` is a directory that contains `plugin.toml`,
or a git URL prefixed `git+`. Signature verification is on unless
you pass `--unsigned` (local development only).

### Exit codes

- `0` — success.
- `1` — generic failure.
- `2` — usage or validation (bad args, bad glob, malformed TOML).
- `6` — manifest invalid at install.
- `7` — signature mismatch at install.
- `8` — plugin unreachable or unhealthy (`doctor` uses this when any
  check fails).

### Examples

```bash
sy plugin list --json
sy plugin doctor --json
sy plugin install ./crates/sy-plugin-md
sy plugin exec sy-plugin-md preview --params '{"path":"README.md"}'
```

### See also

- [How to write a sy plugin](../how-to/write-a-sy-plugin.md)
- [sy file doctor](sy-file-doctor.md)

---

## `sy mon`

On-demand Wayland layer-shell health dashboard plus a 1 Hz
aggregator. Bare `sy mon` toggles the popup (`Mod+M` / `Super+m`).
Without `gui-iced`, bare `sy mon` falls back to `snapshot --json`.
Source: `src/mon/cli.rs`.

### Synopsis

```text
sy mon
sy mon collect [--history-size N] [--tick-ms MS] [--bind PATH] [--history-path PATH]
sy mon snapshot [--json]
sy mon mcp
sy mon open
sy mon close
sy mon doctor [--json]
sy mon waybar
```

### Subcommands

| Subcommand | Purpose |
|------------|---------|
| *(none)* / `open` / `close` | Toggle, open, or close the iced layer-shell popup. `close` is idempotent. |
| `collect` | Long-lived aggregator (`sy-mon-collect.service`). Ring default 600 s, tick 1000 ms. Socket `$XDG_RUNTIME_DIR/sy/mon.sock`. |
| `snapshot` | Latest `SystemSnapshot`. `--json` is the machine document. |
| `mcp` | Stdio JSON-RPC: `system.mon.snapshot` and `system.mon.history`. |
| `doctor` | Plumbing checks (`mon.collect.running`, per-plane sockets, history writable). |
| `waybar` | One-shot `ok` / `degraded` / `down` tile. Missing aggregator → `down`, not an error exit. |

### Exit codes (`snapshot`)

- `0` — success.
- `3` — aggregator unreachable after the connect-retry budget
  (same code as doctor warn-only / power drift, so agents dispatch
  identically).

### Examples

```bash
sy mon
sy mon snapshot --json
sy mon doctor --json
sy mon mcp
sy mon waybar
```

### See also

- [mon schema](../agents/mon-schema.md)
- [mon remote scrape](../admin/mon-remote.md)

---

## `sy syauth`

Phone-as-key sudo applet. Wraps upstream `syauth`. Source:
`src/syauth.rs`.

### Synopsis

```text
sy syauth [<ACTION>] [--waybar] [--service <NAME>] [--control <FLAG>] [--yes]
```

### Options

| Name        | Type   | Default | Env | Description                                                                          |
|-------------|--------|---------|-----|--------------------------------------------------------------------------------------|
| `<ACTION>`  | enum   | `status` | —  | `accept | reject | status | install-pam | uninstall-pam | doctor`. Mutually exclusive with `--waybar`. |
| `--waybar`  | bool   | `false`  | —  | Emit waybar-compatible JSON for the bar slot.                                        |
| `--service` | string | none     | —  | PAM service for `install-pam` / `uninstall-pam` (e.g. `sudo`). Required for `install-pam`. |
| `--control` | string | `sufficient` | — | PAM control flag for `install-pam`.                                                |
| `--yes`     | bool   | `false`  | —  | Skip the upstream CLI's interactive confirmation.                                    |

### Exit codes (`doctor`)

- `0` — all-ok.
- `1` — any `FAIL` row.
- `2` — `WARN`-only.

### Examples

```bash
sy syauth status
sy syauth install-pam --service sudo
sy syauth doctor
sy syauth --waybar
```

### See also

- [tutorial: syauth setup](../tutorials/syauth-setup.md)
- [how-to: troubleshoot syauth](../how-to/troubleshoot-syauth.md)
- [reference: syauth PAM module](syauth-pam-module.md)

---

## Bar-tile applets

The remaining subcommands are single-purpose bar-tile applets the
waybar `custom/sy-*` modules invoke. They take `--waybar` rather than
`--json` to match waybar's per-line JSON schema.

### `sy bat`

Battery applet. `--waybar` emits the bar JSON; no args prints a
human-readable summary.

```bash
sy bat
sy bat --waybar
```

### `sy bright`

Display brightness via `brightnessctl`.

| Name         | Type   | Default | Env | Description                                                |
|--------------|--------|---------|-----|------------------------------------------------------------|
| `<ACTION>`   | enum   | none    | —   | `up | down`. Mutually exclusive with `--waybar`.           |
| `--waybar`   | bool   | `false` | —   | Emit waybar JSON instead of acting.                        |

### `sy bt`

Bluetooth menu / status (fuzzel dropdown; `--waybar` for bar JSON).

### `sy disk`

Disk applet — bar tile when free space on `/` is below the threshold;
no args opens a fuzzel cleanup picker.

| Name              | Type | Default | Env                      | Description                            |
|-------------------|------|---------|--------------------------|----------------------------------------|
| `--waybar`        | bool | `false` | —                        | Emit waybar JSON.                      |
| `--threshold-gib` | u64  | `30`    | `SY_DISK_THRESHOLD_GIB`  | Override the low-space threshold.      |

### `sy fido`

FIDO/U2F auth for swaylock via `pam_u2f` (`enable | disable | status`).

### `sy gpu`

NVIDIA GPU applet — bar tile showing VRAM pressure + util.

### `sy net`

Fuzzel-based network dropdown (wifi, VPN, toggles, `nmtui`). No args.

### `sy notif`

Notification watcher/counter.

| Name         | Type   | Default | Env | Description                                                                       |
|--------------|--------|---------|-----|-----------------------------------------------------------------------------------|
| `<ACTION>`   | enum   | `menu`  | —   | `watch | waybar | count | clear | menu | list | show | read`.                    |
| `<REST...>`  | varargs | empty | —   | Trailing args (e.g. an id for `read` / `show`, flags for `list`).                 |

### `sy npu`

AMD Ryzen AI NPU applet — bar tile showing active/idle + holders.

### `sy popup <KEY>`

Toggle a named popup window. `KEY` is one of `agents | cal | nmtui`.

### `sy pwr`

Power menu: tuned profile + lock/suspend/reboot/shutdown/logout
(`--waybar` for bar JSON).

### `sy silent`

Silent hours — quiet output during a configurable window.

| Name         | Type   | Default | Env | Description                                                                       |
|--------------|--------|---------|-----|-----------------------------------------------------------------------------------|
| `<ACTION>`   | enum   | `toggle` | —  | `toggle | enable | disable | auto | status | watch`. Mutually exclusive with `--waybar`. |
| `--waybar`   | bool   | `false`  | —  | Emit waybar JSON.                                                                 |

### `sy snd <ACTION>`

Session sound jingles (silent-gated). `ACTION` is one of
`login | logout | test`.

### `sy vol`

Volume via `wpctl`.

| Name         | Type   | Default | Env | Description                                                                       |
|--------------|--------|---------|-----|-----------------------------------------------------------------------------------|
| `<ACTION>`   | enum   | none    | —   | `up | down | mute | mic-mute | pick`. Mutually exclusive with `--waybar`.        |
| `--waybar`   | bool   | `false` | —   | Emit waybar JSON instead of acting.                                               |

### `sy wallpaper`

Set the desktop wallpaper (`swaybg`).

| Name         | Type | Default | Env | Description                                                                                                          |
|--------------|------|---------|-----|----------------------------------------------------------------------------------------------------------------------|
| `<PATH>`     | path | none    | —   | Image file. Omit to print the current wallpaper.                                                                     |
| `--start`    | bool | `false` | —   | Re-spawn `swaybg` from the saved state (used by niri startup). Conflicts with `<PATH>` and `--default`.              |
| `--default`  | bool | `false` | —   | Apply the built-in default (kitten logo centered on black) and clear the saved state. Conflicts with the other two.  |

### `sy wifi`

Fuzzel-based wifi picker via `nmcli`. No args.

### `sy wwan`

Mobile broadband (USB 4G modem). Subcommands:
`enable | disable | up | down | status | modeswitch`.
`status` accepts `--json`. `modeswitch` requires `--yes`.

### `sy cal`

Interactive terminal calendar (`h/l` prev/next month, `j/k` year,
`t` today, `q` quit).

### `sy install`

Copy the running `sy` binary into `~/.local/bin` (real file,
SELinux-safe).

### `sy tg-theme`

Open the rendered Telegram palette in Telegram Desktop to apply it.

---

## See also

- [`sy apply`](#sy-apply) — the bootstrap entry point.
- [`sy spark`](#sy-spark), [`sy file`](#sy-file), [`sy plugin`](#sy-plugin), [`sy mon`](#sy-mon)
- [tutorial: getting started](../tutorials/getting-started.md)
- [how-to: add a knowledge source](../how-to/add-a-knowledge-source.md)
- [`CLAUDE.md`](../../CLAUDE.md) — the CLIG + agent-friendly CLI contract.
- [`AGENTS.md`](../../AGENTS.md) — coding-agent persona and non-negotiables.
