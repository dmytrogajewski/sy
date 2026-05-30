<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to write a `sy file` previewer plugin

## Goal

Ship a minimum-viable previewer plugin for `sy file` in your language
of choice. A previewer plugin reads a file path off stdin, returns a
preview body, and shuts down. The host supervises the process under
SPEC §4.3 (signal ladder, capability negotiation, resource limits)
and routes preview requests to it based on the manifest's
`[[capability]]` rows. Three language examples ship below; pick one,
copy the manifest, and `sy plugin install` the result.

The wire protocol is JSON-RPC over line-delimited stdio with an LSP
framing prelude (per
[plugin SPEC §4.2](../../specs/research/sy-file-manager-plugins/SPEC.md)).
The Rust PDK (`sy-plugin-pdk`) hides the framing so a Rust author can
write a 20-line previewer; the Python and Bash examples speak the
framing directly because there is no out-of-tree PDK yet for those
runtimes.

## Prerequisites

- `sy` is installed and `sy plugin doctor` exits `0`. See
  [`docs/reference/sy-file-doctor.md`](../reference/sy-file-doctor.md)
  for the `sy.plugin.doctor/v1` envelope.
- For the Rust example: `cargo` and the workspace cloned at
  `~/sources/sy` so `crates/sy-plugin-pdk` is on disk.
- For the Python example: Python 3.11+ on `$PATH`.
- For the Bash example: a POSIX `sh` and `jq` on `$PATH`.

## Steps

### Rust — via the in-tree PDK

The canonical 20-line previewer lives in the workspace at
[`crates/sy-plugin-pdk/examples/echo_previewer.rs`](../../crates/sy-plugin-pdk/examples/echo_previewer.rs).
The body is small enough to quote in full:

```rust
use sy_plugin_pdk::prelude::*;

define_plugin! {
    id: "sy-plugin-pdk-echo",
    api: "1",
    capabilities: [Previewer { mime: "text/plain" }],
    handlers: {
        "preview": |req: PreviewReq| -> Result<PreviewResp> {
            Ok(PreviewResp::text(format!("echo {} (mime={})", req.path, req.mime)))
        },
    }
}
```

`define_plugin!` registers the `initialize` / `ping` / `shutdown` /
`exit` lifecycle (SPEC §4.2.3), advertises the previewer capability
for `text/plain` files, and dispatches incoming `preview` requests to
your typed closure. The macro shape mirrors
[`crates/sy-plugin-md`](../../crates/sy-plugin-md) — the in-tree
markdown previewer canary — so cargo will compile your plugin against
the same workspace-pinned `serde` / `serde_json` / `tokio` / `anyhow`
versions. Pair the binary with a manifest at the same prefix:

```toml
api = "1"

[plugin]
id = "sy-plugin-pdk-echo"
name = "Echo previewer (Rust)"
version = "0.0.1"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "./echo_previewer"

[[capability]]
kind = "previewer"
mime = "text/plain"

[needs]
fs_read = []
fs_write = []
preview = []
knowledge = []
network = []
exec = []

[limits]
memory_mb = 64
cpu_seconds = 10
nofile = 64
spawn_timeout_ms = 500
shutdown_timeout_ms = 500
```

Build, install, then verify the plugin shows up under
`sy plugin doctor`:

```bash {.no-test}
cargo build --release --example echo_previewer
sy plugin install ./target/release/examples/
sy plugin doctor --json
```

### Python — speaking the wire format directly

The minimum loop reads one JSON-RPC frame per line, dispatches on the
`method` field, and writes one JSON-RPC frame per line back. This
implementation handles the three mandatory lifecycle methods and
`preview`:

```python
#!/usr/bin/env python3
import json
import sys


def handle(req: dict) -> dict:
    method = req.get("method", "")
    if method == "initialize":
        return {"capabilities": [{"kind": "previewer", "mime": "text/plain"}]}
    if method == "ping":
        return {"ok": True}
    if method == "preview":
        path = req.get("params", {}).get("path", "")
        return {"mime": "text/plain", "body": f"echo {path}"}
    if method in ("shutdown", "exit"):
        return {"ok": True}
    return {"error": {"code": -32601, "message": f"unknown method {method}"}}


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    resp = {"jsonrpc": "2.0", "id": req.get("id"), "result": handle(req)}
    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
    if req.get("method") == "exit":
        break
```

Save as `echo_previewer.py`, chmod `+x`, and pair with a manifest
identical to the Rust one above but with `exec = "./echo_previewer.py"`
and `id = "sy-plugin-pyecho"`. `sy plugin install` copies both files
to `$SY_PLUGIN_DIR/<id>/` and registers the capability.

### Bash — speaking the wire format directly

The shell variant is the smallest illustration of the protocol. It
uses `jq` for JSON parsing so the loop body stays under 20 lines:

```bash
#!/usr/bin/env bash
set -eu
while IFS= read -r line; do
    method=$(printf '%s' "$line" | jq -r '.method // empty')
    id=$(printf '%s' "$line" | jq -r '.id // null')
    case "$method" in
        initialize) result='{"capabilities":[{"kind":"previewer","mime":"text/plain"}]}' ;;
        ping)       result='{"ok":true}' ;;
        preview)    path=$(printf '%s' "$line" | jq -r '.params.path // ""')
                    result=$(jq -nc --arg p "$path" '{mime:"text/plain", body:("echo " + $p)}') ;;
        shutdown|exit) result='{"ok":true}' ;;
        *)          result='{}' ;;
    esac
    jq -nc --argjson id "$id" --argjson r "$result" '{jsonrpc:"2.0", id:$id, result:$r}'
    [ "$method" = exit ] && break || true
done
```

Save as `echo_previewer.sh`, chmod `+x`, pair with a manifest using
`exec = "./echo_previewer.sh"` and `id = "sy-plugin-bashecho"`, then
`sy plugin install` the directory. The host's `proc::Supervisor`
treats the bash script the same as the Rust binary: stdin/stdout
pipes, signal ladder on shutdown, and the SPEC §4.4 resource limits.

## Verify

After `sy plugin install` finishes, run:

```bash {.no-test}
sy plugin doctor --json
```

Each installed plugin contributes three rows to the `checks[]` array:
`manifest.valid`, `binary.reachable`, and `capability.routes`. All
three rows must be `"ok": true` for your plugin (the row's `plugin`
field carries your `id`). If `capability.routes` is `false`, another
installed plugin shadows your previewer for `text/plain` — pick a
distinct MIME or raise the `priority` field on your `[[capability]]`
row.

## Troubleshooting

- **`manifest.valid: false`** — re-read the SPEC §4.1 schema. Common
  failures: missing `api`, missing `[plugin]` table, or a
  `[[capability]]` row with an unknown `kind`.
- **`binary.reachable: false`** — the `exec` path in your manifest
  doesn't resolve relative to the manifest directory, or the binary
  doesn't have the execute bit. Add it with `chmod +x`.
- **`capability.routes: false`** — another plugin claims the same
  `(kind, mime)` tuple. Run `sy plugin list --json` to find the
  conflicting id, and either uninstall it or differentiate your MIME.
- **Plugin process crashes mid-preview** — the host's supervisor
  retries with exponential backoff (100 ms → 200 ms → 400 ms) and
  then marks the plugin `Unhealthy`. Tail
  `journalctl --user -t sy-file` for the crash reason.
