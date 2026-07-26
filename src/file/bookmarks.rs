//! `sy file` bookmarks + recent-dirs plane. Roadmap Step 31
//! ([SPEC §3.3 item 15][spec]) — the user pins their working
//! directories with `b<key>` (the keymap reducer turns the chord into
//! a [`Bookmark`] insert keyed by the second character), and every
//! `file.open` IPC op (Step 20) stamps a freedesktop
//! `recently-used.xbel` entry so other DEs (Nautilus, Dolphin, the
//! GTK file-chooser) see the same recent-dirs list under
//! `$XDG_DATA_HOME/recently-used.xbel`.
//!
//! Pinned bookmarks live in a hand-rolled TOML file under
//! `$XDG_STATE_HOME/sy/file/bookmarks.toml`; the XBEL recent-dirs log
//! lives in `$XDG_DATA_HOME/recently-used.xbel` per the freedesktop
//! [Desktop Bookmarks Specification][xbel]. Both files are written
//! atomically (tmp + fsync + rename) so a daemon SIGKILL mid-write
//! leaves the prior copy intact — the journey **J1** next-day beat
//! depends on the pin surviving across restarts.
//!
//! [spec]: ../../../specs/research/sy-file-manager/SPEC.md
//! [xbel]: https://www.freedesktop.org/wiki/Specifications/desktop-bookmark-spec/

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use serde::{Deserialize, Serialize};

/// Filename inside `state_dir` that carries the pinned-bookmarks
/// TOML document. Kept as a constant so the e2e can reach in by
/// path without re-stringifying.
pub const BOOKMARKS_TOML: &str = "bookmarks.toml";

/// Filename inside `xdg_data_dir` (typically `$XDG_DATA_HOME`) for the
/// freedesktop recently-used log.
pub const RECENTLY_USED_XBEL: &str = "recently-used.xbel";

/// Most-recent N entries `read_recent` returns. The freedesktop spec
/// doesn't bound the file length; common consumers cap their UI at
/// 50–100 entries. We cap at 50 to match the GTK file-chooser default.
pub const RECENT_LIMIT: usize = 50;

/// A single pinned bookmark — either reached via a `b<key>` chord
/// (the TOML half) or surfaced from the XBEL recent-dirs log via
/// [`Bookmarks::read_recent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    /// Single-char hotkey the user pressed after `b` to pin / jump.
    /// XBEL entries (no hotkey) use `'_'` as the sentinel so the
    /// type stays uniform.
    pub key: char,
    /// Absolute path of the bookmarked directory (or file, for XBEL
    /// recent-dirs surfaced by `read_recent`).
    pub path: PathBuf,
    /// When the bookmark was added. Surfaces in the XBEL `added=` /
    /// `modified=` / `visited=` attributes per the freedesktop spec.
    pub added_at: SystemTime,
    /// Optional human-readable label. `None` for fresh pins; the XBEL
    /// `<title>` element falls back to the directory basename.
    pub title: Option<String>,
}

/// In-memory bookmarks registry. Owns both the pinned-bookmark map
/// (TOML-backed) and the XBEL recent-dirs file path. The journey-J1
/// next-day beat reads `state_dir`'s TOML at boot via [`load`]; the
/// `file.open` IPC op (Step 20) calls [`Bookmarks::touch_recent`] to
/// keep the XBEL log fresh.
#[derive(Debug, Clone)]
pub struct Bookmarks {
    /// Ordered by char key so iteration is deterministic — the
    /// view-side palette (`'b' → list`) reads the iteration order.
    pub items: BTreeMap<char, Bookmark>,
    /// `$XDG_STATE_HOME/sy/file/` (or test tempdir). [`Bookmarks::save`]
    /// writes `bookmarks.toml` here.
    pub state_dir: PathBuf,
    /// `$XDG_DATA_HOME/` (or test tempdir). [`Bookmarks::touch_recent`]
    /// writes `recently-used.xbel` here.
    pub xbel_dir: PathBuf,
}

/// On-disk wire shape for the TOML document. Serde-derived so the
/// format is stable across releases — bookmark fields can be added
/// later without breaking older readers (extra keys ride through as
/// `#[serde(default)]`-ish ignores).
#[derive(Debug, Default, Serialize, Deserialize)]
struct BookmarksToml {
    #[serde(default)]
    items: Vec<Bookmark>,
}

