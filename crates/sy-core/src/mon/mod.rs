//! `sy mon` shared types.
//!
//! The dashboard plane (`specs/roadmaps/sy-mon/ROADMAP.md`) has four
//! consumers of the same data: the iced popup, the
//! `system.mon.snapshot` sy-ipc op, the MCP `system.mon.snapshot`
//! tool, and the `sy mon snapshot --json` CLI. They all reach for one
//! canonical [`snapshot::SystemSnapshot`] type — defined here in
//! sy-core so it sits next to the metrics catalogue (D-SCHEMA in
//! `specs/research/sy-mon/SPEC.md`).
//!
//! This module is intentionally type-only at this checkpoint: the
//! aggregator that *populates* the snapshot from sensors and Prom
//! scrapes lands in Step 11+ of the ROADMAP. Keeping the wire shape
//! decoupled from `crates/sy-core/src/sensors/*` lets the two evolve
//! independently — the aggregator does the projection.

pub mod ring;
pub mod snapshot;
