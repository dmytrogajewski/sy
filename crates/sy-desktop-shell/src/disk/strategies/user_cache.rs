use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use super::{CleanStrategy, Outcome, Probe};

pub struct UserCache;

const STALE_DAYS: u64 = 30;

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache")
}

/// Subtrees of the cache root that are provisioning, not cache.
///
/// The NPU model blobs under `sy/aiplane` are written once by
/// `prep_npu_workload.py` and never re-touched, so a `+30d` mtime sweep
/// classified multi-GB ONNX weights as stale and deleted them, leaving
/// dangling symlinks and a silently dead knowledge plane
/// (specs/bugs/BUG-20260927-0119.md). They must never be on offer.
const PROTECTED: &[&str] = &["sy"];

/// Shared `find` selectors so the probe and the delete can never drift
/// apart — otherwise the menu advertises bytes the sweep then destroys.
fn find_args(root: &Path, tail: &[&str]) -> Vec<String> {
    let base = root.display().to_string();
    let mut args = vec![
        base.clone(),
        "-type".to_string(),
        "f".to_string(),
        "-mtime".to_string(),
        format!("+{STALE_DAYS}"),
    ];
    for keep in PROTECTED {
        args.extend([
            "-not".to_string(),
            "-path".to_string(),
            format!("{base}/{keep}/*"),
        ]);
    }
    args.extend(tail.iter().map(|arg| (*arg).to_string()));
    args
}

/// Returns (count, total_bytes) of prunable files older than STALE_DAYS.
/// Uses `find -printf` to avoid loading paths into memory.
fn probe_stale(root: &Path) -> (u64, u64) {
    if !root.is_dir() {
        return (0, 0);
    }
    let out = Command::new("find")
        .args(find_args(root, &["-printf", "%s\n"]))
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

/// Delete the prunable files under `root` and report what came back.
/// `-delete` avoids loading paths into rust memory.
fn prune_stale(root: &Path) -> Result<Outcome> {
    if !root.is_dir() {
        return Ok(Outcome::default());
    }
    let (_count_before, total_before) = probe_stale(root);
    let _ = Command::new("find")
        .args(find_args(root, &["-delete"]))
        .status();
    let (count_after, total_after) = probe_stale(root);
    Ok(Outcome {
        reclaimed: total_before.saturating_sub(total_after),
        log: vec![format!("{} stale files remaining", count_after)],
    })
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
        let (count, total) = probe_stale(&cache_dir());
        Ok(Probe {
            reclaimable: total,
            items: vec![format!("{count} stale files in ~/.cache")],
        })
    }
    fn apply(&self, _probe: &Probe) -> Result<Outcome> {
        prune_stale(&cache_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    /// Build `<root>/sy/aiplane/bge/model.onnx` (a provisioned NPU blob)
    /// and `<root>/mozilla/junk.sqlite` (generic stale cache), both 30+
    /// days old by mtime.
    fn aged_tree(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("sy-cache-sweep-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let model = root.join("sy/aiplane/bge/model.onnx");
        let junk = root.join("mozilla/junk.sqlite");
        fs::create_dir_all(model.parent().unwrap()).unwrap();
        fs::create_dir_all(junk.parent().unwrap()).unwrap();
        fs::write(&model, vec![b'x'; 4096]).unwrap();
        fs::write(&junk, vec![b'y'; 4096]).unwrap();
        for p in [&model, &junk] {
            let aged = Command::new("touch")
                .arg("-d")
                .arg("2020-01-01")
                .arg(p)
                .status()
                .expect("touch available");
            assert!(aged.success());
        }
        root
    }

    #[test]
    fn sweep_spares_provisioned_sy_artifacts_and_eats_generic_junk() {
        let root = aged_tree("prune");
        prune_stale(&root).unwrap();
        assert!(
            root.join("sy/aiplane/bge/model.onnx").exists(),
            "NPU model blobs are provisioning, not cache: the sweep must not amputate the plane"
        );
        assert!(
            !root.join("mozilla/junk.sqlite").exists(),
            "generic stale cache must still be reclaimed"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn probe_does_not_promise_protected_bytes_as_reclaimable() {
        let root = aged_tree("probe");
        assert_eq!(
            probe_stale(&root).1,
            4096,
            "only the unprotected junk counts"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn probe_and_delete_share_one_selector_set() {
        let root = Path::new("/home/u/.cache");
        let (mut probe, mut delete) = (
            find_args(root, &["-printf", "%s\n"]),
            find_args(root, &["-delete"]),
        );
        probe.truncate(probe.len() - 2);
        delete.truncate(delete.len() - 1);
        assert_eq!(probe, delete);
    }
}
