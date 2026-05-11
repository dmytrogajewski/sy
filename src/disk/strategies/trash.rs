use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;

use super::{du_bytes, CleanStrategy, Outcome, Probe};

pub struct Trash;

fn trash_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/share/Trash")
}

impl CleanStrategy for Trash {
    fn id(&self) -> &'static str {
        "trash"
    }
    fn label(&self) -> &'static str {
        "trash"
    }
    fn description(&self) -> &'static str {
        "empty ~/.local/share/Trash"
    }
    fn available(&self) -> bool {
        trash_dir().is_dir()
    }
    fn probe(&self) -> Result<Probe> {
        let bytes = du_bytes(&trash_dir());
        Ok(Probe {
            reclaimable: bytes,
            items: vec![format!("{}", trash_dir().display())],
        })
    }
    fn apply(&self, _probe: &Probe) -> Result<Outcome> {
        let dir = trash_dir();
        let before = du_bytes(&dir);
        let used_gio = Command::new("gio")
            .args(["trash", "--empty"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !used_gio {
            for sub in ["files", "info"] {
                let p = dir.join(sub);
                if p.exists() {
                    let _ = std::fs::remove_dir_all(&p);
                    let _ = std::fs::create_dir_all(&p);
                }
            }
        }
        let after = du_bytes(&dir);
        Ok(Outcome {
            reclaimed: before.saturating_sub(after),
            log: vec!["emptied trash".to_string()],
        })
    }
}
