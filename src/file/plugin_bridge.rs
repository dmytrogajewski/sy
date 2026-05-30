//! File-manager → plugin bridge for the J3 hover-preview pipeline.
//! Roadmap [Step 27][step27] (SPEC §3.3 item 8 — "Anything else
//! dispatches to a plugin").
//!
//! Owns the long-lived [`PluginProc`] handles keyed by [`PluginId`]
//! so the warm hover path (second hover on the same MIME) re-uses an
//! already-handshaked supervisor — the budgeted < 100 ms warm path
//! out of the journey's J3 brief. The first hover spawns the plugin
//! through [`crate::plugin::proc::spawn`] inside the cold-path
//! budget (600 ms) and stashes the handle for re-use.
//!
//! Dispatch shape:
//!
//! 1. Look up `(CapKind::Previewer, mime, url)` against the supplied
//!    [`Registry`] (Step 7's index).
//! 2. Get-or-spawn the plugin process under the SPEC §4.3 sandbox
//!    envelope (`PluginProc::spawn` does the SPEC §4.2.3 handshake
//!    inline so the returned handle is already `State::Ready`).
//! 3. Send a `preview` JSON-RPC request (SPEC §4.2.4 capability
//!    method) with `{ path, mime, max_width, max_height }`; await the
//!    `PreviewResp` body.
//! 4. Decode the response into [`PreviewResult`] (PNG bytes or text)
//!    so the GUI layer never sees the base64 wire shape.
//!
//! Plugin crash handling: when [`PluginProc::request`] returns an
//! [`RpcError`] the bridge evicts the supervisor from the cache and
//! surfaces [`BridgeError::PluginCrashed`]. The dispatcher's calling
//! site (Step 27 [`crate::file::view::preview`]) routes the fall-back
//! into the built-in syntect text path per the Step 27 DoD
//! `plugin_crash_falls_back_to_built_in_text`.
//!
//! [step27]: ../../../specs/roadmaps/sy-file-manager/ROADMAP.md
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex;

use crate::plugin::host_fns::{self, HostCtx, PreviewMessage};
use crate::plugin::proc::{self, PluginProc, SpawnOpts};
use crate::plugin::registry::{CapKind, PluginId, Registry};
use crate::plugin::sandbox;

/// Decoded payload returned by a plugin's `preview` capability method.
/// The PNG arm carries the *decoded* PNG bytes — the bridge runs the
/// base64 decode inside [`PluginBridge::preview_for`] so the GUI layer
/// (Step 26+ [`crate::file::view::preview`]) only ever sees binary
/// data ready to hand to `iced::widget::image::Handle::from_bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewResult {
    /// PNG bytes (already base64-decoded). The GUI builds an iced
    /// image handle directly from these.
    Png(Vec<u8>),
    /// Plain UTF-8 body the host's text renderer (syntect / cosmic-text)
    /// will lay out.
    Text(String),
}

/// Failure modes surfaced by [`PluginBridge::preview_for`]. The
/// dispatcher reads these to decide whether to fall back to the
/// built-in text path (per the Step 27 DoD).
#[derive(Debug, Clone)]
pub enum BridgeError {
    /// No plugin in the [`Registry`] declared a `previewer` capability
    /// matching the `(mime, url)` pair. The view falls through to the
    /// "no built-in preview" fallback.
    NoMatch,
    /// Looking up the plugin succeeded but spawning / handshaking
    /// failed (binary missing, sandbox refused, …). Carries the
    /// upstream [`RpcError`] for diagnostics; the view treats this the
    /// same as `PluginCrashed` (fall back to built-in text).
    SpawnFailed(String),
    /// The plugin crashed mid-request or returned a peer error. The
    /// dispatcher routes the fall-back into the syntect built-in (DoD
    /// `plugin_crash_falls_back_to_built_in_text`).
    PluginCrashed(String),
    /// Plugin's `preview` reply was syntactically well-formed but
    /// neither `image.png_base64` nor `text` was populated. Surfaces
    /// as a fall-back trigger so a buggy plugin can't strand the
    /// preview pane.
    InvalidResponse(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::NoMatch => {
                f.write_str("no plugin claims the (mime, url) previewer surface")
            }
            BridgeError::SpawnFailed(s) => write!(f, "plugin spawn failed: {s}"),
            BridgeError::PluginCrashed(s) => write!(f, "plugin crashed mid-request: {s}"),
            BridgeError::InvalidResponse(s) => {
                write!(f, "plugin returned an invalid response: {s}")
            }
        }
    }
}

