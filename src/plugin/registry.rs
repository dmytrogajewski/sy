//! Manifest discovery + dispatch index for the `sy file` plugin runtime.
//!
//! Implements [SPEC §3.3 item 7][spec-discovery]: walks
//! `configs/sy/plugins/*/plugin.toml` (productivised, lowest precedence)
//! and `~/.local/share/sy/plugins/*/plugin.toml` (user-installed,
//! highest precedence outside of test overrides) and builds an in-memory
//! index keyed on `(CapKind, predicate)` so the file manager's hover
//! preview path can resolve "this MIME / URL → that PluginId" in O(1)
//! without scanning manifests at request time.
//!
//! The discovery roots are precedence-ordered (lowest → highest):
//!
//! 1. **Productivised** — `<workspace>/configs/sy/plugins/`, the
//!    no-snowflakes lane every first-party plugin (`sy-plugin-md`,
//!    Step 12) ships through.
//! 2. **User-installed** — `$XDG_DATA_HOME/sy/plugins/` (default
//!    `~/.local/share/sy/plugins/`), where `sy plugin install` lands
//!    third-party plugins. A user plugin with the same `id` as a
//!    productivised one **wins** — Step 9's install flow honours this
//!    so a maintainer can shadow a buggy upstream without editing the
//!    repo.
//! 3. **`$SY_PLUGIN_DIR`** — test / agent override. Only this root is
//!    scanned when the env var is set, which keeps integration tests
//!    hermetic (no leakage from the host's `~/.local/share/sy/`).
//!
//! Discovery is shallow by design — only `$root/*/plugin.toml` is read;
//! we never descend past depth 2. This is both an O(n)-in-manifests
//! guarantee and a security control (a malicious plugin can't hide a
//! second `plugin.toml` three subdirectories deep to smuggle a
//! capability shadowing another plugin's claim).
//!
//! Malformed manifests do **not** poison the registry: a syntactically
//! broken `plugin.toml` surfaces as a `tracing::warn!` and is dropped
//! from the index. The journey's J3 hover path must keep routing
//! markdown to `sy-plugin-md` even when an unrelated third-party plugin
//! ships a corrupted manifest.
//!
//! [spec-discovery]: ../../../specs/research/sy-file-manager-plugins/SPEC.md#33-scope
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::Glob;
use serde::Deserialize;

use crate::plugin::manifest::{self, Capability, Manifest};

/// Test / agent override for the single discovery root. When set,
/// **only** this directory is scanned — neither the productivised
/// `configs/sy/plugins/` lane nor the user `$XDG_DATA_HOME` lane is
/// read. Mirrors [SPEC §4.5 env table][spec-env] entry
/// `SY_PLUGIN_DIR`.
///
/// [spec-env]: ../../../specs/research/sy-file-manager-plugins/SPEC.md#45-cli--mcp-surface
pub const PLUGIN_DIR_ENV: &str = "SY_PLUGIN_DIR";

/// Override for the disabled-plugin TOML file. Default location is
/// `$XDG_STATE_HOME/sy/plugin/disabled.toml`. The override exists so
/// integration tests can point at a fixture under a tempdir without
/// touching the host's real state directory.
pub const DISABLED_TOML_ENV: &str = "SY_PLUGIN_DISABLED_TOML";

/// Unique plugin identity (mirrors `Manifest::plugin.id`). Wrapped in
/// a newtype so dispatch sites can't accidentally pass a free string
/// where a plugin id is expected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId(pub String);

impl PluginId {
    /// Borrow the underlying id as `&str` for logging / serialisation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Capability kind a plugin advertises. Mirrors SPEC §4.2.4 rows
/// (`previewer`, `opener`, `action`, `fetcher`, `indexer`, `cmdbar`).
///
/// Keeping this as a closed enum (rather than a free string) means the
/// dispatch index can't be polluted by a typo'd `"previewr"` slipping
/// past the manifest parser — [`CapKind::from_manifest_kind`] is the
/// single chokepoint where the string → enum conversion happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CapKind {
    /// `previewer` — hover-preview path (journey beat J3).
    Previewer,
    /// `opener` — Enter / xdg-open path.
    Opener,
    /// `action` — multi-select operation (journey beat J5/J6).
    Action,
    /// `fetcher` — badge / fetcher capability.
    Fetcher,
    /// `indexer` — knowledge-plane integration.
    Indexer,
    /// `cmdbar` — command-bar suggestion provider.
    Cmdbar,
}

