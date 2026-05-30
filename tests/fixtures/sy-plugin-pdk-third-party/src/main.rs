//! Third-party previewer fixture for the sy-file-manager Step 11
//! journey E2E. The body is the verbatim 20-line example from
//! `crates/sy-plugin-pdk/README.md` — proving an out-of-tree author
//! can land a journey-J3-shaped previewer using only
//! `sy-plugin-pdk` as a path dep.

use sy_plugin_pdk::prelude::*;

define_plugin! {
    id: "sy-plugin-pdk-third-party",
    api: "1",
    capabilities: [Previewer { mime: "text/plain" }],
    handlers: {
        "preview": |req: PreviewReq| -> Result<PreviewResp> {
            Ok(PreviewResp::text(format!("third-party preview of {}", req.path)))
        },
    }
}
