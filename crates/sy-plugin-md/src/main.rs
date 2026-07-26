//! `sy-plugin-md` — the canary first-party previewer (SPEC §3.3
//! item 18). Drives the [`sy_plugin_md::render`] pipeline behind
//! a `define_plugin!` PDK loop.
//!
//! On a `preview` request the plugin:
//!
//! 1. Resolves `req.path` to a markdown body (read from disk — the
//!    manifest declares `fs_read = ["arg.path"]`).
//! 2. Rasterises to PNG via [`sy_plugin_md::render::render_to_png`].
//! 3. Base64-encodes the PNG and returns it as a
//!    [`sy_plugin_pdk::PreviewResp::image`].
//!
//! No host fns are called today — the plugin reads the path itself.
//! The runtime is the same `define_plugin!` macro Step 11's PDK
//! canary uses, so a regression in either side breaks both.

use sy_plugin_md::render::{render_to_png, RenderOpts, CONTENT_WIDTH_PX};
use sy_plugin_pdk::prelude::*;

define_plugin! {
    id: "sy-plugin-md",
    api: "1",
    version: "0.1.0",
    capabilities: [
        Previewer { mime: "text/markdown" },
        Previewer { url: "*.md" },
        Previewer { url: "*.markdown" },
    ],
    handlers: {
        "preview": |req: PreviewReq| -> Result<PreviewResp> {
            let body = std::fs::read_to_string(&req.path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", req.path))?;
            let width = if req.max_width == 0 { CONTENT_WIDTH_PX } else { req.max_width };
            let max_h = if req.max_height == 0 { 4096 } else { req.max_height };
            let opts = RenderOpts {
                scroll_skip: req.scroll_skip,
                width_px: width,
                max_height_px: max_h,
            };
            let png = render_to_png(&body, &opts)
                .map_err(|e| anyhow::anyhow!("render: {e}"))?;
            let b64 = base64_encode(&png);
            // Compute the final dimensions from the PNG header so the
            // response carries the actual rendered size rather than
            // the budget. PNGs always have IHDR at byte 8..24 with
            // big-endian width@8 / height@12.
            let (w, h) = png_dimensions(&png).unwrap_or((width, max_h));
            Ok(PreviewResp::image(b64, w, h))
        },
    }
}

/// Inline base64 encoder — mirrors the host's
/// `src/plugin/host_fns.rs::base64_encode`. We keep it inline so the
/// plugin's dep tree stays at the four PDK pulls (anyhow + serde +
/// serde_json + tokio) plus the rendering trio.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let (b0, b1, b2) = match c.len() {
            3 => (c[0], c[1], c[2]),
            2 => (c[0], c[1], 0),
            1 => (c[0], 0, 0),
            _ => unreachable!("chunks(3) cannot yield 0"),
        };
        let n: u32 = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        match c.len() {
            3 => {
                out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
                out.push(ALPHABET[(n & 0x3f) as usize] as char);
            }
            2 => {
                out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
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

/// Parse the width / height fields out of a PNG IHDR chunk. The IHDR
/// chunk always starts at byte 8 (PNG signature) + 4 (chunk length) +
/// 4 (chunk type) = byte 16, with width@16..20 / height@20..24 big
/// endian. Returns `None` on truncation rather than panicking.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}
