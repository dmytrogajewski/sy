//! Policy layer (Roadmap Step 18).
//!
//! Two pieces live here:
//!
//! - `rules::rules_baseline(state, snapshot, cfg) -> &str` — the
//!   hand-tuned `ShieldState -> arm-name` lookup table. This is the
//!   floor the Conservative LinUCB bandit (Steps 20–22) is not allowed
//!   to underperform; the table is deterministic given its inputs.
//!
//! The shield projection walker (`shield::project`) consumes this
//! baseline as its fallback when no candidate in the ranked list
//! passes the SPEC §4 shield constraints.

pub mod rules;

pub use rules::rules_baseline;