impl CapKind {
    /// Parse the manifest's free-form `kind` string into the typed
    /// enum. Returns `None` on unknown kinds — the caller should warn
    /// and drop (forward-compat per SPEC §4.1).
    pub fn from_manifest_kind(s: &str) -> Option<Self> {
        match s {
            "previewer" => Some(CapKind::Previewer),
            "opener" => Some(CapKind::Opener),
            "action" => Some(CapKind::Action),
            "fetcher" => Some(CapKind::Fetcher),
            "indexer" => Some(CapKind::Indexer),
            "cmdbar" => Some(CapKind::Cmdbar),
            _ => None,
        }
    }
}

/// A single dispatch-index entry: one `(CapKind, glob-predicate)` row
/// pointing at the plugin that owns it. The registry builds one
/// `IndexEntry` per `[[capability]]` row per discovered manifest.
#[derive(Debug, Clone)]
struct IndexEntry {
    plugin_id: PluginId,
    kind: CapKind,
    /// Compiled url-glob predicate. `None` if the capability used a
    /// mime-glob only.
    url_glob: Option<Glob>,
    /// Compiled mime-glob predicate. `None` if the capability used a
    /// url-glob only.
    mime_glob: Option<Glob>,
}

/// Discovered + indexed plugin registry. Built once at startup (and
/// rebuilt by `sy plugin reload` in Step 8); the file manager's
/// hot-path code only ever reads from this snapshot.
///
/// Internally the registry holds:
///
/// * a `BTreeMap<PluginId, Manifest>` so `sy plugin list` / `cat-
///   manifest` can surface the discovered set in stable id order, and
/// * a `Vec<IndexEntry>` flattened across every `[[capability]]` row
///   across every discovered manifest — `select_for` walks this list,
///   filtering by `kind` and matching the (url|mime) predicates.
pub struct Registry {
    manifests: BTreeMap<PluginId, Manifest>,
    /// Source directory each manifest was loaded from — needed so
    /// downstream code (Step 8's `doctor.binary.reachable` check,
    /// Step 9's installed-plugin spawn) can resolve relative paths
    /// like `[plugin.binary] exec = "./bin/foo"` against the dir the
    /// manifest lives in. Step 7 didn't surface this because Step 8's
    /// integration tests planted absolute exec paths; Step 9 lands
    /// relative paths through `sy plugin install`, so doctor must
    /// resolve them or every freshly-installed plugin reports
    /// `binary.reachable = false`.
    manifest_dirs: BTreeMap<PluginId, PathBuf>,
    index: Vec<IndexEntry>,
}

impl Registry {
    /// Construct an empty registry — no manifests, no index. Used as
    /// the failure-mode return for [`discover_empty`] so the file
    /// plane's `app::run` (Step 27) can keep starting up when plugin
    /// discovery fails without burdening the call site with a fallible
    /// `Option<Registry>`. `Registry::select_for` on an empty registry
    /// always returns `None`, surfacing `BridgeError::NoMatch`.
    pub fn empty() -> Self {
        Self {
            manifests: BTreeMap::new(),
            manifest_dirs: BTreeMap::new(),
            index: Vec::new(),
        }
    }

    /// Public list of every discovered plugin id, sorted lexically.
    /// `sy plugin list` (Step 8) renders from this.
    pub fn plugin_ids(&self) -> impl Iterator<Item = &PluginId> {
        self.manifests.keys()
    }

    /// Borrow a discovered manifest by id. `sy plugin cat-manifest`
    /// (Step 8) renders from this.
    pub fn manifest(&self, id: &PluginId) -> Option<&Manifest> {
        self.manifests.get(id)
    }

