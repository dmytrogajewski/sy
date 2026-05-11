use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;

use super::{CleanStrategy, Outcome, Probe};

pub struct UserCache;

const STALE_DAYS: u64 = 30;

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache")
}

// Returns (count, total_bytes) of files older than STALE_DAYS in ~/.cache.
// Uses `find -printf` to avoid loading paths into memory.
fn probe_stale() -> (u64, u64) {
    let dir = cache_dir();
    if !dir.is_dir() {
        return (0, 0);
    }
    let out = Command::new("find")
        .arg(&dir)
        .args([
            "-type",
            "f",
            "-mtime",
            &format!("+{STALE_DAYS}"),
            "-printf",
            "%s\n",
        ])
        .output();
    let Ok(o) = out else {
        return (0, 0);
    };
    let mut count = 0u64;
    let mut total = 0u64;
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        if let Ok(n) = line.parse::<u64>() {
            total += n;
            count += 1;
        }
    }
    (count, total)
}

impl CleanStrategy for UserCache {
    fn id(&self) -> &'static str {
        "user-cache"
    }
    fn label(&self) -> &'static str {
        "~/.cache (>30d)"
    }
    fn description(&self) -> &'static str {
        "delete files in ~/.cache older than 30 days"
    }
    fn available(&self) -> bool {
        cache_dir().is_dir()
    }
    fn probe(&self) -> Result<Probe> {
        let (count, total) = probe_stale();
        Ok(Probe {
            reclaimable: total,
            items: vec![format!("{count} stale files in ~/.cache")],
        })
    }
    fn apply(&self, _probe: &Probe) -> Result<Outcome> {
        let dir = cache_dir();
        if !dir.is_dir() {
            return Ok(Outcome::default());
        }
        let (_count_before, total_before) = probe_stale();
        // -delete avoids loading paths into rust memory.
        let _ = Command::new("find")
            .arg(&dir)
            .args(["-type", "f", "-mtime", &format!("+{STALE_DAYS}"), "-delete"])
            .status();
        let (count_after, total_after) = probe_stale();
        Ok(Outcome {
            reclaimed: total_before.saturating_sub(total_after),
            log: vec![format!(
                "{} stale files remaining",
                count_after
            )],
        })
    }
}
