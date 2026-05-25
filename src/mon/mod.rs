//! `sy mon` — on-demand layer-shell health dashboard.
//!
//! Source: `specs/research/sy-mon/SPEC.md` (§3 SCOPE item 3, §4 "CLI /
//! MCP surface", §4 Non-functional Reliability). Roadmap step:
//! `specs/roadmaps/sy-mon/ROADMAP.md` Step 11 — daemon shell, clap
//! subcommand, tokio multi-thread runtime, 1 Hz host-sensor tick into
//! the ring buffer from Step 7.
//!
//! Step 11 ships the aggregator's scaffold only: the popup, the IPC
//! handlers, the plane scrape leg, and the doctor surface land in
//! Steps 12-22 of the same roadmap.

pub mod cli;
pub mod client;
pub mod collect;
pub mod doctor;
pub mod mcp;
// Step 22: optional waybar custom-module tile. Pure-function renderer
// over `Option<&SystemSnapshot>` so the classification logic is
// unit-testable without an IPC round-trip.
pub mod waybar;

// Step 15: iced Canvas widgets + theme. Gated on `bar-iced` because
// both modules pull in `iced::Color` / `iced::widget::canvas::Frame`
// and reference `crate::stack::bar::theme` (itself bar-iced-gated).
#[cfg(feature = "bar-iced")]
pub mod theme;
#[cfg(feature = "bar-iced")]
pub mod widgets;

// Step 16: popup app (iced + iced_layershell). Same gating as the
// widget tree above — depends on iced + iced_layershell and consumes
// `mon::theme` + `mon::widgets`.
#[cfg(feature = "bar-iced")]
pub mod app;
#[cfg(feature = "bar-iced")]
pub mod state;
#[cfg(feature = "bar-iced")]
pub mod view;

/// Domain error carrying a stable CLIG exit code. The `sy mon snapshot`
/// path raises this when the aggregator is unreachable so `main.rs`
/// can dispatch the CLAUDE.md "drift detected" exit (3) without the
/// generic `Result` path's default of 1. Mirrors the
/// `knowledge::KnowledgeError` / `stack::StackError` shape so callers
/// can downcast uniformly.
#[derive(Debug)]
pub struct MonError {
    pub code: i32,
    pub msg: String,
}

impl std::fmt::Display for MonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::error::Error for MonError {}