    /// Borrow the directory the manifest was loaded from. Used by
    /// Step 8's `doctor.binary.reachable` check to resolve relative
    /// `[plugin.binary] exec` paths and by Step 9's install /
    /// supervisor wiring to compute the sandbox `workdir`. Returns
    /// `None` for plugin ids the registry doesn't know.
    pub fn manifest_dir(&self, id: &PluginId) -> Option<&Path> {
        self.manifest_dirs.get(id).map(|p| p.as_path())
    }

    /// Resolve the dispatch route for a `(kind, mime, url)` triple,
    /// honouring two tie-break rules (matches SPEC §3.3 item 7):
    ///
    /// 1. **URL globs win over MIME globs.** A capability with a `url`
    ///    predicate that matches the candidate URL ranks higher than
    ///    one that matches only via its mime predicate. The journey
    ///    J3 hover path passes both — a `README.md` file at MIME
    ///    `text/markdown` — and the plugin author may have declared
    ///    one or the other (or both); the URL predicate is more
    ///    specific.
    /// 2. **Ties broken by manifest id alphabetical.** A deterministic
    ///    fallback so two plugins claiming the same (kind, predicate)
    ///    resolve identically across runs.
    ///
    /// Returns `None` when no capability matches. Callers fall back to
    /// the built-in text path (Phase B Step 19 will land that).
    pub fn select_for(&self, kind: CapKind, mime: &str, url: &str) -> Option<&PluginId> {
        let mut url_match: Option<&IndexEntry> = None;
        let mut mime_match: Option<&IndexEntry> = None;
        for entry in self.index.iter().filter(|e| e.kind == kind) {
            if let Some(g) = entry.url_glob.as_ref() {
                if g.compile_matcher().is_match(url)
                    && url_match
                        .map(|cur| entry.plugin_id < cur.plugin_id)
                        .unwrap_or(true)
                {
                    url_match = Some(entry);
                    continue;
                }
            }
            if let Some(g) = entry.mime_glob.as_ref() {
                if g.compile_matcher().is_match(mime)
                    && mime_match
                        .map(|cur| entry.plugin_id < cur.plugin_id)
                        .unwrap_or(true)
                {
                    mime_match = Some(entry);
                }
            }
        }
        url_match.or(mime_match).map(|e| &e.plugin_id)
    }
}

/// Convenience for the file plane's `app::run` (Step 27) fall-back
/// path. Same return type as [`discover`] minus the `Result` wrapper —
/// returns an empty registry that always answers `None` to
/// `select_for`. Lets the file manager keep starting when discovery
/// itself blows up.
pub fn discover_empty() -> Registry {
    Registry::empty()
}

/// Discover manifests from the precedence-ordered roots and build the
/// dispatch index. Honours `$SY_PLUGIN_DIR` (sole override), then the
/// user XDG lane, then the productivised lane. User-installed plugins
/// override productivised ones when ids collide.
///
/// Malformed manifests are logged via `tracing::warn!` and skipped —
/// they never abort discovery, so one corrupt third-party plugin can't
/// take down J3's hover preview routing for `sy-plugin-md`.
pub fn discover() -> Result<Registry> {
    let roots = resolve_roots();
    let disabled = load_disabled_set();
    // Lowest precedence first; later inserts override earlier ones by
    // id. The roots list is already ordered productivised → user-XDG,
    // and `$SY_PLUGIN_DIR` (when set) replaces the whole list.
    let mut manifests: BTreeMap<PluginId, Manifest> = BTreeMap::new();
    let mut manifest_dirs: BTreeMap<PluginId, PathBuf> = BTreeMap::new();
    for root in &roots {
        scan_root(root, &mut manifests, &mut manifest_dirs);
    }
    if !disabled.is_empty() {
        manifests.retain(|id, _| !disabled.contains(&id.0));
        manifest_dirs.retain(|id, _| !disabled.contains(&id.0));
    }
    let index = build_index(&manifests);
    Ok(Registry {
        manifests,
        manifest_dirs,
        index,
    })
}

