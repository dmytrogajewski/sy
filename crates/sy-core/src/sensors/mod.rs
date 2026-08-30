//! Host-sensor parsers and thin I/O wrappers.
//!
//! Per SPEC §3 SCOPE item 2, every metric the `sy mon` aggregator and
//! the waybar tiles consume is read here exactly once. Each submodule
//! exposes:
//!
//! 1. A typed `*Sample` struct that derives `Serialize` / `Deserialize`
//!    so it can feed `crates/sy-core/src/mon/snapshot.rs::SystemSnapshot`
//!    in Step 6 without an intermediate copy.
//! 2. A pure `parse_*(&str) -> ...` function. No filesystem access,
//!    no clock, no syscalls — testable from a fixture string.
//! 3. A `sample()` wrapper that owns the procfs / sysfs read and
//!    delegates the bytes-to-struct conversion to the pure parser.
//!
//! Later sensors (`net`, `disk`, `bat`, `gpu_*`, `npu_xdna`, `power`,
//! `supervisor`) land in subsequent ROADMAP steps and follow the same
//! shape.

pub mod bat;
pub mod cpu;
pub mod disk;
pub mod gpu_amd;
pub mod gpu_nvidia;
pub mod load;
pub mod mem;
pub mod net;
pub mod npu_xdna;
pub mod supervisor;
