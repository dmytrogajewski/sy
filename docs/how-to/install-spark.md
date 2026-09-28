# Use Sparkplane through sy

Sparkplane is maintained in [Sumatoshi-tech/sparkplane](https://github.com/Sumatoshi-tech/sparkplane).
It works independently; `sy spark HOST ...` is a process bridge.

## Pin and install the client

Configure `[integrations.sparkplane]` in `sy.toml` with `enabled = true`.
Its `[integrations.sparkplane.release]` table requires:

| Field | Meaning |
|---|---|
| `version` | Exact release version without the `v` tag prefix |
| `target` | `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu` |
| `sha256` | Exact client digest in the signed release inventory |
| `public_key` | Independently verified minisign public key, base64 form |

Use the published Sparkplane release and independently trusted public key.
Do not trust a key just because it arrived with a download. Run
`sy apply --only sparkplane --dry-run` to inspect installation, then
`sy apply --only sparkplane`. The scoped form leaves desktop settings,
services, the sy executable and other coding-agent packages untouched.
`SY_APPLY_ONLY=sparkplane` selects the same scope. A general `sy apply`
also installs the enabled integration alongside its other components.

The installer verifies signatures and hashes before atomically activating the
client under `$XDG_DATA_HOME/sparkplane/client/`. Disabled integrations perform
no downloads or writes. Client updates happen only during apply; this never
implicitly upgrades the appliance or reloads a model.

The installed executable also works directly, without sy:
`~/.local/share/sparkplane/client/current/sparkplane HOST ...` (adjust the data
root if using `XDG_DATA_HOME`). On this workstation, `~/.local/bin/sparkplane`
links to that executable, so `sparkplane HOST ...` uses the same verified pin.

## Use the bridge

```sh
sy spark --help
sy spark dgx-spark status --json
sy spark dgx-spark ps --json
sy spark dgx-spark launch codex --model qwen3.8:flash-next
```

The sandbox keeps its existing filesystem and approval policy. If the agent
needs internet access for dependency installation, documentation lookup, or a
remote API, opt in for that launch:

```sh
sy spark dgx-spark launch codex --allow-network -- --sandbox workspace-write
```

`--allow-network` is session-only. It can also be set with
`SPARKPLANE_LAUNCH_ALLOW_NETWORK=true`; it is not persisted in Sparkplane
launch state. See the [Sparkplane network-control reference](https://github.com/Sumatoshi-tech/sparkplane/blob/main/docs/reference/spark.md#internet-access-for-development)
for client-specific behavior.

The bridge verifies the installed executable and its protocol, then replaces
itself using `exec`. It forwards arguments and exit status without a shell.
It does not initialize the AMD runtime, discover a repository or download
software. A missing installation produces an actionable error.

`SY_SPARK_*` environment variables are translated to `SPARKPLANE_*` unless
the canonical variable already exists. Sparkplane owns host profiles and
credentials under `$XDG_CONFIG_HOME/sparkplane/`; sy stores no appliance secrets.

For appliance installation, upgrades, rollback and migration, follow
[Sparkplane's operator guide](https://github.com/Sumatoshi-tech/sparkplane/blob/main/docs/how-to/install-spark.md).
Existing sy-managed appliances require the signed namespace migration before
the new client can use the renamed control API. Do not point an old appliance
at a new client and assume the control APIs are interchangeable.
