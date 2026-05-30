//! Canonical 20-line PDK previewer used by both the `echo.rs`
//! integration test and the README's "20-line previewer" snippet.
//!
//! The body below (between the `BEGIN PDK 20-LINE EXAMPLE` and `END`
//! markers) is what `crates/sy-plugin-pdk/README.md` quotes verbatim —
//! keep the line count under 20 to honour the Step 11 DoD.

// BEGIN PDK 20-LINE EXAMPLE
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
// END PDK 20-LINE EXAMPLE
