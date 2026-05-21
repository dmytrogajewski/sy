//! Shield — the 5-state DFA that enforces SPEC §4 "Concrete Shield
//! Constraint Set (HX 370)" safety constraints. Pure-function
//! `dfa::transition` (Step 17) classifies each [`Snapshot`] into one
//! of five states; Step 18 layers a `project` pass on top to pick the
//! arm that respects the resulting constraint envelope.
//!
//! The DFA is intentionally side-effect-free: no clock reads, no I/O,
//! no internal mutable state. Daemon-level concerns (the 30 s
//! "MEETING lock after VAD release" timer) live in the caller (Step
//! 19); the DFA returns `Meeting` while `prev == Meeting && call_active`
//! holds and lets the caller pin `prev` for the lock duration.
//!
//! [`Snapshot`]: crate::power::snapshot::Snapshot

pub mod dfa;
pub mod project;

pub use dfa::{transition, ShieldState};
pub use project::{project, ThrashTracker};
