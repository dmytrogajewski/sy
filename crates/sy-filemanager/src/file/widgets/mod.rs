//! Custom widgets specific to the `sy file` UX. Roadmap Step 24
//! seeds the directory with [`icon`] (Nerd-Font glyph map) and
//! [`chip`] (selection / mode chips); Step 25 will land the
//! breadcrumb + per-op progress chip alongside.
//!
//! The directory exists so each widget is one file (vs. crowding
//! [`super::view`]); SPEC §3.1 names the same layout
//! (`widgets/{crumb,progress_row,chip,icon}.rs`).

pub mod chip;
pub mod crumb;
pub mod icon;
pub mod progress_row;
