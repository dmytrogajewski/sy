//! Pluggable cleaning strategies. The `CleanStrategy` trait is the
//! extension point — drop a new impl into this module's children and add it
//! to `registered()`. The `Ranker` trait orders probed strategies; the
//! built-in `ReclaimableRanker` sorts by bytes desc, and an ML-based ranker
//! can be wired in later without touching call sites.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

pub mod cargo_targets;
pub mod flatpak;
pub mod podman;
pub mod trash;
pub mod user_cache;

#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub reclaimable: u64,
    /// Short, human-readable preview of what would be removed (e.g. paths,
    /// counts). Surfaced as a hint in the fuzzel menu and recorded in the
    /// disk-history log.
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub reclaimed: u64,
    /// Lines describing what apply() actually did. Shown in the post-apply
    /// notification and recorded in the disk-history log.
    pub log: Vec<String>,
}

pub trait CleanStrategy: Send + Sync {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn available(&self) -> bool {
        true
    }
    fn probe(&self) -> Result<Probe>;
    fn apply(&self, probe: &Probe) -> Result<Outcome>;
}

pub trait Ranker: Send + Sync {
    fn rank<'a>(&self, probes: &'a [(String, Probe)]) -> Vec<&'a str>;
}

pub struct ReclaimableRanker;

impl Ranker for ReclaimableRanker {
    fn rank<'a>(&self, probes: &'a [(String, Probe)]) -> Vec<&'a str> {
        let mut idx: Vec<usize> = (0..probes.len()).collect();
        idx.sort_by_key(|&i| std::cmp::Reverse(probes[i].1.reclaimable));
        idx.into_iter().map(|i| probes[i].0.as_str()).collect()
    }
}

pub fn registered() -> Vec<Box<dyn CleanStrategy>> {
    vec![
        Box::new(trash::Trash),
        Box::new(cargo_targets::CargoTargets),
        Box::new(user_cache::UserCache),
        Box::new(podman::Podman),
        Box::new(flatpak::Flatpak),
    ]
}

pub(crate) fn du_bytes(p: &Path) -> u64 {
    if !p.exists() {
        return 0;
    }
    let out = Command::new("du").args(["-sb"]).arg(p).output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        Err(_) => 0,
    }
}