/// Returns the precedence-ordered list of roots to scan (lowest →
/// highest precedence). When `$SY_PLUGIN_DIR` is set, returns it as the
/// only root so tests can run hermetically without seeing the host's
/// real install lanes.
fn resolve_roots() -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os(PLUGIN_DIR_ENV) {
        return vec![PathBuf::from(p)];
    }
    let mut roots = Vec::with_capacity(2);
    // Productivised lane — relative to the workspace root. The bin
    // never reads this at runtime (Step 8's CLI scans
    // `$XDG_DATA_HOME` only), but `sy apply` (Step 35) productivises
    // these into `$XDG_DATA_HOME` via symlink so the runtime path is
    // single-rooted. The path is kept here so dev-mode discovery
    // (running `cargo run` from the workspace) still sees the
    // first-party plugins.
    roots.push(PathBuf::from("configs/sy/plugins"));
    // User-installed lane — `$XDG_DATA_HOME/sy/plugins/` (default
    // `~/.local/share/sy/plugins/`). Wins on id collision because the
    // user explicitly installed it.
    if let Some(home) = xdg_data_plugins_dir() {
        roots.push(home);
    }
    roots
}

/// Resolve `$XDG_DATA_HOME/sy/plugins/` with the freedesktop fallback
/// to `~/.local/share/sy/plugins/`. Returns `None` if neither
/// `$XDG_DATA_HOME` nor `$HOME` is set — the caller treats that as
/// "no user lane to scan" rather than as an error.
fn xdg_data_plugins_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(d).join("sy").join("plugins"));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("sy")
            .join("plugins"),
    )
}

/// Scan one root for `*/plugin.toml`. Depth capped at 2 (the root
/// itself + one immediate child dir). Missing roots are silently
/// skipped — running `sy file` on a host without any plugins
/// installed is the common case, not an error.
fn scan_root(
    root: &Path,
    out: &mut BTreeMap<PluginId, Manifest>,
    dirs: &mut BTreeMap<PluginId, PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for ent in entries.flatten() {
        let manifest_dir = ent.path();
        let manifest_path = manifest_dir.join("plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }
        match load_manifest_file(&manifest_path) {
            Ok(m) => {
                let id = PluginId(m.plugin.id.clone());
                // Later roots in the list win on id collision (user
                // overrides productivised) — `insert` overwrites.
                out.insert(id.clone(), m);
                dirs.insert(id, manifest_dir);
            }
            Err(err) => {
                tracing::warn!(
                    target = "sy::plugin::registry",
                    path = %manifest_path.display(),
                    error = %err,
                    "plugin.toml malformed; skipping"
                );
            }
        }
    }
}

/// Read + parse + validate a single `plugin.toml`. The
/// `manifest::load` helper already runs the SPEC §4.1 grammar checks;
/// any error surfaced here is fatal for the *plugin*, not the registry.
fn load_manifest_file(path: &Path) -> Result<Manifest> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("read plugin.toml at {}", path.display()))?;
    manifest::load(&src).with_context(|| format!("parse plugin.toml at {}", path.display()))
}

/// Flatten every `[[capability]]` row across the discovered manifests
/// into the linear index `select_for` walks at dispatch time. We
/// compile the globs once at registry-build time so the hot path is
/// "glob.compile_matcher().is_match(url)" with no allocation beyond
/// the matcher itself.
fn build_index(manifests: &BTreeMap<PluginId, Manifest>) -> Vec<IndexEntry> {
    let mut out = Vec::new();
    for (id, m) in manifests {
        for cap in &m.capabilities {
            let Some(kind) = CapKind::from_manifest_kind(&cap.kind) else {
                tracing::warn!(
                    target = "sy::plugin::registry",
                    plugin_id = %id.as_str(),
                    kind = %cap.kind,
                    "unknown capability kind; dropping from dispatch index (forward-compat)"
                );
                continue;
            };
            let url_glob = compile_or_warn(id, cap, cap.url.as_deref(), "url");
            let mime_glob = compile_or_warn(id, cap, cap.mime.as_deref(), "mime");
            out.push(IndexEntry {
                plugin_id: id.clone(),
                kind,
                url_glob,
                mime_glob,
            });
        }
    }
    out
}

