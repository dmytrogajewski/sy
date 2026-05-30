//! Rust PDK for `sy file` plugins — the ergonomic path from
//! [plugin SPEC §3.3 item 10][spec].
//!
//! ```ignore
//! use sy_plugin_pdk::prelude::*;
//!
//! define_plugin! {
//!     id: "echo",
//!     api: "1",
//!     capabilities: [Previewer { mime: "text/plain" }],
//!     handlers: {
//!         preview: |req: PreviewReq| -> anyhow::Result<PreviewResp> {
//!             Ok(PreviewResp::text(format!("hello {}", req.path)))
//!         }
//!     }
//! }
//! ```
//!
//! The macro expands into a `main` that wires `tokio::io::{stdin,
//! stdout}` through the SPEC §4.2 LSP framing + JSON-RPC 2.0 codec the
//! host's [`crate::plugin::transport`][host-transport] enforces, and
//! routes `initialize` / `ping` / `shutdown` / `exit` automatically.
//! The author only writes their capability handlers.
//!
//! Zero `unwrap` / `expect` in the runtime path — malformed frames
//! surface as `RpcError::Protocol` and the macro never panics. See the
//! README's "20-line previewer" snippet for the canonical wire shape.
//!
//! [spec]: ../../../specs/research/sy-file-manager-plugins/SPEC.md#33-scope
//! [host-transport]: ../../../src/plugin/transport.rs

pub mod runtime;
pub mod types;
#[macro_use]
pub mod macros;

/// Private re-exports the [`define_plugin!`] macro uses to address
/// its dependencies. Plugin authors should never reach into this
/// module directly — its surface is allowed to change between PDK
/// releases. The re-exports exist so a third-party crate that lists
/// only `sy-plugin-pdk` as a dep (and not `serde_json` / `tokio` /
/// `anyhow`) still compiles the macro expansion.
#[doc(hidden)]
pub mod __priv {
    pub use anyhow;
    pub use serde_json;
    pub use tokio;
}

/// Convenience re-exports for plugin authors. `use
/// sy_plugin_pdk::prelude::*` should be the only `use` line a 20-line
/// previewer needs.
pub mod prelude {
    pub use crate::define_plugin;
    pub use crate::runtime::{run, PluginInfo};
    pub use crate::types::{
        host, Capability, HandlerError, PreviewImage, PreviewReq, PreviewResp, RpcError,
    };
    pub use anyhow::Result;
}
