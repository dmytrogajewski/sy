//! Filesystem ladder for `sy file`. Step 13 lands the submodule
//! shape; each leaf is filled in by a later roadmap step:
//!
//! * Step 15 — [`walk`] (async `statx` fast-path)
//! * Step 16-17 — [`copy`] (`copy_file_range` + `tokio-uring`)
//! * Step 18 — [`trash`] (freedesktop spec)
//! * Step 19 — [`watch`] (`notify-rs`) + [`mime`] (`tree_magic_mini`)
//!
//! Each submodule today carries only a doc-comment and a
//! `#[cfg(test)] mod tests` block asserting the module compiles +
//! is reachable; the real implementations land in their respective
//! steps without breaking the `pub mod` declarations here.
pub mod copy;
pub mod mime;
// Step 32 (SPEC §3.3 item 14) — `/proc/self/mountinfo` parser +
// optional udisks2 D-Bus probe. The 3-pane sidebar + `:m` palette
// both read `Mount` via this module.
pub mod mounts;
pub mod trash;
pub mod walk;
pub mod watch;
