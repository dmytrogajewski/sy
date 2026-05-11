//! `qdr.toml` — folder-local indexing manifest.
//!
//! Drop a `qdr.toml` at a folder root and the knowledge daemon will pick
//! it up: that folder becomes an indexed source under the manifest's
//! per-folder rules (include/exclude globs, depth cap, file-size cap,
//! payload tags, schedule). The marker file is also the kill-switch:
//! delete it (or set `enabled = false`) and the daemon retires the source
//! and drops its qdrant points.
//!
//! Discovery topology lives in `daemon.rs`; this module just owns the
//! schema, the parser, and a `Walker` builder that maps a `QdrManifest`
//! onto an `ignore::WalkBuilder` + post-walk globset filter.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::Deserialize;

use super::extract::DEFAULT_MAX_BYTES;

pub const MANIFEST_FILENAME: &str = "qdr.toml";

/// Maximum recursion depth used by shallow-`$HOME` discovery. Picks up
/// `~/foo/qdr.toml` (depth 1) and `~/foo/bar/qdr.toml` (depth 2) without
/// scanning the whole home tree.
pub const SHALLOW_HOME_DEPTH: usize = 2;

#[derive(Debug, Clone, Deserialize)]
struct QdrToml {
    #[serde(default)]
    knowledge: ManifestSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ManifestSection {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    include: Option<Vec<String>>,
    #[serde(default)]
    exclude: Option<Vec<String>>,
    #[serde(default)]
    max_depth: Option<usize>,
    #[serde(default)]
    max_file_bytes: Option<u64>,
    #[serde(default)]
    respect_gitignore: Option<bool>,
    #[serde(default)]
    follow_symlinks: Option<bool>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// Parsed `qdr.toml` with defaults applied. Cheap to clone.
#[derive(Debug, Clone)]
pub struct QdrManifest {
    /// Absolute path of the folder containing the `qdr.toml`.
    pub folder: PathBuf,
    pub name: String,
    pub enabled: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_depth: Option<usize>,
    pub max_file_bytes: u64,
    pub respect_gitignore: bool,
    pub follow_symlinks: bool,
    pub schedule: Option<String>,
    pub tags: Vec<String>,
}

impl QdrManifest {
    /// Read `<folder>/qdr.toml` and apply defaults.
    pub fn load(folder: &Path) -> Result<Self> {
        let p = folder.join(MANIFEST_FILENAME);
        let body = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let parsed: QdrToml =
            toml::from_str(&body).with_context(|| format!("parse {}", p.display()))?;
        let s = parsed.knowledge;
        let name = s
            .name
            .unwrap_or_else(|| folder_basename(folder).unwrap_or_else(|| "qdr".into()));
        Ok(Self {
            folder: folder.to_path_buf(),
            name,
            enabled: s.enabled.unwrap_or(true),
            include: s.include.unwrap_or_default(),
            exclude: s.exclude.unwrap_or_default(),
            max_depth: s.max_depth,
            max_file_bytes: s.max_file_bytes.unwrap_or(DEFAULT_MAX_BYTES),
            respect_gitignore: s.respect_gitignore.unwrap_or(true),
            follow_symlinks: s.follow_symlinks.unwrap_or(false),
            schedule: s.schedule,
            tags: s.tags.unwrap_or_default(),
        })
    }

    /// `WalkBuilder` configured per the manifest. Caller still has to
    /// post-filter results through `glob_filter()` because globset isn't a
    /// first-class `ignore` filter.
    ///
    /// The walker auto-prunes subdirectories that hold their own `qdr.toml`
    /// — those are owned by an inner manifest, so the outer one shouldn't
    /// double-index them.
    pub fn walker(&self) -> WalkBuilder {
        let mut wb = WalkBuilder::new(&self.folder);
        wb.hidden(false)
            .git_ignore(self.respect_gitignore)
            .git_exclude(self.respect_gitignore)
            .git_global(self.respect_gitignore)
            .follow_links(self.follow_symlinks);
        if let Some(d) = self.max_depth {
            wb.max_depth(Some(d));
        }
        let root = self.folder.clone();
        wb.filter_entry(move |dent| {
            // Files: always pass; the file-level filtering lives elsewhere.
            if !dent.file_type().map_or(false, |ft| ft.is_dir()) {
                return true;
            }
            // The job's own root must be allowed (it carries our qdr.toml).
            if dent.path() == root.as_path() {
                return true;
            }
            // Any nested directory carrying a qdr.toml is owned by its own
            // (inner) manifest — skip the whole subtree here.
            !dent.path().join(MANIFEST_FILENAME).exists()
        });
        wb
    }

    /// Compile include/exclude globs once. Returns `None` when both lists
    /// are empty — caller can skip the filter step entirely.
    pub fn glob_filter(&self) -> Result<Option<ManifestGlobFilter>> {
        if self.include.is_empty() && self.exclude.is_empty() {
            return Ok(None);
        }
        Ok(Some(ManifestGlobFilter::compile(
            &self.folder,
            &self.include,
            &self.exclude,
        )?))
    }
}

/// Pre-compiled include/exclude matcher rooted at a manifest folder.
#[derive(Debug, Clone)]
pub struct ManifestGlobFilter {
    root: PathBuf,
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl ManifestGlobFilter {
    fn compile(root: &Path, include: &[String], exclude: &[String]) -> Result<Self> {
        let inc = if include.is_empty() {
            None
        } else {
            Some(build_glob_set(include).context("compile include globs")?)
        };
        let exc = if exclude.is_empty() {
            None
        } else {
            Some(build_glob_set(exclude).context("compile exclude globs")?)
        };
        Ok(Self {
            root: root.to_path_buf(),
            include: inc,
            exclude: exc,
        })
    }

    /// Decide whether the (absolute) file path should be indexed.
    pub fn matches(&self, path: &Path) -> bool {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        if let Some(inc) = &self.include {
            if !inc.is_match(rel) {
                return false;
            }
        }
        if let Some(exc) = &self.exclude {
            if exc.is_match(rel) {
                return false;
            }
        }
        true
    }
}

fn build_glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("invalid glob {p:?}"))?);
    }
    Ok(b.build()?)
}

