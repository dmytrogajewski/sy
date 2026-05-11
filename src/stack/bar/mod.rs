//! Iced + iced_layershell bar daemon.
//!
//! NOTE: The iced GUI implementation is gated behind the `bar-iced` feature
//! to keep the default `cargo build` lean. Without the feature, `sy stack
//! bar` exits with a clear message — all CLI / MCP flows still work.
//!
//! When the feature is on, this module spins up an iced Application running
//! on a wlr-layer-shell surface anchored to the right edge, polls items.json
//! + cliphist on a tick, and serves IPC ops on a side thread.

use anyhow::Result;

#[cfg(not(feature = "bar-iced"))]
pub fn run() -> Result<()> {
    eprintln!(
        "sy stack bar requires the `bar-iced` feature.\n\
         Build with: cargo build --release --features bar-iced\n\
         (CLI / MCP / state functionality is available without this feature.)"
    );
    Ok(())
}

#[cfg(feature = "bar-iced")]
mod app;
#[cfg(feature = "bar-iced")]
mod theme;

#[cfg(feature = "bar-iced")]
pub fn run() -> Result<()> {
    app::run()
}
