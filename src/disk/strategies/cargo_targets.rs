use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use walkdir::WalkDir;

use super::{du_bytes, CleanStrategy, Outcome, Probe};

pub struct CargoTargets;

const STALE_DAYS: u64 = 14;
const MAX_DEPTH: usize = 4;

fn sources_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("sources")
}

fn find_stale_targets() -> Vec<PathBuf> {
    let root = sources_root();
    if !root.is_dir() {
        return Vec::new();
    }
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(STALE_DAYS * 86400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut out = Vec::new();
    let walker = WalkDir::new(&root)
        .max_depth(MAX_DEPTH)
        .into_iter()
        .filter_entry(|e| {
            let n = e.file_name();
            // Don't descend into target/, node_modules/, .git/.
            n != "target" && n != "node_modules" && n != ".git"
        });
    for entry in walker.flatten() {
        if !entry.file_type().is_dir() {
            continue;
        }
        let path = entry.path();
        let target = path.join("target");
        if !target.is_dir() || !path.join("Cargo.toml").is_file() {
            continue;
        }
        let stale = std::fs::metadata(&target)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|m| m < cutoff)
            .unwrap_or(false);
        if stale {
            out.push(target);
        }
    }
    out
}

impl CleanStrategy for CargoTargets {
    fn id(&self) -> &'static str {
        "cargo-targets"
    }
    fn label(&self) -> &'static str {
        "cargo targets"
    }
    fn description(&self) -> &'static str {
        "remove ~/sources/*/target dirs older than 14 days"
    }
    fn available(&self) -> bool {
        sources_root().is_dir()
    }
    fn probe(&self) -> Result<Probe> {
        let dirs = find_stale_targets();
        let total: u64 = dirs.iter().map(|p| du_bytes(p)).sum();
        let items: Vec<String> = dirs.iter().take(10).map(|p| p.display().to_string()).collect();
        Ok(Probe {
            reclaimable: total,
            items,
        })
    }
    fn apply(&self, _probe: &Probe) -> Result<Outcome> {
        let dirs = find_stale_targets();
        let mut reclaimed = 0u64;
        let mut log = Vec::new();
        for d in dirs {
            let before = du_bytes(&d);
            if std::fs::remove_dir_all(&d).is_ok() {
                reclaimed += before;
                log.push(format!("removed {}", d.display()));
            }
        }
        Ok(Outcome { reclaimed, log })
    }
}
