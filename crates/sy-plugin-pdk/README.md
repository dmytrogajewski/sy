# sy-plugin-pdk

Rust Plugin Development Kit for `sy file` plugins
([SPEC §3.3 item 10](../../specs/research/sy-file-manager-plugins/SPEC.md#33-scope)).

`define_plugin!` hides JSON-RPC + LSP framing, the SPEC §4.2.3
`initialize` / `ping` / `shutdown` / `exit` lifecycle, and the
SPEC §4.2.5 host-callable namespace. Plugin authors write typed
handlers; the PDK wires stdin/stdout for them.

## 20-line previewer

The full source of a working previewer plugin — same body cargo
builds at `examples/echo_previewer.rs`:

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

Build + install:

```bash
cargo build --release --example echo_previewer
sy plugin install ./target/release/examples/  # tip: ship a plugin.toml alongside
sy plugin doctor    # green check on a freshly-installed previewer
```

## Host-fn surface

`host::fs::read` returns a typed `Result<Vec<u8>, RpcError>` — no
base64 decoding at the call site:

```rust
use sy_plugin_pdk::prelude::*;

define_plugin! {
    id: "my-pdf-previewer",
    api: "1",
    capabilities: [Previewer { mime: "application/pdf" }],
    handlers: {
        "preview": |req: PreviewReq, host| -> Result<PreviewResp> {
            let body = sy_plugin_pdk::types::host::fs::read(&host, &req.path)
                .await
                .map_err(|e| anyhow::anyhow!("fs.read: {e}"))?;
            // … render PDF to PNG, return PreviewResp::image(...)
            Ok(PreviewResp::text(format!("got {} bytes", body.len())))
        },
    }
}
```

`host.notify.waybar`, `host.fs.cha`, `host.fs.write_cache`, and the
rest of the SPEC §4.2.5 surface land under `sy_plugin_pdk::types::host::*`
as they're needed. The macro automatically advertises which host
fns the plugin offers via the `initialize.result.host_methods`
array — the host's negotiation pass drops the ones the runtime
hasn't shipped yet (forward-compat per SPEC §4.1).

## Dependencies

Zero deps beyond the workspace-pinned `{serde, serde_json, tokio,
anyhow}`. Step 11 DoD: the PDK never pulls in a third-party crate
that the host doesn't already pin.

## Deferred PDKs (out-of-tree)

[plugin SPEC §3.3 item 11](../../specs/research/sy-file-manager-plugins/SPEC.md#33-scope)
calls out TypeScript / Python / Go PDKs as out-of-tree work — they're
sugar over the same JSON-RPC + LSP framing, which any language whose
binary can speak the protocol can target directly.

## License

MIT (matches the workspace).