/// Read the TOML document at `state_dir/bookmarks.toml` (if any), tolerate
/// corruption with a `tracing::warn!`, and return the in-memory
/// [`Bookmarks`] registry. The XBEL log lives under `xbel_dir` — only
/// the directory is captured here; the file is touched lazily on the
/// first [`Bookmarks::touch_recent`] call.
pub fn load(state_dir: &Path, xbel_dir: &Path) -> Result<Bookmarks> {
    let toml_path = state_dir.join(BOOKMARKS_TOML);
    let items = match std::fs::read(&toml_path) {
        Ok(bytes) => match std::str::from_utf8(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|s| toml::from_str::<BookmarksToml>(s).map_err(|e| e.to_string()))
        {
            Ok(doc) => doc
                .items
                .into_iter()
                .map(|b| (b.key, b))
                .collect::<BTreeMap<char, Bookmark>>(),
            Err(e) => {
                tracing::warn!(
                    target = "sy::file::bookmarks",
                    path = %toml_path.display(),
                    error = %e,
                    "bookmarks.toml is corrupt; starting fresh"
                );
                BTreeMap::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", toml_path.display()));
        }
    };
    Ok(Bookmarks {
        items,
        state_dir: state_dir.to_path_buf(),
        xbel_dir: xbel_dir.to_path_buf(),
    })
}

impl Bookmarks {
    /// Pin a directory under `key`. Overwrites any prior pin under the
    /// same key (the yazi convention — `b<k>` is idempotent). Persists
    /// the change to disk so a subsequent [`load`] sees the same map.
    pub fn pin(&mut self, key: char, path: PathBuf, title: Option<String>) -> Result<()> {
        self.items.insert(
            key,
            Bookmark {
                key,
                path,
                added_at: SystemTime::now(),
                title,
            },
        );
        self.save()
    }

    /// Remove the pin under `key` (no-op if absent). Persists.
    pub fn unpin(&mut self, key: char) -> Result<()> {
        self.items.remove(&key);
        self.save()
    }

    /// Read-only lookup. Returns the pinned path the `b<key>` chord
    /// should warp to, or `None` if `key` is unbound.
    pub fn jump(&self, key: char) -> Option<&Path> {
        self.items.get(&key).map(|b| b.path.as_path())
    }

    /// Persist the in-memory map to `state_dir/bookmarks.toml` via the
    /// tmp + rename atomic-write dance. Public so a future MCP op can
    /// drive a batch update without round-tripping through `pin` /
    /// `unpin`.
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("mkdir -p {}", self.state_dir.display()))?;
        let final_path = self.state_dir.join(BOOKMARKS_TOML);
        let tmp_path = self.state_dir.join(format!("{BOOKMARKS_TOML}.tmp"));
        let doc = BookmarksToml {
            items: self.items.values().cloned().collect(),
        };
        let body = toml::to_string_pretty(&doc).context("serialising bookmarks.toml")?;
        {
            let mut f = std::fs::File::create(&tmp_path)
                .with_context(|| format!("create {}", tmp_path.display()))?;
            f.write_all(body.as_bytes())
                .with_context(|| format!("write {}", tmp_path.display()))?;
            f.sync_all()
                .with_context(|| format!("fsync {}", tmp_path.display()))?;
        }
        std::fs::rename(&tmp_path, &final_path).with_context(|| {
            format!("rename {} -> {}", tmp_path.display(), final_path.display())
        })?;
        Ok(())
    }

    /// Append a `<bookmark>` entry to `recently-used.xbel` for `path`.
    /// Called by the daemon's `file.open` IPC handler so other DEs see
    /// the same recent-dirs list. Idempotent — repeated calls against
    /// the same `path` update the entry's `modified=` / `visited=`
    /// stamps in place rather than duplicating.
    pub fn touch_recent(&mut self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(&self.xbel_dir)
            .with_context(|| format!("mkdir -p {}", self.xbel_dir.display()))?;
        let xbel_path = self.xbel_dir.join(RECENTLY_USED_XBEL);
        let now = SystemTime::now();
        let mut existing = read_recent_from(&xbel_path).unwrap_or_default();
        // De-dupe by path: if `path` is already in the log, drop the
        // prior entry so the new (most-recent) `added_at` lands at the
        // front per the spec's "most-recent first" ordering.
        existing.retain(|b| b.path != path);
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty());
        existing.insert(
            0,
            Bookmark {
                key: '_',
                path: path.to_path_buf(),
                added_at: now,
                title,
            },
        );
        existing.truncate(RECENT_LIMIT);
        write_xbel(&xbel_path, &existing)?;
        Ok(())
    }

    /// Parse the XBEL recent-dirs log into a flat [`Bookmark`] list
    /// (most-recent first). Surfaces to the view layer's recent-dirs
    /// palette and to the `bookmarks::tests::xbel_written_on_open`
    /// DoD via a quick-xml round-trip parse.
    pub fn read_recent(&self) -> Result<Vec<Bookmark>> {
        read_recent_from(&self.xbel_dir.join(RECENTLY_USED_XBEL))
    }
}

