//! sy-core: shared vocabulary for the sy workspace.
//!
//! This crate is the place where types and errors that cross
//! subsystem boundaries live (`WorkloadKind`, `Priority`,
//! `ErrorCode`, IPC envelope shapes — landing one step at a time
//! per specs/roadmaps/arch-workspace/ROADMAP.md). It is kept
//! deliberately small. Per matklad's "Fast Rust Builds" essay:
//! a hub crate's most important property is the set of crates it
//! does *not* transitively depend on. Resist the urge to add a
//! convenience helper here — put it in the crate that owns the
//! behaviour.

pub mod error;
pub mod metrics;
pub mod notify;
pub mod obs;
pub mod priority;
pub mod trace;
pub mod workload;

pub use error::ErrorCode;
pub use priority::Priority;
pub use trace::{SpanId, TraceId};
pub use workload::{
    SpeechSpan, WorkloadHealth, WorkloadInput, WorkloadKind, WorkloadOutput, WorkloadState,
};