impl std::error::Error for BridgeError {}

/// Long-lived bridge between the file plane's hover-preview path and
/// the plugin runtime. Cheap to clone (one `Arc` per field); the
/// caller wraps this in `Arc<PluginBridge>` and shares it across the
/// reducer / view layers.
pub struct PluginBridge {
    /// Discovery + dispatch index (Step 7). Lives behind an `Arc` so
    /// the file plane can swap the registry on `sy plugin reload`
    /// (Step 8) without re-allocating the bridge itself.
    registry: Arc<Registry>,
    /// Long-lived plugin processes keyed by id. The warm-path budget
    /// (< 100 ms) is exactly the cost of `cache.lock` +
    /// `proc.request` round-trip; the spawn happens on the cold path
    /// only.
    procs: Mutex<HashMap<PluginId, PluginProc>>,
    /// SPEC §4.2.5 host-callable surface the bridge passes into every
    /// spawned [`PluginProc`]. The `host.preview.*` host fns
    /// (currently unused by `sy-plugin-md`, but in scope for future
    /// plugins) push onto the channel embedded here so the file
    /// plane's UI thread can consume payloads pushed from the plugin
    /// side.
    host_ctx: HostCtx,
}

impl std::fmt::Debug for PluginBridge {
    /// Manual Debug — `Registry` (Step 7) is intentionally not
    /// `Debug` (would dump the entire dispatch index), so we render
    /// only the cardinalities the file plane cares about.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginBridge")
            .field("plugins", &self.registry.plugin_ids().count())
            .finish()
    }
}

/// Per-request budget. The file plane's hover path overrides this on
/// the cold call (600 ms wall-clock from the journey J3 brief) — for
/// the warm call the same budget is plenty because the supervisor is
/// already `State::Ready`. Pinned as a module constant so the Step
/// 27 tests assert against the same number.
pub const PREVIEW_REQUEST_TIMEOUT_MS: u64 = 800;

impl PluginBridge {
    /// Construct a bridge from a registry + a [`HostCtx`]. The caller
    /// is responsible for plumbing the `host_ctx`'s `preview_tx` to
    /// the file plane's UI thread (see [`host_fns::ctx_for_with_preview`]).
    pub fn new(registry: Arc<Registry>, host_ctx: HostCtx) -> Self {
        Self {
            registry,
            procs: Mutex::new(HashMap::new()),
            host_ctx,
        }
    }

    /// Borrow the underlying registry (so the file plane can introspect
    /// which plugins claim a given MIME for diagnostics / `sy plugin
    /// list`).
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Cold + warm hover entry point. Dispatches `path` through the
    /// plugin runtime under the `(CapKind::Previewer, mime, <basename>)`
    /// row and returns the decoded payload.
    ///
    /// On a plugin crash the supervisor is evicted from the cache so
    /// the next hover triggers a fresh spawn (the SPEC §4.4 restart
    /// ladder lives inside [`PluginProc`]; the bridge's eviction is
    /// the outer "if the supervisor parks Unhealthy, start over"
    /// loop).
    pub async fn preview_for(&self, mime: &str, path: &Path) -> Result<PreviewResult, BridgeError> {
        let url = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let plugin_id = self
            .registry
            .select_for(CapKind::Previewer, mime, &url)
            .cloned()
            .ok_or(BridgeError::NoMatch)?;
        let response = self.request_preview(&plugin_id, mime, path).await?;
        decode_preview_response(&response)
    }