/// Pure helper: parse an XBEL file at `path` into a Bookmark list.
/// Returns an empty list if the file doesn't exist; bubbles up on
/// genuine I/O failure. Used by both [`Bookmarks::read_recent`] and
/// [`Bookmarks::touch_recent`]'s rewrite-in-place path.
fn read_recent_from(path: &Path) -> Result<Vec<Bookmark>> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut reader = Reader::from_str(&body);
    reader.config_mut().trim_text(true);
    let mut out: Vec<Bookmark> = Vec::new();
    let mut current: Option<(PathBuf, SystemTime, Option<String>)> = None;
    let mut in_title = false;
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .with_context(|| format!("parsing {}", path.display()))?
        {
            Event::Start(ref e) | Event::Empty(ref e) if e.name().as_ref() == b"bookmark" => {
                let mut href: Option<String> = None;
                let mut added: Option<String> = None;
                for attr in e.attributes().flatten() {
                    let val = attr.unescape_value().unwrap_or_default().into_owned();
                    match attr.key.as_ref() {
                        b"href" => href = Some(val),
                        b"added" => added = Some(val),
                        _ => {}
                    }
                }
                if let Some(h) = href {
                    let p = href_to_path(&h);
                    let when = added
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                        .map(|dt| {
                            std::time::UNIX_EPOCH
                                + std::time::Duration::from_secs(dt.timestamp().max(0) as u64)
                        })
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    current = Some((p, when, None));
                }
                let is_empty = matches!(
                    reader.read_event_into(&mut Vec::new()).ok(),
                    Some(Event::End(_))
                );
                if is_empty {
                    if let Some((p, t, title)) = current.take() {
                        out.push(Bookmark {
                            key: '_',
                            path: p,
                            added_at: t,
                            title,
                        });
                    }
                }
            }
            Event::Start(ref e) if e.name().as_ref() == b"title" => {
                in_title = true;
            }
            Event::Text(ref t) if in_title => {
                let s = t.unescape().unwrap_or_default().into_owned();
                if let Some(ref mut cur) = current {
                    cur.2 = Some(s);
                }
            }
            Event::End(ref e) if e.name().as_ref() == b"title" => {
                in_title = false;
            }
            Event::End(ref e) if e.name().as_ref() == b"bookmark" => {
                if let Some((p, t, title)) = current.take() {
                    out.push(Bookmark {
                        key: '_',
                        path: p,
                        added_at: t,
                        title,
                    });
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Atomic write of the XBEL document. Same tmp + fsync + rename dance
/// as `save()`. Public-fn-internal so `touch_recent`'s in-place rewrite
/// stays atomic.
fn write_xbel(path: &Path, items: &[Bookmark]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("no parent for {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
    let tmp_path = path.with_extension("xbel.tmp");
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("xbel xml decl")?;
    let mut xbel = BytesStart::new("xbel");
    xbel.push_attribute(("version", "1.0"));
    writer
        .write_event(Event::Start(xbel.clone()))
        .context("xbel open")?;
    for b in items {
        let href = path_to_href(&b.path);
        let ts = format_rfc3339(b.added_at);
        let mut elem = BytesStart::new("bookmark");
        elem.push_attribute(("href", href.as_str()));
        elem.push_attribute(("added", ts.as_str()));
        elem.push_attribute(("modified", ts.as_str()));
        elem.push_attribute(("visited", ts.as_str()));
        writer
            .write_event(Event::Start(elem.clone()))
            .context("bookmark open")?;
        if let Some(ref t) = b.title {
            writer
                .write_event(Event::Start(BytesStart::new("title")))
                .context("title open")?;
            writer
                .write_event(Event::Text(BytesText::new(t)))
                .context("title text")?;
            writer
                .write_event(Event::End(BytesEnd::new("title")))
                .context("title close")?;
        }
        writer
            .write_event(Event::End(BytesEnd::new("bookmark")))
            .context("bookmark close")?;
    }
    writer
        .write_event(Event::End(BytesEnd::new("xbel")))
        .context("xbel close")?;
    let body = writer.into_inner();
    {
        let mut f = std::fs::File::create(&tmp_path)
            .with_context(|| format!("create {}", tmp_path.display()))?;
        f.write_all(&body)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// `file:///abs/path` formatter per the freedesktop XBEL bookmark
/// `href=` attribute. Percent-escaping is delegated to a small inline
/// table covering the characters the spec calls out (space, `#`, `?`,
/// `%`); the rest of the path rides through verbatim.
fn path_to_href(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::from("file://");
    for ch in s.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '%' => out.push_str("%25"),
            other => out.push(other),
        }
    }
    out
}

/// Reverse of [`path_to_href`]. Decodes the percent-escapes we emit;
/// other `%xx` sequences ride through verbatim (XBEL files from other
/// DEs may stamp more elaborate escapes — we don't round-trip those
/// today, only ours).
fn href_to_path(href: &str) -> PathBuf {
    let trimmed = href.strip_prefix("file://").unwrap_or(href);
    let mut out = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2) {
                if let Ok(byte) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                    out.push(byte as char);
                    continue;
                }
            }
            out.push('%');
        } else {
            out.push(c);
        }
    }
    PathBuf::from(out)
}

