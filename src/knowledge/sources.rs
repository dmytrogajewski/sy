//! Read/write [[knowledge.sources]] in sy.toml.
//!
//! sy.toml is the source of truth for which paths are indexed. `sy
//! knowledge add/rm/schedule` rewrite it atomically (temp + rename, same
//! pattern as `src/stack/state.rs:save`). The daemon also reads sy.toml on
//! `RefreshSources` IPC events.

use std::{
    env,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::DEFAULT_SCHEDULE;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnowledgeSection {
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub qdrant_port: Option<u16>,
    /// Whether to auto-discover `qdr.toml` files in `$HOME` at depth ≤ 2.
    /// Default `true`. Disable to opt out of the shallow-home convenience
    /// watcher; explicit `mode = "discover"` sources still work.
    #[serde(default)]
    pub discover_home: Option<bool>,
    /// Sleep this many ms after each embed-batch upsert during scheduled
    /// daemon passes. 0 = off. Has no effect on user-driven `sy knowledge
    /// index/sync/search` (they should stay snappy).
    #[serde(default)]
    pub cpu_throttle_ms: Option<u64>,
    /// Adaptive CPU cap: 1..100 = the daemon polls its own `/proc/self/stat`
    /// after each batch and inserts a sleep proportional to overshoot to
    /// keep average usage near this percentage. 0 / unset = no cap.
    #[serde(default)]
    pub cpu_max_percent: Option<u8>,
    /// Process nice level applied at daemon startup. Default 10 (lower
    /// priority than interactive shells). Range -20..19 per setpriority(2).
    #[serde(default)]
    pub nice: Option<i32>,
    /// Whether `sy auto-configure` (and `sy knowledge mcp-enable/disable`)
    /// should keep `sy-knowledge` registered in each known agent's
    /// MCP-server config. Default `true`. Honours `SY_KNOWLEDGE_MCP_ENABLED`.
    #[serde(default)]
    pub mcp_enabled: Option<bool>,
    #[serde(default, rename = "sources")]
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SourceMode {
    /// Walk the entire subtree and index everything (subject to .gitignore
    /// and the global file-size cap). Backwards-compatible with pre-qdr.toml
    /// behaviour.
    #[default]
    Explicit,
    /// Walk the subtree looking for `qdr.toml` files. Each manifested
    /// folder becomes its own indexed source under the manifest's rules.
    Discover,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub path: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: SourceMode,
}

fn default_true() -> bool {
    true
}

/// Locate the sy repo root. Walks up from cwd; falls back to ~/sources/sy.
pub fn find_root() -> Result<PathBuf> {
    if let Ok(r) = env::var("SY_ROOT") {
        if !r.is_empty() {
            return Ok(PathBuf::from(r));
        }
    }
    let mut cur = env::current_dir().context("cwd")?;
    loop {
        if cur.join("sy.toml").exists()
            && cur.join("configs").is_dir()
            && cur.join("themes").is_dir()
        {
            return Ok(cur);
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => {
                if let Ok(home) = env::var("HOME") {
                    let guess = PathBuf::from(home).join("sources/sy");
                    if guess.exists() {
                        return Ok(guess);
                    }
                }
                anyhow::bail!("could not find sy repo root (set SY_ROOT or run from inside the repo)")
            }
        }
    }
}

pub fn sy_toml_path() -> Result<PathBuf> {
    Ok(find_root()?.join("sy.toml"))
}

pub fn load() -> Result<KnowledgeSection> {
    let p = sy_toml_path()?;
    if !p.exists() {
        return Ok(KnowledgeSection::default());
    }
    let s = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;

    #[derive(Deserialize, Default)]
    struct Top {
        #[serde(default)]
        knowledge: KnowledgeSection,
    }
    let top: Top = toml::from_str(&s).with_context(|| format!("parse {}", p.display()))?;
    Ok(top.knowledge)
}

pub fn schedule_interval() -> String {
    load()
        .ok()
        .and_then(|k| k.schedule)
        .unwrap_or_else(|| DEFAULT_SCHEDULE.to_string())
}

/// Add a source. Returns true if newly added, false if already present.
pub fn add(path: &Path, disabled: bool, mode: SourceMode) -> Result<bool> {
    let abs = canonicalize_for_storage(path)?;
    let mut k = load()?;
    if k.sources.iter().any(|s| same_path(&s.path, &abs)) {
        return Ok(false);
    }
    k.sources.push(Source {
        path: abs,
        enabled: !disabled,
        mode,
    });
    write(&k)?;
    Ok(true)
}

/// Remove a source. Returns true if it was removed, false if not present.
pub fn remove(path: &Path) -> Result<bool> {
    let abs = canonicalize_for_storage(path)?;
    let mut k = load()?;
    let before = k.sources.len();
    k.sources.retain(|s| !same_path(&s.path, &abs));
    if k.sources.len() == before {
        return Ok(false);
    }
    write(&k)?;
    Ok(true)
}

/// Set the schedule interval. Validates the format.
pub fn set_schedule(interval: &str) -> Result<()> {
    parse_interval(interval).with_context(|| format!("invalid interval '{interval}'"))?;
    let mut k = load()?;
    k.schedule = Some(interval.to_string());
    write(&k)
}

/// Parse a schedule string like `15m`, `30s`, `1h` into seconds.
pub fn parse_interval(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty interval");
    }
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = num.parse().context("interval is not a number")?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        other => anyhow::bail!("unknown unit '{other}' (expected s/m/h/d)"),
    };
    if secs == 0 {
        anyhow::bail!("interval must be > 0");
    }
    Ok(secs)
}

