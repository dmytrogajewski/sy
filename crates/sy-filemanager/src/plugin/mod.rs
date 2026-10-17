//! `sy file` plugin runtime — binaries-over-stdio plugin host.
//!
//! Plugins are plain binaries that speak JSON-RPC 2.0 over framed
//! stdio (LSP-style). Each plugin ships a [`manifest::Manifest`]
//! (TOML) describing its identity, capabilities, host-fn needs, and
//! resource limits. See [plugin SPEC §1
//! Summary](../../specs/research/sy-file-manager-plugins/SPEC.md#1-summary)
//! and the per-step roadmap under
//! `specs/roadmaps/sy-file-manager/ROADMAP.md`.
//!
//! This module is the staging ground for that runtime; later roadmap
//! steps add `sandbox`, `proc`, `capability`, `host_fns`,
//! `registry`, `cli`, and `install` submodules in order.
pub mod capability;
pub mod cli;
pub mod host_fns;
pub mod install;
pub mod manifest;
pub mod proc;
pub mod registry;
pub mod rpc;
pub mod sandbox;
pub mod transport;
