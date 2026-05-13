//! On-disk index metadata at $XDG_STATE_HOME/sy/knowledge/.
//!
//! `index.json` tracks every indexed file: its mtime, content hash, and the
//! set of point ids it owns in Qdrant. On a re-index pass, we walk source
//! roots and compare hashes — files whose hash differs get their points
//! deleted + re-upserted; files no longer present get their points dropped.

use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    /// Map keyed by the absolute path of an indexed file (already
    /// expanded, no `~`).
    #[serde(default)]
    pub files: HashMap<String, FileEntry>,
    /// When the last incremental sync finished.
    #[serde(default)]
    pub last_sync_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// File mtime as seconds since epoch.
    pub mtime: u64,
    /// blake3 of the extracted text (not the raw bytes — text-equivalent
    /// changes are skipped, e.g. PDF re-export with same text).
    pub content_hash: String,
    /// Qdrant point ids this file owns. Stable so we can delete on update.
    pub point_ids: Vec<String>,
}

pub fn root_dir() -> Result<PathBuf> {
    let base = if let Ok(x) = env::var("XDG_STATE_HOME") {
        if x.is_empty() {
            default_state_root()?
        } else {
            PathBuf::from(x)
        }
    } else {
        default_state_root()?
    };
    let dir = base.join("sy").join("knowledge");
    fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    Ok(dir)
}

fn default_state_root() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local").join("state"))
}

pub fn data_dir() -> Result<PathBuf> {
    let base = if let Ok(x) = env::var("XDG_DATA_HOME") {
        if x.is_empty() {
            default_data_root()?
        } else {
            PathBuf::from(x)
        }
    } else {
        default_data_root()?
    };
    let dir = base.join("sy").join("knowledge");
    fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    Ok(dir)
}

fn default_data_root() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local").join("share"))
}

pub fn index_path() -> Result<PathBuf> {
    Ok(root_dir()?.join("index.json"))
}

pub fn qdrant_storage_dir() -> Result<PathBuf> {
    let d = data_dir()?.join("qdrant");
    fs::create_dir_all(&d).ok();
    Ok(d)
}

pub fn qdrant_log_path() -> Result<PathBuf> {
    Ok(root_dir()?.join("qdrant.log"))
}

pub fn load() -> Result<Index> {
    let p = index_path()?;
    if !p.exists() {
        return Ok(Index::default());
    }
    let s = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    if s.trim().is_empty() {
        return Ok(Index::default());
    }
    serde_json::from_str(&s).with_context(|| format!("parse {}", p.display()))
}

pub fn save(idx: &Index) -> Result<()> {
    let p = index_path()?;
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(idx)?;
    {
        let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &p).with_context(|| format!("rename {}", p.display()))?;
    Ok(())
}

/// File mtime in seconds since UNIX epoch (0 if unavailable).
pub fn mtime_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// blake3 of arbitrary bytes, lowercase hex.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