/// Expand `~` and resolve relative paths to absolute. Does NOT require the
/// path to exist (we want to allow registering paths that haven't been
/// created yet).
pub fn expand(path: &str) -> Result<PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = env::var("HOME").context("HOME not set")?;
        return Ok(PathBuf::from(home).join(rest));
    }
    if path == "~" {
        return Ok(PathBuf::from(env::var("HOME").context("HOME not set")?));
    }
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return Ok(p);
    }
    Ok(env::current_dir()?.join(p))
}

fn canonicalize_for_storage(path: &Path) -> Result<String> {
    // Store absolute path; use canonicalize where possible so duplicates
    // (different ways of writing the same path) collapse.
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let canon = fs::canonicalize(&abs).unwrap_or(abs);
    // Reverse-substitute $HOME → "~/" for storage, so sy.toml stays
    // portable across machines if HOME differs.
    if let Ok(home) = env::var("HOME") {
        let home_p = PathBuf::from(&home);
        if let Ok(stripped) = canon.strip_prefix(&home_p) {
            return Ok(format!("~/{}", stripped.display()));
        }
    }
    Ok(canon.display().to_string())
}

fn same_path(a: &str, b: &str) -> bool {
    let pa = expand(a).unwrap_or_else(|_| PathBuf::from(a));
    let pb = expand(b).unwrap_or_else(|_| PathBuf::from(b));
    fs::canonicalize(&pa).unwrap_or(pa) == fs::canonicalize(&pb).unwrap_or(pb)
}

