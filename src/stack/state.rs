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
    let abs = fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))?;
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
    if let Ok(t) = thumbs_dir() {
        delete_thumbs_in(&t, id);
    }
}

/// Remove every `<id>.<size>.png` thumbnail in `cache_dir`. Split
/// out so tests can drive it without `XDG_CACHE_HOME` env tricks.
pub fn delete_thumbs_in(cache_dir: &Path, id: &str) {
    if let Ok(rd) = fs::read_dir(cache_dir) {
        for e in rd.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with(&format!("{id}.")) && name.ends_with(".png") {
                    let _ = fs::remove_file(e.path());
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
    indices.sort_by_key(|i| std::cmp::Reverse(i.0)); // newest first
    let evict: Vec<usize> = indices.into_iter().skip(max).map(|(_, idx)| idx).collect();
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

fn cache_base() -> Result<PathBuf> {
    if let Ok(x) = env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return Ok(PathBuf::from(x));
        }
    }
    let home = env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".cache"))
}

/// `$XDG_CACHE_HOME/sy/stack/thumbs/` — created on demand. Used by
/// the bar to materialise 20×20 and 256×256 PNGs for image slots so
/// every repaint reads pre-decoded pixels off disk instead of
/// re-decoding the original.
pub fn thumbs_dir() -> Result<PathBuf> {
    let d = cache_base()?.join("sy").join("stack").join("thumbs");
    fs::create_dir_all(&d).with_context(|| format!("mkdir {}", d.display()))?;
    Ok(d)
}

fn is_image_item(item: &Item) -> bool {
    if item.content_kind == "image" {
        return true;
    }
    if let Some(p) = &item.path {
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
        )
    } else {
        false
    }
}

/// Write an aspect-letterboxed `size×size` PNG of `src_img` to
/// `cache_dir/<file_stem>.<size>.png`. Skips the work if the
/// destination already exists, so callers can call freely from a
/// per-frame view path. Shared between
/// `state::thumbnail_path_at` (stack items) and
/// `clip::decode_to_thumb` (cliphist binary entries).
pub fn write_thumbnail_png(
    cache_dir: &Path,
    file_stem: &str,
    src_img: image::DynamicImage,
    size: u32,
) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir).with_context(|| format!("mkdir {}", cache_dir.display()))?;
    let dest = cache_dir.join(format!("{file_stem}.{size}.png"));
    if dest.exists() {
        return Ok(dest);
    }
    let scaled = src_img.resize(size, size, image::imageops::FilterType::Triangle);
    let scaled_rgba = scaled.to_rgba8();
    let (sw, sh) = (scaled_rgba.width(), scaled_rgba.height());
    let mut canvas = image::RgbaImage::from_pixel(size, size, image::Rgba([0, 0, 0, 0]));
    let ox = ((size - sw) / 2) as i64;
    let oy = ((size - sh) / 2) as i64;
    image::imageops::overlay(&mut canvas, &scaled_rgba, ox, oy);
    canvas
        .save(&dest)
        .with_context(|| format!("write thumbnail {}", dest.display()))?;
    Ok(dest)
}

/// Render a `size×size` aspect-letterboxed PNG thumbnail of the
/// image referenced by `item` into `cache_dir`. Returns `Ok(None)`
/// for non-image items so callers can dispatch on the result. The
/// destination file is `cache_dir/<id>.<size>.png`; re-uses the
/// cached copy if already present (mtime-stable across calls).
pub fn thumbnail_path_at(cache_dir: &Path, item: &Item, size: u32) -> Result<Option<PathBuf>> {
    if !is_image_item(item) {
        return Ok(None);
    }
    // Bar repaints once per second; an early stat avoids re-opening
    // the source image and re-encoding the PNG when the cache is
    // already warm. The detailed cache-check inside
    // `write_thumbnail_png` still covers concurrent first-decodes.
    let dest = cache_dir.join(format!("{}.{}.png", item.id, size));
    if dest.exists() {
        return Ok(Some(dest));
    }
    let source = match &item.path {
        Some(p) => p.clone(),
        None => blobs_dir()?.join(&item.id).join("payload"),
    };
    let src_img = image::open(&source).with_context(|| format!("decode {}", source.display()))?;
    Ok(Some(write_thumbnail_png(
        cache_dir, &item.id, src_img, size,
    )?))
}

