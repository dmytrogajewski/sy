//! Knowledge index status, stored under the knowledge state directory.

use std::{fs, io::Write, path::PathBuf};

use anyhow::{Context, Result};

#[cfg(test)]
pub use crate::aiplane::status::FRESH_SECS;
pub use crate::aiplane::status::{
    Status, default_queue_caps, default_queue_depths, is_fresh, notify_waybar,
};

fn status_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(base) if !base.is_empty() => PathBuf::from(base),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(".local/state"),
    };
    let dir = base.join("sy/knowledge");
    fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    Ok(dir.join("status.json"))
}

pub fn load() -> Result<Status> {
    let path = status_path()?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return crate::aiplane::status::load();
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

pub fn save(snapshot: &Status) -> Result<()> {
    let path = status_path()?;
    let temp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&temp).with_context(|| format!("create {}", temp.display()))?;
    file.write_all(&serde_json::to_vec_pretty(snapshot)?)?;
    file.sync_all()?;
    fs::rename(&temp, &path).with_context(|| format!("rename {}", path.display()))?;
    notify_waybar();
    Ok(())
}