fn folder_basename(p: &Path) -> Option<String> {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}

/// Discover manifests under `root`. `deep=false` caps recursion at
/// `SHALLOW_HOME_DEPTH` (used for `$HOME`); `deep=true` is unbounded
/// (used for explicit `mode = "discover"` roots).
///
/// Walks gitignore-aware so casual `target/` / `node_modules/` trees
/// don't blow the discovery cost. Malformed manifests get logged and
/// skipped — discovery never fails the whole pass for one bad file.
pub fn discover(root: &Path, deep: bool) -> Vec<QdrManifest> {
    if !root.exists() {
        return Vec::new();
    }
    let mut wb = WalkBuilder::new(root);
    wb.hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true);
    if !deep {
        wb.max_depth(Some(SHALLOW_HOME_DEPTH));
    }
    let mut out = Vec::new();
    for dent in wb.build() {
        let dent = match dent {
            Ok(d) => d,
            Err(_) => continue,
        };
        let p = dent.path();
        if !p.is_file() {
            continue;
        }
        if p.file_name().and_then(|n| n.to_str()) != Some(MANIFEST_FILENAME) {
            continue;
        }
        let folder = match p.parent() {
            Some(f) => f.to_path_buf(),
            None => continue,
        };
        match QdrManifest::load(&folder) {
            Ok(m) => out.push(m),
            Err(e) => {
                eprintln!(
                    "sy knowledge: invalid {}: {e}",
                    p.display()
                );
            }
        }
    }
    out
}

/// Combine shallow-`$HOME` and configured deep discover roots into a flat
/// list, deduplicated by folder. Disabled manifests are returned with
/// `enabled = false` so callers can decide whether to retire them.
pub fn discover_all() -> Vec<QdrManifest> {
    use std::collections::HashMap;
    let mut by_folder: HashMap<PathBuf, QdrManifest> = HashMap::new();

    if super::sources::discover_home_enabled() {
        if let Ok(home) = std::env::var("HOME") {
            for m in discover(Path::new(&home), false) {
                by_folder.entry(m.folder.clone()).or_insert(m);
            }
        }
    }
    for root in super::sources::discover_roots().unwrap_or_default() {
        for m in discover(&root, true) {
            by_folder.entry(m.folder.clone()).or_insert(m);
        }
    }
    let mut out: Vec<QdrManifest> = by_folder.into_values().collect();
    out.sort_by(|a, b| a.folder.cmp(&b.folder));
    out
}