/// RFC3339 stamp for the XBEL `added=` / `modified=` / `visited=`
/// attributes. Format: `YYYY-MM-DDThh:mm:ssZ`.
fn format_rfc3339(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DoD bullet 1: pin two keys, drop the registry, reload from disk,
    /// assert both keys round-trip. The `'a' → path1, 'b' → path2`
    /// shape mirrors the journey-J1 next-day beat: the user pinned a
    /// directory yesterday, restarted today, and `'a'` still warps to
    /// the same path.
    #[test]
    fn pin_then_jump_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let xbel = tmp.path().join("xbel");
        let path1 = tmp.path().join("p1");
        let path2 = tmp.path().join("p2");
        let mut bm = load(&state, &xbel).expect("load empty");
        bm.pin('a', path1.clone(), None).expect("pin a");
        bm.pin('b', path2.clone(), None).expect("pin b");
        assert_eq!(bm.jump('a'), Some(path1.as_path()));
        drop(bm);
        let bm2 = load(&state, &xbel).expect("reload");
        assert_eq!(bm2.jump('a'), Some(path1.as_path()));
        assert_eq!(bm2.jump('b'), Some(path2.as_path()));
    }

    /// DoD bullet 2 — `touch_recent` writes a `<bookmark>` element to
    /// `recently-used.xbel` for every distinct path. Three calls →
    /// three `<bookmark>` entries on the round-trip parse (verifies the
    /// freedesktop schema spirit via the quick-xml reader).
    #[test]
    fn xbel_written_on_open() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let xbel = tmp.path().join("xbel");
        let mut bm = load(&state, &xbel).expect("load empty");
        let p1 = tmp.path().join("dir1");
        let p2 = tmp.path().join("dir2");
        let p3 = tmp.path().join("dir3");
        bm.touch_recent(&p1).expect("touch p1");
        bm.touch_recent(&p2).expect("touch p2");
        bm.touch_recent(&p3).expect("touch p3");
        let recent = bm.read_recent().expect("read recent");
        assert_eq!(recent.len(), 3, "three distinct paths → three entries");
        let hrefs: Vec<_> = recent.iter().map(|b| b.path.clone()).collect();
        assert!(hrefs.contains(&p1));
        assert!(hrefs.contains(&p2));
        assert!(hrefs.contains(&p3));
        // The on-disk doc must declare itself as XBEL 1.0.
        let body = std::fs::read_to_string(xbel.join(RECENTLY_USED_XBEL)).expect("xbel body");
        assert!(body.contains("<?xml version=\"1.0\""));
        assert!(body.contains("<xbel version=\"1.0\""));
        assert!(body.matches("<bookmark").count() == 3);
    }

    /// DoD bullet 3 — a corrupt `bookmarks.toml` doesn't kill `load`;
    /// the function emits a `tracing::warn!` and returns an empty
    /// registry so the daemon keeps running.
    #[test]
    fn toml_survives_corruption_with_warn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let xbel = tmp.path().join("xbel");
        std::fs::create_dir_all(&state).expect("mkdir state");
        std::fs::write(state.join(BOOKMARKS_TOML), b"\xff this is not toml \xff =")
            .expect("write garbage");
        let bm = load(&state, &xbel).expect("load tolerates corruption");
        assert!(bm.items.is_empty(), "corruption → empty registry");
    }

    /// Coverage for the dedup path: touching the same dir twice keeps
    /// one entry, not two. Surfaces the journey-J1 invariant that the
    /// XBEL log is "set of recent dirs" not "log of every open".
    #[test]
    fn touch_recent_dedupes_same_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let xbel = tmp.path().join("xbel");
        let mut bm = load(&state, &xbel).expect("load empty");
        let p = tmp.path().join("dir");
        bm.touch_recent(&p).expect("first touch");
        bm.touch_recent(&p).expect("second touch");
        let recent = bm.read_recent().expect("read recent");
        assert_eq!(recent.len(), 1, "same path → one dedup entry");
    }

    /// Coverage for the unpin path so the public surface stays
    /// reachable from the test side.
    #[test]
    fn unpin_removes_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let xbel = tmp.path().join("xbel");
        let mut bm = load(&state, &xbel).expect("load empty");
        let p = tmp.path().join("dir");
        bm.pin('z', p, None).expect("pin");
        assert!(bm.jump('z').is_some());
        bm.unpin('z').expect("unpin");
        assert!(bm.jump('z').is_none());
    }
}
