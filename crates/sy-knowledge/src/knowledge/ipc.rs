//! Knowledge-plane IPC.
//!
//! As of the aiplane refactor the wire types + serve/request live in
//! `crate::aiplane::ipc`; this module re-exports them so the rest of
//! `knowledge::` continues to import `ipc::Op`, `ipc::send`, etc.
//! unchanged. Same socket, same JSON shape.

pub use crate::aiplane::ipc::*;
