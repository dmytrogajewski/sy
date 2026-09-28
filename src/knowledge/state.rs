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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    /// Map keyed by the absolute path of an indexed file (already
    /// expanded, no `~`).
    #[serde(default)]
    pub files: HashMap<String, FileEntry>,
    /// When the last incremental sync finished.
    #[serde(default)]
    pub last_sync_unix: u64,
    /// Chunk/embed contract this index was written under. Compare against
    /// [`CHUNK_SCHEMA_VERSION`]; see it for why this exists.
    #[serde(default = "Index::legacy_chunk_schema_version")]
    pub chunk_schema_version: u32,
}

impl Default for Index {
    /// A fresh index is current by definition — that is what makes
    /// `sy knowledge sync` (which starts from `Index::default()`) clear any
    /// recorded schema drift.
    fn default() -> Self {
        Self {
            files: HashMap::new(),
            last_sync_unix: 0,
            chunk_schema_version: CHUNK_SCHEMA_VERSION,
        }
    }
}

/// Chunk/embed contract version. Bump it whenever the *shape* of an index
/// entry changes — `chunk::encode`'s window or [`crate::knowledge::chunk::MAX_CHUNK_CHARS`],
/// the overlap, the sparse leg, the dense payload schema, the embedder model.
///
/// Without this, the incremental index is content-hash-only: it compares the
/// *file*'s extracted text, so a chunker change silently applies to new files
/// only and the collection keeps serving chunks the current chunker would
/// never produce — indexed-green and unsearchable, the exact failure
/// `BUG-20260927-1215.md` documents. The bump turns that into a loud,
/// deliberate rebuild (see [`resync_required`]) instead of a stale corpus.
///
/// `1` is the baseline: whitespace-token + [`crate::knowledge::chunk::MAX_CHUNK_CHARS`]
/// chunking with the char-cap fix landed. Indexes written before this field
/// existed are read as `1` (see [`Index::legacy_chunk_schema_version`]) so
/// introducing the mechanism does not itself force a multi-hour re-embed.
pub const CHUNK_SCHEMA_VERSION: u32 = 1;

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

impl Index {
    /// Value assumed for an `index.json` written before
    /// [`CHUNK_SCHEMA_VERSION`] existed — the contract in force at the time,
    /// so upgrading `sy` never rewrites a healthy index by accident.
    fn legacy_chunk_schema_version() -> u32 {
        1
    }
}

/// Why an incremental pass must not run, or `None` when the on-disk index
/// matches the current chunk/embed contract.
///
/// A mismatch is *not* repaired by forgetting the file entries: their Qdrant
/// points would stay searchable with no owner to delete them. Only a full
/// resync (which drops the collection first) may clear it, so the answer is
/// an actionable error naming the command.
pub fn resync_required(idx: &Index) -> Option<String> {
    if idx.chunk_schema_version == CHUNK_SCHEMA_VERSION {
        return None;
    }
    Some(format!(
        "index was built under chunk-schema v{}; this sy speaks v{} — run          `sy knowledge sync --yes` to drop and re-embed the collection          (an incremental pass would keep serving pre-v{} chunks)",
        idx.chunk_schema_version, CHUNK_SCHEMA_VERSION, CHUNK_SCHEMA_VERSION
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the constant: an index under another contract must
    /// not be updated incrementally.
    #[test]
    fn stale_chunk_schema_forces_a_full_resync() {
        let mut idx = Index::default();
        assert_eq!(
            idx.chunk_schema_version, CHUNK_SCHEMA_VERSION,
            "a fresh index carries the current contract"
        );
        assert!(
            resync_required(&idx).is_none(),
            "current schema must index incrementally"
        );

        // Simulate the next bump: an index written under the previous version.
        idx.chunk_schema_version = CHUNK_SCHEMA_VERSION - 1;
        let why = resync_required(&idx).expect("older schema must be refused");
        assert!(
            why.contains("sy knowledge sync --yes") && why.contains("chunk-schema v0"),
            "the refusal must name the command and both versions, got {why}"
        );
    }

    /// Upgrading `sy` must not silently invalidate a healthy corpus: an
    /// `index.json` predating the field is read as the baseline contract.
    #[test]
    fn an_index_json_without_the_field_reads_as_the_baseline() -> Result<()> {
        let legacy = r#"{"files":{},"last_sync_unix":42}"#;
        let idx: Index = serde_json::from_str(legacy).expect("legacy index.json parses");
        assert_eq!(idx.last_sync_unix, 42);
        assert_eq!(idx.chunk_schema_version, 1, "pre-field indexes are v1");
        assert!(
            resync_required(&idx).is_none(),
            "reading an old index must not itself demand a re-embed"
        );
        Ok(())
    }

    /// The version survives a save/load round trip, so drift is detected by
    /// the daemon on the next pass rather than only in memory.
    #[test]
    fn chunk_schema_version_round_trips_through_index_json() {
        let idx = Index {
            chunk_schema_version: CHUNK_SCHEMA_VERSION + 1,
            ..Index::default()
        };
        let body = serde_json::to_string(&idx).expect("serialise");
        let back: Index = serde_json::from_str(&body).expect("parse back");
        assert_eq!(back.chunk_schema_version, CHUNK_SCHEMA_VERSION + 1);
        assert!(resync_required(&back).is_some());
    }
}