/// Convenience wrapper that resolves the cache dir from
/// `thumbs_dir()` before delegating to `thumbnail_path_at`.
pub fn thumbnail_path(item: &Item, size: u32) -> Result<Option<PathBuf>> {
    let dir = thumbs_dir()?;
    thumbnail_path_at(&dir, item, size)
}

/// Render a hover-preview body for a text/code payload.
///
/// Returns the first `max_lines` lines of the UTF-8-lossy decode of
/// `bytes`, joined with `\n`. Inputs longer than `max_lines` get an
/// ellipsis line appended so the viewer knows the body was clipped.
/// Used by the bar's hover popup for text and code slots.
pub fn text_preview(bytes: &[u8], max_lines: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    let total = s.lines().count();
    let mut out: String = s
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    if total > max_lines {
        out.push_str("\n…");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_png(path: &Path, w: u32, h: u32) {
        let img: image::RgbImage =
            image::ImageBuffer::from_fn(w, h, |_, _| image::Rgb([200, 50, 50]));
        img.save(path).expect("write fixture png");
    }

    fn image_item(id: &str, path: PathBuf) -> Item {
        Item {
            id: id.into(),
            kind: Kind::User,
            path: Some(path),
            name: "fixture.png".into(),
            created_at: 0,
            content_kind: "file".into(),
            size: 0,
        }
    }

    #[test]
    fn thumbnail_path_creates_20x20_png() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("fixture.png");
        make_test_png(&src, 40, 40);
        let item = image_item("abc12345", src);
        let thumbs = tmp.path().join("thumbs");
        let dest = thumbnail_path_at(&thumbs, &item, 20)
            .expect("ok result")
            .expect("Some path");
        assert!(dest.exists(), "cached PNG should exist at {dest:?}");
        let decoded = image::open(&dest).expect("decode cached png");
        assert_eq!(decoded.width(), 20);
        assert_eq!(decoded.height(), 20);
    }

    #[test]
    fn thumbnail_path_is_cached_on_second_call() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("fixture.png");
        make_test_png(&src, 32, 32);
        let item = image_item("def67890", src);
        let thumbs = tmp.path().join("thumbs");
        let first = thumbnail_path_at(&thumbs, &item, 20).unwrap().unwrap();
        let first_mtime = fs::metadata(&first).unwrap().modified().unwrap();
        let second = thumbnail_path_at(&thumbs, &item, 20).unwrap().unwrap();
        assert_eq!(first, second);
        let second_mtime = fs::metadata(&second).unwrap().modified().unwrap();
        assert_eq!(
            first_mtime, second_mtime,
            "second call must hit the cache, not re-encode"
        );
    }

    #[test]
    fn delete_blobs_removes_thumbnails() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("fixture.png");
        make_test_png(&src, 32, 32);
        let item = image_item("delthumb", src);
        let thumbs = tmp.path().join("thumbs");
        thumbnail_path_at(&thumbs, &item, 20).unwrap();
        thumbnail_path_at(&thumbs, &item, 256).unwrap();
        assert!(thumbs.join("delthumb.20.png").exists());
        assert!(thumbs.join("delthumb.256.png").exists());
        delete_thumbs_in(&thumbs, "delthumb");
        assert!(!thumbs.join("delthumb.20.png").exists());
        assert!(!thumbs.join("delthumb.256.png").exists());
    }

    #[test]
    fn text_preview_truncates_to_24_lines() {
        const MAX: usize = 24;
        let body: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let preview = text_preview(body.as_bytes(), MAX);
        let lines: Vec<&str> = preview.lines().collect();
        assert_eq!(
            lines.len(),
            MAX + 1,
            "should be {MAX} body lines + one ellipsis marker"
        );
        assert_eq!(lines[0], "line 0");
        assert_eq!(lines[MAX - 1], "line 23");
        assert_eq!(lines[MAX], "…");
    }

    #[test]
    fn text_preview_returns_short_input_verbatim() {
        let preview = text_preview(b"only\ntwo\n", 24);
        let lines: Vec<&str> = preview.lines().collect();
        assert_eq!(lines, vec!["only", "two"]);
    }

    #[test]
    fn thumbnail_path_returns_none_for_text_item() {
        let tmp = tempfile::tempdir().unwrap();
        let item = Item {
            id: "txt00001".into(),
            kind: Kind::User,
            path: None,
            name: "snippet".into(),
            created_at: 0,
            content_kind: "text".into(),
            size: 0,
        };
        let thumbs = tmp.path().join("thumbs");
        let res = thumbnail_path_at(&thumbs, &item, 20).unwrap();
        assert!(res.is_none());
    }
}
