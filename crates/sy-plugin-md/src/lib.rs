//! First-party Markdown previewer for `sy file` (SPEC §3.3 item 18).
//!
//! The library half exposes [`render::render_to_png`] so the crate's
//! integration tests can drive the rasteriser directly without
//! spawning the bin. The bin (`src/main.rs`) is a thin
//! `define_plugin!` wrapper that calls the same entry point.
//!
//! Pipeline: `pulldown_cmark::Parser` → in-memory `Block` list
//! (`render::layout`) → `cosmic-text::Buffer` layout per block →
//! `tiny-skia::Pixmap` rasterisation → `Pixmap::encode_png`. No
//! chrome, no keyring, no terminal image protocol; the test
//! `no_chrome_no_keyring` locks that contract.

pub mod ahash;
pub mod render;
