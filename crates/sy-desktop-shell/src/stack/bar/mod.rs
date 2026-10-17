//! Iced + iced_layershell bar daemon.
//!
//! NOTE: The iced GUI implementation is gated behind the `gui-iced` feature
//! (formerly `bar-iced`; the old name is preserved as an alias for one
//! release) to keep the default `cargo build` lean. Without the feature,
//! `sy stack bar` exits with a clear message — all CLI / MCP flows still
//! work.
//!
//! When the feature is on, this module spins up an iced Application running
//! on a wlr-layer-shell surface anchored to the right edge, polls items.json
//! + cliphist on a tick, and serves IPC ops on a side thread.

use anyhow::Result;

#[cfg(not(feature = "gui-iced"))]
pub fn run() -> Result<()> {
    eprintln!(
        "sy stack bar requires the `gui-iced` feature.\n\
         Build with: cargo build --release --features gui-iced\n\
         (CLI / MCP / state functionality is available without this feature.)"
    );
    Ok(())
}

#[cfg(feature = "gui-iced")]
mod app;
// `pub(crate)` so `src/mon/theme.rs` (Step 15) can re-export the
// `Palette` tokens through its four-slot projection. Visibility is
// crate-local; no external consumer.
#[cfg(feature = "gui-iced")]
pub(crate) mod theme;

#[cfg(feature = "gui-iced")]
pub fn run() -> Result<()> {
    app::run()
}
