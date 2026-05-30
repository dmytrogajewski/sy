//! Example previewer that calls the typed `host::fs::read` PDK helper
//! and folds the returned bytes into its preview reply. Exists so the
//! `tests/host_fn_typed.rs` integration test can lock the typed host-
//! fn surface end-to-end against a real binary the macro generated.

use sy_plugin_pdk::prelude::*;

define_plugin! {
    id: "sy-plugin-pdk-host-fn-reader",
    api: "1",
    capabilities: [Previewer { mime: "text/plain" }],
    handlers: {
        "preview": |req: PreviewReq, host| -> Result<PreviewResp> {
            // The PDK gives us a typed `Vec<u8>` (or `RpcError`) —
            // no JSON-RPC boilerplate at the call site.
            let body = sy_plugin_pdk::types::host::fs::read(&host, &req.path)
                .await
                .map_err(|e| anyhow::anyhow!("host.fs.read({}): {e}", req.path))?;
            let text = String::from_utf8_lossy(&body).into_owned();
            Ok(PreviewResp::text(format!("got {} bytes: {text}", body.len())))
        },
    }
}
