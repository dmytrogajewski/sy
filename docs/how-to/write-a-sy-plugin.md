<!-- Template source: Good Docs Project how-to template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/how-to. Diátaxis quadrant: how-to. -->

# How to write a sy file previewer plugin

## Goal

Ship a small previewer for `sy file` in Rust, Python, or Bash. A
previewer reads a file path, returns a preview body, and exits. The
host starts and stops the process, applies resource limits, and
routes requests based on the MIME types in `plugin.toml`.

## Prerequisites

- `sy` is installed and `sy plugin doctor` exits `0`. See
  [sy file doctor](../reference/sy-file-doctor.md) for the
  `sy.plugin.doctor/v1` envelope.
- Rust example: `cargo` and this repo cloned so
  `crates/sy-plugin-pdk` is on disk.
- Python example: Python 3.11+ on `$PATH`.
- Bash example: a POSIX `sh` and `jq` on `$PATH`.

The wire format is JSON-RPC on stdin/stdout (one JSON object per
line, with an LSP-style length header). The Rust PDK hides that
framing. Python and Bash speak it directly. The full contract lives
in the [plugin SPEC](../../specs/research/sy-file-manager-plugins/SPEC.md)
if you need every field.

## Steps

### Rust — via the in-tree PDK

The 20-line example is
[`crates/sy-plugin-pdk/examples/echo_previewer.rs`](../../crates/sy-plugin-pdk/examples/echo_previewer.rs):

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

`define_plugin!` registers `initialize` / `ping` / `shutdown` /
`exit`, advertises a `text/plain` previewer, and dispatches
`preview` to your closure. Pair the binary with this manifest:

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

Build, install, then check doctor:

```bash {.no-test}
cargo build --release --example echo_previewer
sy plugin install ./target/release/examples/
sy plugin doctor --json
```

### Python — speaking the wire format directly

The minimum loop reads one JSON-RPC object per line, dispatches on
`method`, and writes one object per line back:

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

Save as `echo_previewer.py`, `chmod +x`, and use a manifest like the
Rust one with `exec = "./echo_previewer.py"` and
`id = "sy-plugin-pyecho"`. `sy plugin install` copies both files
into the plugin directory.

### Bash — speaking the wire format directly

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

Save as `echo_previewer.sh`, `chmod +x`, pair with
`exec = "./echo_previewer.sh"` and `id = "sy-plugin-bashecho"`, then
`sy plugin install` the directory. The host treats the script like
any other plugin binary: pipes, shutdown signals, resource limits.

## Result

After `sy plugin install` finishes, run:

```bash {.no-test}
sy plugin doctor --json
```

Each installed plugin contributes three rows: `manifest.valid`,
`binary.reachable`, and `capability.routes`. All three must be
`"ok": true` for your id. If `capability.routes` is false, another
plugin already owns `text/plain` — pick a different MIME or raise
`priority` on your `[[capability]]` row.

If a row stays false, see
[How to troubleshoot a sy file plugin](troubleshoot-sy-plugin.md).

## See also

- [How to troubleshoot a sy file plugin](troubleshoot-sy-plugin.md)
- [How to run sy file](run-sy-file.md)
- [CLI: `sy plugin`](../reference/cli.md#sy-plugin)
- [sy file doctor](../reference/sy-file-doctor.md)