/// Atomically rewrite sy.toml's [knowledge] section in-place, preserving
/// every comment, blank line, and ordering. We edit existing fields in
/// place where possible and only insert/remove keys that actually changed,
/// so leading-comment decoration on the [knowledge] table itself survives.
fn write(section: &KnowledgeSection) -> Result<()> {
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

    let p = sy_toml_path()?;
    let body = if p.exists() {
        fs::read_to_string(&p)?
    } else {
        String::new()
    };
    let mut doc: DocumentMut = body
        .parse()
        .with_context(|| format!("parse {}", p.display()))?;

    // Get-or-create the [knowledge] table without dropping its decoration.
    if doc.get("knowledge").is_none() {
        doc.insert("knowledge", Item::Table(Table::new()));
    }
    let k = doc["knowledge"]
        .as_table_mut()
        .context("[knowledge] is not a table")?;

    // Scalar fields: set if Some, remove if None.
    set_or_remove(k, "schedule", section.schedule.as_deref().map(|s| value(s)));
    set_or_remove(
        k,
        "embedding_model",
        section.embedding_model.as_deref().map(|s| value(s)),
    );
    set_or_remove(
        k,
        "qdrant_port",
        section.qdrant_port.map(|p| value(p as i64)),
    );

    // [[knowledge.sources]] — rebuild the array of tables. Preserves the
    // KEY's decoration (the comment block above `[[knowledge.sources]]`),
    // only the entries themselves get rewritten.
    set_or_remove(
        k,
        "discover_home",
        section.discover_home.map(|b| value(b)),
    );
    set_or_remove(
        k,
        "cpu_throttle_ms",
        section.cpu_throttle_ms.map(|n| value(n as i64)),
    );
    set_or_remove(
        k,
        "cpu_max_percent",
        section.cpu_max_percent.map(|n| value(n as i64)),
    );
    set_or_remove(k, "nice", section.nice.map(|n| value(n as i64)));
    set_or_remove(k, "mcp_enabled", section.mcp_enabled.map(value));

    if section.sources.is_empty() {
        k.remove("sources");
    } else {
        let mut arr = ArrayOfTables::new();
        for s in &section.sources {
            let mut t = Table::new();
            t.insert("path", value(s.path.clone()));
            t.insert("enabled", value(s.enabled));
            // Only emit `mode` when non-default, to keep sy.toml tidy for
            // pre-existing `[[knowledge.sources]]` entries.
            if s.mode != SourceMode::Explicit {
                let m = match s.mode {
                    SourceMode::Explicit => "explicit",
                    SourceMode::Discover => "discover",
                };
                t.insert("mode", value(m));
            }
            arr.push(t);
        }
        k.insert("sources", Item::ArrayOfTables(arr));
    }

    let serialised = doc.to_string();
    let tmp = p.with_extension("toml.tmp");
    {
        let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(serialised.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &p).with_context(|| format!("rename to {}", p.display()))?;
    Ok(())
}

fn set_or_remove(t: &mut toml_edit::Table, key: &str, new: Option<toml_edit::Item>) {
    match new {
        Some(v) => {
            t.insert(key, v);
        }
        None => {
            t.remove(key);
        }
    }
}

/// Notify the daemon that sources or schedule changed (best-effort).
pub fn notify_daemon_refresh() {
    let _ = super::ipc::send(&super::ipc::Op::RefreshSources);
}

#[allow(dead_code)] // used by tests / future tooling
pub fn save_for_test(section: &KnowledgeSection) -> Result<()> {
    write(section)
}

/// Return the list of enabled, expanded source roots in `Explicit` mode.
/// Manifest-discovered roots are NOT returned here — they're enumerated
/// dynamically by `manifest::discover_all()`.
pub fn enabled_paths() -> Result<Vec<PathBuf>> {
    Ok(load()?
        .sources
        .into_iter()
        .filter(|s| s.enabled && s.mode == SourceMode::Explicit)
        .map(|s| expand(&s.path).unwrap_or_else(|_| PathBuf::from(s.path)))
        .collect())
}

/// Enabled, expanded `mode = "discover"` roots. The daemon walks each of
/// these recursively (gitignore-aware) for `qdr.toml` files.
pub fn discover_roots() -> Result<Vec<PathBuf>> {
    Ok(load()?
        .sources
        .into_iter()
        .filter(|s| s.enabled && s.mode == SourceMode::Discover)
        .map(|s| expand(&s.path).unwrap_or_else(|_| PathBuf::from(s.path)))
        .collect())
}

/// Whether shallow-`$HOME` (depth ≤ 2) auto-discovery is on. Defaults to
/// true when `[knowledge].discover_home` is unset. Honours
/// `SY_KNOWLEDGE_DISCOVER_HOME=0|1`.
pub fn discover_home_enabled() -> bool {
    if let Ok(v) = env::var("SY_KNOWLEDGE_DISCOVER_HOME") {
        return matches!(v.as_str(), "1" | "true" | "yes" | "on");
    }
    load().ok().and_then(|k| k.discover_home).unwrap_or(true)
}

/// CPU throttle between embed batches (scheduled daemon passes only).
/// Honours `SY_KNOWLEDGE_CPU_THROTTLE_MS`.
pub fn cpu_throttle() -> Duration {
    if let Ok(v) = env::var("SY_KNOWLEDGE_CPU_THROTTLE_MS") {
        if let Ok(n) = v.parse::<u64>() {
            return Duration::from_millis(n);
        }
    }
    let ms = load().ok().and_then(|k| k.cpu_throttle_ms).unwrap_or(0);
    Duration::from_millis(ms)
}

/// Adaptive CPU cap target as a fraction (0.0..1.0). `None` = uncapped.
/// Honours `SY_KNOWLEDGE_CPU_MAX_PERCENT`.
pub fn cpu_max_percent() -> Option<u8> {
    if let Ok(v) = env::var("SY_KNOWLEDGE_CPU_MAX_PERCENT") {
        if let Ok(n) = v.parse::<u8>() {
            return if (1..=100).contains(&n) { Some(n) } else { None };
        }
    }
    load()
        .ok()
        .and_then(|k| k.cpu_max_percent)
        .filter(|n| (1..=100).contains(n))
}

/// Nice level applied at daemon startup. Defaults to 10. Honours
/// `SY_KNOWLEDGE_NICE`.
pub fn nice_level() -> i32 {
    if let Ok(v) = env::var("SY_KNOWLEDGE_NICE") {
        if let Ok(n) = v.parse::<i32>() {
            return n;
        }
    }
    load().ok().and_then(|k| k.nice).unwrap_or(10)
}

/// Whether `sy-knowledge` should be registered in agents' MCP configs.
/// Default `true`. Honours `SY_KNOWLEDGE_MCP_ENABLED=0|1|true|false`.
pub fn mcp_enabled() -> bool {
    if let Ok(v) = env::var("SY_KNOWLEDGE_MCP_ENABLED") {
        return matches!(v.as_str(), "1" | "true" | "yes" | "on");
    }
    load().ok().and_then(|k| k.mcp_enabled).unwrap_or(true)
}

/// Persist the `mcp_enabled` flag in sy.toml (round-trip via `toml_edit`).
pub fn set_mcp_enabled(value: bool) -> Result<()> {
    let mut k = load()?;
    k.mcp_enabled = Some(value);
    write(&k)
}