/// Compile a glob predicate (url or mime) or warn-and-drop on failure.
/// `manifest::validate` already rejects malformed globs at load time,
/// so this only ever fires if a later commit loosens the manifest
/// validator — keeping it defensive avoids a panic in the dispatch
/// index.
fn compile_or_warn(
    id: &PluginId,
    cap: &Capability,
    raw: Option<&str>,
    kind_hint: &str,
) -> Option<Glob> {
    let s = raw?;
    match Glob::new(s) {
        Ok(g) => Some(g),
        Err(err) => {
            tracing::warn!(
                target = "sy::plugin::registry",
                plugin_id = %id.as_str(),
                cap_kind = %cap.kind,
                glob_kind = %kind_hint,
                glob = %s,
                error = %err,
                "capability glob failed to compile; dropping predicate"
            );
            None
        }
    }
}

/// TOML shape of `$SY_PLUGIN_DISABLED_TOML` (default
/// `$XDG_STATE_HOME/sy/plugin/disabled.toml`):
///
/// ```toml
/// disabled = ["sample", "another-id"]
/// ```
#[derive(Deserialize)]
struct DisabledFile {
    #[serde(default)]
    disabled: Vec<String>,
}

/// Load the disabled-id list from the override env var or the default
/// XDG path. Returns an empty set if neither file exists (the common
/// case) or if the file is malformed (with a `tracing::warn!`).
fn load_disabled_set() -> std::collections::BTreeSet<String> {
    let path = disabled_toml_path();
    let Some(p) = path else {
        return std::collections::BTreeSet::new();
    };
    let Ok(src) = std::fs::read_to_string(&p) else {
        return std::collections::BTreeSet::new();
    };
    match toml::from_str::<DisabledFile>(&src) {
        Ok(d) => d.disabled.into_iter().collect(),
        Err(err) => {
            tracing::warn!(
                target = "sy::plugin::registry",
                path = %p.display(),
                error = %err,
                "disabled.toml malformed; ignoring"
            );
            std::collections::BTreeSet::new()
        }
    }
}

/// Resolve the disabled-toml path: `$SY_PLUGIN_DISABLED_TOML` →
/// `$XDG_STATE_HOME/sy/plugin/disabled.toml` →
/// `~/.local/state/sy/plugin/disabled.toml`. Returns `None` if no
/// suitable path can be constructed (neither override nor `$HOME`).
fn disabled_toml_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(DISABLED_TOML_ENV) {
        return Some(PathBuf::from(p));
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return Some(
            PathBuf::from(d)
                .join("sy")
                .join("plugin")
                .join("disabled.toml"),
        );
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("sy")
            .join("plugin")
            .join("disabled.toml"),
    )
}

/// Process-wide mutex serialising every test that mutates the env
/// vars this module reads (`SY_PLUGIN_DIR`, `SY_PLUGIN_DISABLED_TOML`,
/// `XDG_DATA_HOME`, `HOME`). Exposed at module scope (not under
/// `#[cfg(test)]`) so the integration-test binary in
/// `tests/sy_file_journey_e2e.rs` can lock against the same instance
/// as the in-source `tests` module — both run in the same process
/// when the e2e binary `#[path]`-imports this file, and a per-mod
/// lock would let them race the env table against each other.
///
/// Outside tests this static is never touched; the `dead_code` lint
/// is pacified because the `tests` modules in this file and in the
/// e2e binary both reference it via the `env_lock()` helper.
///
/// `#[cfg(test)]`: the bin never reads this static — it
/// exists solely so the in-source `#[cfg(test)] mod tests` tree
/// and the `tests/sy_file_journey_e2e.rs` integration binary
/// share one process-wide mutex when they both `#[path]`-import
/// `registry.rs`. Step 13's daemon does not consume it either;
/// the lock is permanent test infrastructure.
#[doc(hidden)]
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the process-wide env-lock for the duration of an
/// env-mutating test. Returns a poisoned-guard-safe lock so a panic
/// in one test doesn't permanently block the rest.
///
/// Exported (rather than left module-private) so the e2e binary in
/// `tests/sy_file_journey_e2e.rs` can serialise its `step07_*` env
/// mutations against the in-source `tests` module's mutations of the
/// same env vars.
///
/// `#[cfg(test)]`: see [`ENV_LOCK`] — this is permanent test
/// infrastructure, not bin code.
#[doc(hidden)]
#[cfg(test)]
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    //! Registry tests run hermetically by setting `$SY_PLUGIN_DIR` to a
    //! tempdir — this both shortcircuits the precedence ladder to a
    //! single known root and prevents leakage from the host's real
    //! `~/.local/share/sy/plugins/` install lane.
    //!
    //! The tests use the module-level [`ENV_LOCK`] when mutating
    //! `$SY_PLUGIN_DIR` and friends because the integration-test
    //! binary runs every `#[test]` in parallel by default and Rust's
    //! `set_var` is process-global. The lock is shared with the e2e
    //! binary so `step07_*` and the in-source tests serialise.
    use super::*;

    const SAMPLE_MANIFEST: &str = r#"
