//! DGX Spark workstation client and feature-gated appliance bootstrap.

pub mod cli;
pub mod client;
#[cfg(feature = "spark-agent")]
pub mod engine;
pub mod install;
pub mod launch;
#[cfg(feature = "spark-agent")]
pub mod reconcile;
pub mod wire;

#[cfg(feature = "spark-agent")]
pub mod agent;
#[cfg(all(feature = "spark-agent", test))]
pub mod bench;
#[cfg(feature = "spark-agent")]
pub mod executor;
#[cfg(feature = "spark-agent")]
pub mod gateway;
#[cfg(feature = "spark-agent")]
pub mod model;
#[cfg(feature = "spark-agent")]
pub mod model_catalog;
#[cfg(all(feature = "spark-agent", test))]
pub mod recipe;
pub mod resources;
#[cfg(feature = "spark-agent")]
pub mod state;
#[cfg(feature = "spark-agent")]
pub mod upstream;

pub const EXIT_INTERNAL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_REJECTED: i32 = 3;
pub const EXIT_UNREACHABLE: i32 = 4;
pub const EXIT_OPERATION_FAILED: i32 = 5;
#[cfg(feature = "spark-agent")]
pub const MAX_ENGINE_STARTUP_DEADLINE_SECONDS: u64 = 1_800;