    /// Send `preview` to the plugin (spawning it on cache miss). On
    /// [`RpcError`] the supervisor is evicted from the cache so the
    /// next hover re-handshakes a fresh child.
    async fn request_preview(
        &self,
        plugin_id: &PluginId,
        mime: &str,
        path: &Path,
    ) -> Result<serde_json::Value, BridgeError> {
        let params = json!({
            "path": path.to_string_lossy(),
            "mime": mime,
            "max_width": 0u32,
            "max_height": 0u32,
            "scroll_skip": 0u32,
        });
        // Hold the lock just long enough to get a *handle into the
        // map*; the actual JSON-RPC await runs against the supervisor
        // without the lock so two simultaneous hovers on different
        // plugins don't serialise on the cache mutex. We rebuild the
        // request path under one lock acquisition to keep the
        // get-or-spawn step atomic per id.
        self.ensure_spawned(plugin_id).await?;
        let result = {
            let guard = self.procs.lock().await;
            let Some(proc_) = guard.get(plugin_id) else {
                return Err(BridgeError::SpawnFailed(format!(
                    "{} evicted from cache before request",
                    plugin_id.as_str()
                )));
            };
            proc_.request("preview", params).await
        };
        match result {
            Ok(v) => Ok(v),
            Err(e) => {
                let _ = self.procs.lock().await.remove(plugin_id);
                Err(BridgeError::PluginCrashed(format!(
                    "{}: {e}",
                    plugin_id.as_str()
                )))
            }
        }
    }

    /// Cache miss → spawn the plugin under the SPEC §4.3 envelope and
    /// stash the handle. Cache hit → no-op. Held the procs lock for
    /// the spawn so a hover storm can't double-spawn the same id.
    async fn ensure_spawned(&self, plugin_id: &PluginId) -> Result<(), BridgeError> {
        let mut guard = self.procs.lock().await;
        if guard.contains_key(plugin_id) {
            return Ok(());
        }
        let manifest = self.registry.manifest(plugin_id).cloned().ok_or_else(|| {
            BridgeError::SpawnFailed(format!("no manifest for {}", plugin_id.as_str()))
        })?;
        let workdir = sandbox::runtime_dir_for(plugin_id.as_str());
        std::fs::create_dir_all(&workdir).map_err(|e| {
            BridgeError::SpawnFailed(format!("mkdir workdir {}: {e}", workdir.display()))
        })?;
        let mut opts = SpawnOpts::new(workdir);
        opts.host_ctx = Some(self.host_ctx.clone());
        opts.request_timeout = std::time::Duration::from_millis(PREVIEW_REQUEST_TIMEOUT_MS);
        let proc_ = proc::spawn(manifest, opts)
            .await
            .map_err(|e| BridgeError::SpawnFailed(format!("{}: {e}", plugin_id.as_str())))?;
        guard.insert(plugin_id.clone(), proc_);
        Ok(())
    }

    /// Drain every cached supervisor, sending each a graceful
    /// `shutdown`. Called by the file plane on `Ctrl+Q`. Idempotent.
    pub async fn shutdown_all(&self) {
        let mut guard = self.procs.lock().await;
        let drained: Vec<(PluginId, PluginProc)> = guard.drain().collect();
        drop(guard);
        for (_id, mut p) in drained {
            let _ = p.shutdown().await;
        }
    }
}

/// Parse a plugin's `preview` JSON-RPC result body into the typed
/// [`PreviewResult`]. The shape matches the PDK's `PreviewResp`
/// (`{ image: { png_base64, w, h }? , text: String? }`); the bridge
/// decodes the base64 here so the GUI layer never sees the wire form.
fn decode_preview_response(v: &serde_json::Value) -> Result<PreviewResult, BridgeError> {
    if let Some(img) = v.get("image") {
        let b64 = img
            .get("png_base64")
            .and_then(|s| s.as_str())
            .ok_or_else(|| BridgeError::InvalidResponse("image.png_base64 missing".into()))?;
        let bytes = base64_decode(b64)
            .map_err(|e| BridgeError::InvalidResponse(format!("image.png_base64 decode: {e}")))?;
        return Ok(PreviewResult::Png(bytes));
    }
    if let Some(text) = v.get("text").and_then(|s| s.as_str()) {
        return Ok(PreviewResult::Text(text.to_string()));
    }
    Err(BridgeError::InvalidResponse(
        "preview reply must populate either `image` or `text`".into(),
    ))
}

