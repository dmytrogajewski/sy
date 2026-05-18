//! sy-agt policy resolver — SPEC §4.4 Step 1.
//!
//! Read-only policy plane: parses the TOML profile under
//! `configs/policy/profiles/<name>.toml`, optionally overlays a
//! per-tool file under `configs/policy/tools/<tool>.toml`, and
//! answers `decide(tool, argv) -> Decision`. No sandbox spawn yet —
//! enforcement lands in Step 3 (Landlock + seccomp + scope). The
//! resolver also exposes a `fingerprint()` of the resolved policy
//! that later commits stamp on every audit-log record.

pub mod cli;
pub mod consent;
pub mod grant;
pub mod resolver;
pub mod schema;

pub use consent::{ConsentDecision, ConsentError, ConsentStore};
pub use resolver::{Decision, Resolver};
