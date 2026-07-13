//! Typed wire shapes for the PDK.
//!
//! Mirrors the host's [SPEC §4.2.4 capability methods][spec-caps]
//! request / response shapes as plain `serde::Deserialize`-able
//! structs, plus the [§4.2.5 host-callable namespace][spec-host] as
//! the [`host`] sub-module. Plugin authors operate on these typed
//! shapes; the runtime ([`crate::runtime`]) translates them to / from
//! `serde_json::Value` at the JSON-RPC boundary.
//!
//! Custom error codes match the host's
//! [`src/plugin/rpc.rs`][host-rpc] verbatim — the integer values are
//! part of the plugin contract and must never drift between PDK and
//! host.
//!
//! [spec-caps]: ../../../specs/research/sy-file-manager-plugins/SPEC.md#424-capability-methods-host-→-plugin
//! [spec-host]: ../../../specs/research/sy-file-manager-plugins/SPEC.md#425-host-callable-methods-plugin-→-host
//! [host-rpc]: ../../../src/plugin/rpc.rs
use serde::{Deserialize, Serialize};

/// JSON-RPC error code constants the PDK can return verbatim. Mirrors
/// `src/plugin/rpc.rs` so re-numbering breaks PDK + host together.
pub mod codes {
    /// SPEC §4.2.2 reserved JSON-RPC code for "method not found".
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// SPEC §4.2.2 reserved JSON-RPC code for "invalid params".
    pub const INVALID_PARAMS: i32 = -32602;
    /// SPEC §4.2.2 reserved JSON-RPC code for an internal error.
    pub const INTERNAL_ERROR: i32 = -32603;
    /// SPEC §4.2.2 `-32099 CAP_NOT_GRANTED`.
    pub const CAP_NOT_GRANTED: i32 = -32099;
}

/// SPEC §4.2.4 `preview` request params for a `previewer` capability.
///
/// Plugin authors receive this typed shape directly — the runtime
/// deserialises the raw `params` blob for them. The four sizing
/// fields are optional so a previewer that ignores them (the canonical
/// echo previewer) can still parse the wire body.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PreviewReq {
    /// Absolute path to the file the host wants previewed.
    pub path: String,
    /// MIME type the host sniffed (may be empty if unknown).
    #[serde(default)]
    pub mime: String,
    /// Pane width budget in px. Optional — `0` means "no hint".
    #[serde(default)]
    pub max_width: u32,
    /// Pane height budget in px.
    #[serde(default)]
    pub max_height: u32,
    /// Logical scroll skip in lines. SPEC §4.2.4 calls this
    /// `scroll_skip`; zero on first preview.
    #[serde(default)]
    pub scroll_skip: u32,
}

/// SPEC §4.2.4 `preview` result.
///
/// Carries either a rendered PNG or a span-styled text block. Authors
/// usually build this through [`PreviewResp::text`] /
/// [`PreviewResp::image`] so the JSON shape stays correct.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PreviewResp {
    /// Inline PNG (base64-encoded for the wire). Mutually exclusive
    /// with `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<PreviewImage>,
    /// Plain UTF-8 body (the host renders it via its own text engine).
    /// Mutually exclusive with `image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl PreviewResp {
    /// Build a text-only preview response.
    pub fn text(body: impl Into<String>) -> Self {
        Self {
            image: None,
            text: Some(body.into()),
        }
    }

    /// Build an image preview response from a base64-encoded PNG and
    /// its rendered dimensions.
    pub fn image(png_base64: impl Into<String>, w: u32, h: u32) -> Self {
        Self {
            image: Some(PreviewImage {
                png_base64: png_base64.into(),
                w,
                h,
            }),
            text: None,
        }
    }
}

/// SPEC §4.2.4 `preview` image payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewImage {
    /// Base64-encoded PNG body (no URL prefix).
    pub png_base64: String,
    /// Rendered width in pixels.
    pub w: u32,
    /// Rendered height in pixels.
    pub h: u32,
}

/// Capability declaration as it appears in `initialize.result.capabilities`.
///
/// Plugin authors usually construct these via the macro DSL; the typed
/// struct is exposed so the runtime can validate the shape at compile
/// time without re-parsing JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capability {
    /// SPEC §4.2.4 capability kind (`previewer`, `opener`, `action`, …).
    pub kind: String,
    /// Optional URL glob predicate (`*.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Optional MIME glob predicate (`text/*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