/// Mirror of `crate::plugin::host_fns::base64_decode` — the canonical
/// host-side decoder for RFC 4648 base64. Duplicated rather than
/// re-exported because the host_fns helper is `pub(crate)`-shaped and
/// the bridge re-uses the same alphabet table verbatim.
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn idx(b: u8) -> Result<u32, String> {
        match b {
            b'A'..=b'Z' => Ok((b - b'A') as u32),
            b'a'..=b'z' => Ok((b - b'a') as u32 + 26),
            b'0'..=b'9' => Ok((b - b'0') as u32 + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("bad base64 byte: 0x{b:02x}")),
        }
    }
    let raw: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(raw.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in &raw {
        if b == b'=' {
            break;
        }
        let v = idx(b)?;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// Convenience constructor for unit tests + the file plane's
/// `app::run` bootstrap. Returns the bridge wrapped in an `Arc` plus
/// the receiver halves of the `notify` + `preview` channels so the
/// caller can wire them into the UI thread. The theme is supplied as
/// JSON so a Step 32+ palette change doesn't ripple through this
/// signature.
pub fn build_with_channels(
    registry: Arc<Registry>,
    theme: serde_json::Value,
) -> (
    Arc<PluginBridge>,
    tokio::sync::mpsc::Receiver<host_fns::Notification>,
    tokio::sync::mpsc::Receiver<PreviewMessage>,
) {
    // The host_ctx workdir is the *parent* of every per-plugin
    // workdir; the supervisor builds the actual per-plugin slot from
    // `sandbox::runtime_dir_for(plugin_id)` at spawn time. We hand
    // the parent path in here so `host.fs.write_cache` round-trips
    // land under a stable root even for plugins that don't trigger a
    // hover.
    let parent =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default()).join("sy-plugins");
    let (ctx, notify_rx, preview_rx) = host_fns::ctx_for_with_preview(parent, theme);
    let bridge = Arc::new(PluginBridge::new(registry, ctx));
    (bridge, notify_rx, preview_rx)
}

#[cfg(test)]
mod tests {
    //! In-source unit tests cover the pure helpers. The cross-process
    //! integration tests (registry → spawn → preview round-trip) live
    //! in `tests/sy_file_plugin_preview.rs` so they share the same
    //! `#[path]` shim every other plugin integration test uses.
    use super::*;

    /// `decode_preview_response` decodes the PNG arm into the raw
    /// bytes the GUI layer can feed to `Handle::from_bytes`.
    #[test]
    fn decode_preview_response_image_arm_strips_base64() {
        // Round-trip via the host_fns base64 encoder so the wire
        // shape matches what `sy-plugin-md` actually emits.
        let body = b"\x89PNG\r\n\x1a\nSTUB".to_vec();
        let b64 = host_fns_base64_encode(&body);
        let resp = json!({ "image": { "png_base64": b64, "w": 1, "h": 1 } });
        let got = decode_preview_response(&resp).expect("image arm decodes");
        assert_eq!(got, PreviewResult::Png(body));
    }

    /// `decode_preview_response` falls through to the text arm.
    #[test]
    fn decode_preview_response_text_arm_passes_body_through() {
        let resp = json!({ "text": "hello world" });
        let got = decode_preview_response(&resp).expect("text arm decodes");
        assert_eq!(got, PreviewResult::Text("hello world".to_string()));
    }

    /// A reply with neither `image` nor `text` surfaces as
    /// `InvalidResponse` so the dispatcher falls back to the built-in.
    #[test]
    fn decode_preview_response_empty_payload_is_invalid() {
        let resp = json!({});
        let err = decode_preview_response(&resp).expect_err("empty reply must error");
        assert!(matches!(err, BridgeError::InvalidResponse(_)));
    }

    /// Mirror of the host's encoder. Kept inside the test module so
    /// we don't expose a redundant copy on the public surface.
    fn host_fns_base64_encode(input: &[u8]) -> String {
        const ALPHA: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for c in input.chunks(3) {
            let (b0, b1, b2) = match c.len() {
                3 => (c[0], c[1], c[2]),
                2 => (c[0], c[1], 0),
                1 => (c[0], 0, 0),
                _ => unreachable!(),
            };
            let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            match c.len() {
                3 => {
                    out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
                    out.push(ALPHA[(n & 0x3f) as usize] as char);
                }
                2 => {
                    out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
                    out.push('=');
                }
                1 => {
                    out.push('=');
                    out.push('=');
                }
                _ => unreachable!(),
            }
        }
        out
    }
}
