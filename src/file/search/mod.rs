//! Search surface for the `sy file` plane. Roadmap Step 25 lands the
//! in-pane `/` fuzzy filter ([`filename`]) backed by `nucleo`'s low-
//! level [`nucleo::Matcher`]; Step 30 bolts a sibling
//! [`knowledge`] for the `:k <query>` palette path.
//!
//! Both submodules are intentionally NOT `#[cfg(feature = "gui-iced")]`
//! gated — the matcher is headless-safe and the CLI/MCP surface (Step
//! 20+) reaches for it from the `--ipc search` op too. The
//! `knowledge` module is similarly headless; the IPC dial to
//! `sy-knowledge.service` does not require the GUI to be wired.

pub mod filename;
pub mod knowledge;