api = "1"

[plugin]
id = "sample"
name = "Sample Previewer"
version = "0.0.0"
api_min = "1"
api_max = "1"

[plugin.binary]
exec = "./bin/sample"

[[capability]]
kind = "previewer"
mime = "text/markdown"

[needs]
fs_read = []
fs_write = []
preview = []
knowledge = []
network = []
exec = []

[limits]
memory_mb = 64
cpu_seconds = 10
nofile = 64
spawn_timeout_ms = 500
shutdown_timeout_ms = 500
"#;

    fn write_manifest_under(root: &Path, plugin_dir_name: &str, manifest_body: &str) {
        let dir = root.join(plugin_dir_name);
        std::fs::create_dir_all(&dir).expect("mkdir plugin dir");
        std::fs::write(dir.join("plugin.toml"), manifest_body).expect("write plugin.toml");
    }

    /// Sets `$SY_PLUGIN_DIR` to `root` for the lifetime of the
    /// returned guard; clears it on drop so other tests aren't
    /// poisoned. The `env_lock` field keeps the test critical section
    /// single-threaded across both this `tests` module and the e2e
    /// binary in `tests/sy_file_journey_e2e.rs`.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl EnvGuard {
        fn new(plugin_dir: &Path) -> Self {
            let lock = env_lock();
            // SAFETY: the lock above serialises every registry test
            // against the process-global env table; no other thread
            // observes a mutation through this guard.
            unsafe {
                std::env::set_var(PLUGIN_DIR_ENV, plugin_dir);
                std::env::remove_var(DISABLED_TOML_ENV);
            }
            Self { _lock: lock }
        }
        fn with_disabled(&self, path: &Path) {
            // SAFETY: ENV_LOCK still held via `_lock`.
            unsafe {
                std::env::set_var(DISABLED_TOML_ENV, path);
            }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: see EnvGuard::new.
            unsafe {
                std::env::remove_var(PLUGIN_DIR_ENV);
                std::env::remove_var(DISABLED_TOML_ENV);
            }
        }
    }

    /// SPEC §3.3 item 7 — a productivised `plugin.toml` at
    /// `$root/sample/plugin.toml` must be discovered and indexed under
    /// the `PluginId("sample")` it declares.
    #[test]
    fn discovers_productivised_manifest() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_manifest_under(tmp.path(), "sample", SAMPLE_MANIFEST);
        let _g = EnvGuard::new(tmp.path());

        let reg = discover().expect("discover ok");
        let ids: Vec<&PluginId> = reg.plugin_ids().collect();
        assert_eq!(ids.len(), 1, "exactly one manifest discovered");
        assert_eq!(ids[0], &PluginId("sample".to_string()));
        assert!(reg.manifest(&PluginId("sample".to_string())).is_some());
    }

    /// SPEC §3.3 item 7 — when both lanes ship a plugin with the same
    /// id, the user-installed lane wins. We model this via the
    /// `$SY_PLUGIN_DIR` override carrying the **user** root + an
    /// additional ad-hoc productivised root the test plants in the
    /// same tempdir tree — the helper `scan_root` is the unit under
    /// test for the "later wins on collision" property.
    #[test]
    fn user_manifest_overrides_productivised_same_id() {
        let tmp = tempfile::tempdir().expect("tmp");
        let productivised = tmp.path().join("productivised");
        let user = tmp.path().join("user");
        std::fs::create_dir_all(&productivised).expect("mkdir productivised");
        std::fs::create_dir_all(&user).expect("mkdir user");
        // Same id, different version — the user version's value of
        // `version = "9.9.9"` proves which copy survived the override.
        write_manifest_under(&productivised, "sample", SAMPLE_MANIFEST);
        let user_src = SAMPLE_MANIFEST.replace("version = \"0.0.0\"", "version = \"9.9.9\"");
        write_manifest_under(&user, "sample", &user_src);

        // Walk the helper directly — `discover()` only exposes one
        // override root via env, but the unit under test
        // (`scan_root`) is the merge-by-id primitive we need to lock
        // in. Build the BTreeMap by walking productivised first then
        // user, which matches the precedence order `resolve_roots`
        // produces.
        let mut out: BTreeMap<PluginId, Manifest> = BTreeMap::new();
        let mut dirs: BTreeMap<PluginId, PathBuf> = BTreeMap::new();
        scan_root(&productivised, &mut out, &mut dirs);
        scan_root(&user, &mut out, &mut dirs);
        let m = out
            .get(&PluginId("sample".to_string()))
            .expect("sample present");
        assert_eq!(
            m.plugin.version, "9.9.9",
            "user manifest must win on id collision"
        );
    }

    /// URL globs are more specific than MIME globs — a manifest
    /// declaring `url = "*.md"` for `previewer` must outrank one
    /// declaring `mime = "text/*"` for the same kind when both could
    /// match. Journey J3 hovers `README.md` at MIME `text/markdown`
    /// and expects the `*.md` plugin to win.
    #[test]
    fn select_for_returns_specific_url_before_mime() {
        let tmp = tempfile::tempdir().expect("tmp");
        let url_only = SAMPLE_MANIFEST
            .replace("id = \"sample\"", "id = \"plugin-a-url\"")
            .replace("mime = \"text/markdown\"", "url = \"*.md\"");
        let mime_only = SAMPLE_MANIFEST
            .replace("id = \"sample\"", "id = \"plugin-b-mime\"")
            .replace("mime = \"text/markdown\"", "mime = \"text/*\"");
        write_manifest_under(tmp.path(), "plugin-a-url", &url_only);
        write_manifest_under(tmp.path(), "plugin-b-mime", &mime_only);
        let _g = EnvGuard::new(tmp.path());

        let reg = discover().expect("discover ok");
        let got = reg
            .select_for(CapKind::Previewer, "text/markdown", "README.md")
            .expect("dispatch hits");
        assert_eq!(
            got,
            &PluginId("plugin-a-url".to_string()),
            "url glob must beat mime glob; got {got:?}"
        );
    }

    /// A syntactically broken manifest must not poison the registry —
    /// the good one is returned and the bad one surfaces as a
    /// `tracing::warn!` (verified indirectly via "good is found",
    /// since `tracing` events aren't easily captured here).
    #[test]
    fn malformed_manifest_skipped_with_warn() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_manifest_under(tmp.path(), "good", SAMPLE_MANIFEST);
        // Syntactically broken TOML — missing closing quote.
        write_manifest_under(
            tmp.path(),
            "bad",
            "api = \"1\nthis is not valid toml at all",
        );
        let _g = EnvGuard::new(tmp.path());

        let reg = discover().expect("discover never propagates a parse error");
        let ids: Vec<&PluginId> = reg.plugin_ids().collect();
        assert_eq!(
            ids,
            vec![&PluginId("sample".to_string())],
            "only the good manifest survives; bad one is warn+dropped"
        );
    }

    /// A `disabled = ["sample"]` entry in the disabled-toml file
    /// removes that plugin from the discovered set. Mirrors the
    /// `sy plugin disable <id>` UX (Step 8).
    #[test]
    fn disabled_plugins_excluded() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_manifest_under(tmp.path(), "sample", SAMPLE_MANIFEST);
        let disabled_path = tmp.path().join("disabled.toml");
        std::fs::write(&disabled_path, "disabled = [\"sample\"]\n").expect("write disabled");
        let g = EnvGuard::new(tmp.path());
        g.with_disabled(&disabled_path);

        let reg = discover().expect("discover ok");
        assert_eq!(
            reg.plugin_ids().count(),
            0,
            "disabled plugin must not be in the registry"
        );
    }
}