/// Structured error a plugin handler can return to surface a JSON-RPC
/// error object to the host (instead of an opaque internal error).
///
/// Bubbles up through `anyhow::Error::downcast_ref` in the runtime
/// dispatch path — authors return `Err(HandlerError::new(code,
/// message).with_data(...).into())` and the runtime preserves the
/// numeric code + structured `data` on the wire.
#[derive(Debug, Clone)]
pub struct HandlerError {
    /// JSON-RPC numeric code (see [`codes`] for SPEC §4.2.2 values).
    pub code: i32,
    /// Short human-readable summary.
    pub message: String,
    /// Optional structured detail block. `Value::Null` when omitted.
    pub data: serde_json::Value,
}

impl HandlerError {
    /// Build a new handler error with no `data` payload.
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: serde_json::Value::Null,
        }
    }

    /// Attach a structured `data` block to the error.
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HandlerError(code={}, message={:?})",
            self.code, self.message
        )
    }
}

impl std::error::Error for HandlerError {}

/// Error variants surfaced by the runtime back to the plugin author.
///
/// `Transport` means the stdio pipe is broken or a frame failed to
/// decode (the runtime usually exits the main loop and the process
/// terminates). `HostError` is what a `host::fs::read` call returns
/// when the host's JSON-RPC reply carries an `error` object.
#[derive(Debug)]
pub enum RpcError {
    /// stdio transport-layer failure (frame decode, EOF, …).
    Transport(String),
    /// Host returned a JSON-RPC error object in response to a plugin-
    /// initiated request.
    Host {
        /// Numeric JSON-RPC error code (e.g. `-32099 CAP_NOT_GRANTED`).
        code: i32,
        /// Short message from the host.
        message: String,
        /// Structured detail block from the host.
        data: serde_json::Value,
    },
    /// JSON (de)serialisation failure.
    Json(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Transport(s) => write!(f, "transport: {s}"),
            RpcError::Host {
                code,
                message,
                data,
            } => write!(f, "host error code={code} message={message} data={data}"),
            RpcError::Json(s) => write!(f, "json: {s}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// Host-callable namespace — the thin typed wrappers around the SPEC
/// §4.2.5 methods (`host.fs.read`, `host.notify.waybar`, …).
///
/// Functions in this module are async because they post a request
/// onto the runtime's outbound writer and await the matching response
/// in the supervisor's request-id table. The runtime gives each
/// plugin author a [`crate::runtime::HostHandle`] reference at handler-call time via
/// the macro DSL; reach for these helpers through that handle.
pub mod host {
    use super::RpcError;
    use crate::runtime::HostHandle;
    use serde_json::json;

    /// Filesystem host fns (SPEC §4.2.5 `host.fs.*`).
    pub mod fs {
        use super::*;

        /// Call `host.fs.read` with the given path. Returns the file's
        /// raw bytes on success or an [`RpcError::Host`] carrying the
        /// host's JSON-RPC error code (e.g. `-32099 CAP_NOT_GRANTED`
        /// when the manifest didn't allow this path).
        pub async fn read(host: &HostHandle, path: &str) -> Result<Vec<u8>, RpcError> {
            let result = host.call("host.fs.read", json!({ "path": path })).await?;
            let b64 = result
                .get("bytes_base64")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError::Json("host.fs.read result missing bytes_base64".into()))?;
            crate::runtime::base64_decode(b64)
                .map_err(|e| RpcError::Json(format!("host.fs.read base64: {e}")))
        }
    }

    /// Notification host fns (SPEC §4.2.5 `host.notify.*`).
    pub mod notify {
        use super::*;

        /// Push a waybar pill (SPEC §4.2.5 `host.notify.waybar`).
        pub async fn waybar(host: &HostHandle, text: &str, tooltip: &str) -> Result<(), RpcError> {
            host.call(
                "host.notify.waybar",
                json!({ "text": text, "tooltip": tooltip }),
            )
            .await
            .map(|_| ())
        }
    }
}
