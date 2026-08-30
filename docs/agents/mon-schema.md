# `sy mon` snapshot schema

Who this is for: agents and scripts that parse
`sy mon snapshot --json`, the `system.mon.snapshot` IPC method, or
the MCP tools of the same name.

This is the field list. Humans who just want the popup can press
`Super+m` or run `sy mon`.

Source of truth:
[`crates/sy-core/src/mon/snapshot.rs`](../../crates/sy-core/src/mon/snapshot.rs).
A filled example lives at
[`crates/sy-core/tests/snapshots/mon/spec-example.json`](../../crates/sy-core/tests/snapshots/mon/spec-example.json).

## `SystemSnapshot`

One coherent slice of host + plane state at a single instant. The
aggregator publishes one snapshot per 1 Hz tick.

| Field            | Type                          | Meaning |
|------------------|-------------------------------|---------|
| `schema_version` | `u32`                         | See [SemVer policy](#schema_version-semver-policy). |
| `captured_at_ms` | `u64`                         | Capture instant in Unix milliseconds. The aggregator stamps this once per tick *after* every panel has been collected. |
| `cpu`            | [`CpuPanel`](#cpupanel)       | Host CPU panel. |
| `mem`            | [`MemPanel`](#mempanel)       | Host memory panel (MiB-scaled). |
| `gpu`            | `Vec<GpuPanel>`               | One entry per physical GPU. Empty on a host without any discoverable GPU. |
| `npu`            | [`NpuPanel`](#npupanel)       | AMD Ryzen AI NPU panel. |
| `net`            | `Vec<NetIfacePanel>`          | One entry per interface in `/proc/net/dev`. |
| `disk`           | `Vec<DiskDevicePanel>`        | One entry per device in `/proc/diskstats`. |
| `aiplane`        | `AiplanePanel`                | Per-workload-kind queue / warm pool / latency. |
| `knowledge`      | `KnowledgePanel`              | Collections, docs indexed, embed throughput, search QPS. |
| `agents`         | `AgentsPanel`                 | Running count, RSS total, recent policy denials. |
| `supervisor`     | `SupervisorPanel`             | One row per supervised plane. |
| `errors`         | `Vec<MonError>`               | Per-source errors observed during the tick. Empty on a fully-healthy tick. |

### `CpuPanel`

- `per_core_util_pct: Vec<f32>` — percent busy per logical core in `cpuN`
  order, range `0.0..=100.0`.
- `freq_mhz: Vec<u32>` — scaling-current frequency in MHz, same order.
- `temp_c: f32` — package temperature in Celsius. Zero when the host has
  no resolvable thermal zone; the aggregator tags `errors[]` in that
  case rather than dropping the panel.
- `load_avg: [f32; 3]` — `/proc/loadavg` 1 / 5 / 15-minute values.

### `MemPanel`

- `total_mib: u64`, `used_mib: u64`, `swap_used_mib: u64`.

### `NpuPanel`

- `vendor: String` — short tag (`"amd-xdna"`). Empty when no NPU is
  present.
- `util_pct: u32`, `active: bool`, `fw_version: String`, `power_w: f32`.
- `holders: Vec<String>` — live holders of `/dev/accel/accel0` as
  reported by `lsof`. Usually `["sy-aiplane"]`; empty when idle.

### `MonError`

- `plane: String` — `"host"` for sensor reads, otherwise the plane name
  (`"aiplane"`, `"knowledge"`, etc.).
- `kind: String` — short discriminator (`"timeout"`, `"missing_socket"`,
  `"parse_error"`).
- `message: String` — free-form detail for human display.

All other panel structs follow the same shape. The exhaustive set is
defined in `crates/sy-core/src/mon/snapshot.rs`; consult that file
for the authoritative field list.

## MCP tool surface

The `sy mon mcp` stdio JSON-RPC server advertises two tools:

### `system.mon.snapshot`

Returns the latest `SystemSnapshot` from the running aggregator.
Request body is `{}`. The response is the full `SystemSnapshot` JSON
object documented above.

```json
{ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
  "params": { "name": "system.mon.snapshot", "arguments": {} } }
```

### `system.mon.history`

Returns a contiguous slice of the in-memory ring buffer. Request
body is `{ "n": <count> }` (the number of past snapshots to return,
capped at the ring depth). The response is a JSON array of
`SystemSnapshot` objects ordered oldest-first.

```json
{ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
  "params": { "name": "system.mon.history", "arguments": { "n": 60 } } }
```

The streaming `system.mon.subscribe` op lives on the sy-ipc UDS
socket only — not on MCP. SPEC §7 OQ 4 keeps MCP poll-only.

## `schema_version` SemVer policy

`SystemSnapshot::schema_version` is a `u32` that follows SemVer-major
semantics:

- **Bumping is breaking.** A field rename, a removal, or a type change
  on any existing field requires bumping `schema_version` and shipping
  a deprecation note in `CHANGELOG.md`.
- **Additive changes do not bump.** Adding a new optional field, a new
  panel, or a new variant to an `enum`-style string is not a breaking
  change. Consumers that ignore unknown fields keep working; consumers
  that schema-validate against the spec must accept supersets.
- **Consumers MAY refuse** to parse a snapshot whose `schema_version`
  is higher than they know. The wire shape is documented per-version;
  cross-version compatibility is not promised.

Current version: `2`. Version 2 removes the retired experimental power
panel. See `SCHEMA_VERSION` in
`crates/sy-core/src/mon/snapshot.rs`.
