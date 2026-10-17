//! Search surface for the `sy file` plane. Roadmap Step 25 lands the
//! in-pane `/` fuzzy filter ([`filename`]) backed by `nucleo`'s low-
//! level [`nucleo::Matcher`]; Step 30 bolts a sibling
//! [`knowledge`] for the `:k <query>` palette path.
//!
//! The fuzzy matcher serves the GUI and its headless tests; CLI/MCP
//! filename search uses its own substring filter. Knowledge search is
//! shared by GUI and IPC and remains available without the GUI feature.

#[cfg(any(feature = "gui-iced", test))]
pub mod filename;
pub mod knowledge;
