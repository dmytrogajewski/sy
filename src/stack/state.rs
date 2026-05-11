//! On-disk state for sy stack.
//!
//! Layout:
//!   $XDG_STATE_HOME/sy/stack/
//!     items.json             ← Vec<Item>, app + user pools (clipboard NOT stored)
//!     blobs/<id>/payload     ← raw bytes for content items
//!     blobs/<id>/meta.json   ← optional metadata (mime, original name)
//!     links/<id>             ← stable temp paths for `sy stack link` of content items
//!
//! Writes are atomic via temp-file + rename.

use std::{
    env,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::Kind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub kind: Kind,
    /// File items reference an absolute path on disk; content items have None
    /// here and store payload under blobs/<id>/payload.
    pub path: Option<PathBuf>,
    /// Display name (basename for files, snippet head for content).
    pub name: String,
    /// Unix epoch seconds.
    pub created_at: u64,
    /// Sniffed content type — "text", "image", "binary", "file".
    pub content_kind: String,
    /// Byte size of the payload (file size or stored content length).
    pub size: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Items {
    #[serde(default)]
    pub items: Vec<Item>,
}

pub fn root_dir() -> Result<PathBuf> {
    let base = if let Ok(x) = env::var("XDG_STATE_HOME") {
        if !x.is_empty() {
            PathBuf::from(x)
        } else {
            default_state()?
        }
    } else {
        default_state()?
    };
    let dir = base.join("sy").join("stack");
    fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    Ok(dir)
}

fn default_state() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local").join("state"))
}

pub fn items_path() -> Result<PathBuf> {
    Ok(root_dir()?.join("items.json"))
}
pub fn blobs_dir() -> Result<PathBuf> {
    let d = root_dir()?.join("blobs");
    fs::create_dir_all(&d).ok();
    Ok(d)
}
pub fn links_dir() -> Result<PathBuf> {
    let d = root_dir()?.join("links");
    fs::create_dir_all(&d).ok();
    Ok(d)
}

pub fn load() -> Result<Items> {
    let p = items_path()?;
    if !p.exists() {
        return Ok(Items::default());
    }
    let s = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    if s.trim().is_empty() {
        return Ok(Items::default());
    }
    serde_json::from_str(&s).with_context(|| format!("parse {}", p.display()))
}

pub fn save(items: &Items) -> Result<()> {
    let p = items_path()?;
    let tmp = p.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(items)?;
    {
        let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &p).with_context(|| format!("rename to {}", p.display()))?;
    Ok(())
}

/// Generate a short item id (8-char hex). Time + pid mix is enough since
/// the id namespace is per-user and items rarely exceed dozens.
pub fn new_id() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mixed = (n ^ pid.wrapping_mul(0x9E3779B97F4A7C15)) as u64;
    format!("{:08x}", mixed & 0xffff_ffff)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Push a content payload (raw bytes) under a fresh id; returns the new Item.
pub fn push_content(kind: Kind, name: String, bytes: &[u8], content_kind: &str) -> Result<Item> {
    let id = new_id();
    let dir = blobs_dir()?.join(&id);
    fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let payload = dir.join("payload");
    let mut f = File::create(&payload).with_context(|| format!("create {}", payload.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(Item {
        id,
        kind,
        path: None,
        name,
        created_at: now_secs(),
        content_kind: content_kind.to_string(),
        size: bytes.len() as u64,
    })
}

/// Push a file-path reference under a fresh id; returns the new Item.
pub fn push_file(kind: Kind, name: String, path: &Path) -> Result<Item> {
    let abs = fs::canonicalize(path)
        .with_context(|| format!("canonicalize {}", path.display()))?;
    let meta = fs::metadata(&abs).with_context(|| format!("stat {}", abs.display()))?;
    let content_kind = sniff_kind(&abs);
    Ok(Item {
        id: new_id(),
        kind,
        path: Some(abs),
        name,
        created_at: now_secs(),
        content_kind,
        size: meta.len(),
    })
}

pub fn sniff_kind(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tiff" | "svg" => "image".into(),
        "txt" | "md" | "rs" | "toml" | "json" | "yaml" | "yml" | "kdl" | "css" | "js" | "ts"
        | "html" | "py" | "sh" | "go" | "c" | "h" | "cpp" | "hpp" => "text".into(),
        _ => "file".into(),
    }
}

/// Read the stored payload bytes for a content item.
pub fn read_payload(id: &str) -> Result<Vec<u8>> {
    let p = blobs_dir()?.join(id).join("payload");
    let mut f = File::open(&p).with_context(|| format!("open {}", p.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Resolve an item to a filesystem path. For content items, materialise the
/// payload under links/<id>[.ext] and return that — stable across calls.
pub fn link_path(item: &Item) -> Result<PathBuf> {
    if let Some(p) = &item.path {
        return Ok(p.clone());
    }
    let ext = match item.content_kind.as_str() {
        "text" => ".txt",
        "image" => "",
        _ => "",
    };
    let dir = links_dir()?;
    let name = format!("{}{}", item.id, ext);
    let dest = dir.join(&name);
    if !dest.exists() {
        let bytes = read_payload(&item.id)?;
        let mut f = File::create(&dest).with_context(|| format!("create {}", dest.display()))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    Ok(dest)
}

/// Locate an item by id. Returns NotFound (exit 3) if absent.
pub fn find<'a>(items: &'a Items, id: &str) -> Result<&'a Item> {
    items
        .items
        .iter()
        .find(|i| i.id == id)
        .ok_or_else(|| not_found(id))
}

pub fn not_found(id: &str) -> anyhow::Error {
    super::StackError {
        code: super::exit::NOT_FOUND,
        msg: format!("stack item not found: {id}"),
    }
    .into()
}

/// Remove blob+link directories for an item id (best effort).
pub fn delete_blobs(id: &str) {
    if let Ok(b) = blobs_dir() {
        let _ = fs::remove_dir_all(b.join(id));
    }
    if let Ok(l) = links_dir() {
        // Remove any link file that starts with id (with or without ext).
        if let Ok(rd) = fs::read_dir(&l) {
            for e in rd.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    if name == id || name.starts_with(&format!("{id}.")) {
                        let _ = fs::remove_file(e.path());
                    }
                }
            }
        }
    }
}

/// Cap a pool to `max` items, evicting oldest. Returns ids that were removed.
pub fn enforce_cap(items: &mut Items, kind: Kind, max: usize) -> Vec<String> {
    let mut indices: Vec<(u64, usize)> = items
        .items
        .iter()
        .enumerate()
        .filter(|(_, i)| i.kind == kind)
        .map(|(idx, i)| (i.created_at, idx))
        .collect();
    if indices.len() <= max {
        return Vec::new();
    }
    indices.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    let evict: Vec<usize> = indices
        .into_iter()
        .skip(max)
        .map(|(_, idx)| idx)
        .collect();
    let mut removed_ids = Vec::with_capacity(evict.len());
    // Remove highest indices first to keep earlier indices valid.
    let mut sorted = evict.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    for idx in sorted {
        let it = items.items.remove(idx);
        removed_ids.push(it.id.clone());
        delete_blobs(&it.id);
    }
    removed_ids
}

pub fn check_caps(items: &mut Items, max_app: usize, max_user: usize) -> Vec<String> {
    let mut evicted = enforce_cap(items, Kind::App, max_app);
    evicted.extend(enforce_cap(items, Kind::User, max_user));
    evicted
}
