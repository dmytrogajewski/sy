//! `sy file` keymap loader. Roadmap Step 34 (SPEC §3.3 item 17 +
//! item 18) — `configs/sy/file-keymap.toml` is the user-overridable
//! layer the operator edits in `$XDG_CONFIG_HOME/sy/file-keymap.toml`
//! after `sy apply` writes the productivised default. The iced
//! reducer in `src/file/app.rs::handle_key` hard-codes the same
//! bindings; this loader is the source-of-truth for a future
//! "hot-load keymap on SIGHUP" pass to consult.
//!
//! Shape mirrors yazi's `keymap.toml`:
//!
//! ```toml
//! [[keymap]]
//! keys = ["space"]
//! action = "selection.toggle"
//! ```
//!
//! Today the loader returns the parsed list verbatim — the reducer
//! doesn't yet consult it at dispatch time. SIGHUP swaps the live
//! [`KeymapConfig`] on the daemon so the wire-shape ratchet lands
//! before the dispatch-time integration in a follow-up step.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// One keychord → action row from `file-keymap.toml`. Stable wire
/// shape; new keys land via `serde(default)` so older productivised
/// files keep loading.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Binding {
    /// The keychord(s) that fire this action. Multi-entry lists let
    /// an operator bind both `space` and `enter` to the same action
    /// without two `[[keymap]]` blocks.
    pub keys: Vec<String>,
    /// Behavioural action — namespaced (`selection.toggle`,
    /// `commandbar.open_filter`, …) so the reducer dispatch table
    /// can map each prefix to its handler.
    pub action: String,
}

/// Parsed `file-keymap.toml` body. Order-preserving; the reducer's
/// future dispatch path reads first-match-wins so the operator's
/// overrides land at the top of their file.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct KeymapConfig {
    /// The full list of bindings in file order. Empty when no rows
    /// were declared; `default()` produces the yazi-shaped defaults.
    #[serde(default, rename = "keymap")]
    pub bindings: Vec<Binding>,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

impl KeymapConfig {
    /// Yazi-shaped default keymap. Mirrors the productivised
    /// `configs/sy/file-keymap.toml` so a daemon launched without a
    /// user override behaves identically to one that reads the
    /// productivised file.
    pub fn defaults() -> Self {
        let pairs: &[(&str, &str)] = &[
            ("space", "selection.toggle"),
            ("shift+arrowdown", "selection.range_extend_down"),
            ("shift+arrowup", "selection.range_extend_up"),
            ("*", "selection.all"),
            ("a", "selection.invert"),
            ("y", "clipboard.stash_copy"),
            ("x", "clipboard.stash_move"),
            ("p", "clipboard.paste"),
            ("d", "fs.trash"),
            ("/", "commandbar.open_filter"),
            (":", "commandbar.open_palette"),
            ("escape", "commandbar.close"),
            ("b", "bookmark.chord_jump"),
            ("B", "bookmark.chord_pin"),
        ];
        let bindings = pairs
            .iter()
            .map(|(k, a)| Binding {
                keys: vec![(*k).to_owned()],
                action: (*a).to_owned(),
            })
            .collect();
        Self { bindings }
    }

    /// Parse a `file-keymap.toml` body. Returns the yazi-shaped
    /// defaults when the body has no `[[keymap]]` rows so a half-
    /// edited file doesn't strand the operator with no bindings.
    pub fn parse(body: &str) -> Result<Self> {
        let mut cfg: Self = toml::from_str(body).context("parse file-keymap.toml")?;
        if cfg.bindings.is_empty() {
            cfg = Self::defaults();
        }
        Ok(cfg)
    }

    /// Read `path` and parse it. The caller (the daemon's SIGHUP
    /// handler) is responsible for resolving the productivised
    /// fallback (`<repo>/configs/sy/file-keymap.toml`) when the user
    /// override is absent.
    pub fn load(path: &Path) -> Result<Self> {
        let body =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&body)
    }

    /// Number of bindings the parsed config carries. Pinned in the
    /// SIGHUP test to assert a fresh load picked up a new entry; the
    /// daemon's `reload_keymap` traces it on every hot-reload.
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// `true` when no `[[keymap]]` rows were parsed. Mirrors
    /// `Vec::is_empty` so clippy is happy with `len()`. Pinned by
    /// the SIGHUP e2e and by the defaults unit test below.
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Look up the action bound to `key`, first match wins (matches
    /// the reducer's intended dispatch contract). Returns `None` when
    /// no binding is present so a half-edited file doesn't crash the
    /// daemon. Pinned in the SIGHUP e2e to assert a fresh entry took.
    pub fn action_for(&self, key: &str) -> Option<&str> {
        self.bindings
            .iter()
            .find(|b| b.keys.iter().any(|k| k == key))
            .map(|b| b.action.as_str())
    }
}

/// Resolve `$XDG_CONFIG_HOME/sy/file-keymap.toml` (falls back to
/// `$HOME/.config/sy/file-keymap.toml` per the freedesktop XDG
/// basedir spec). Pulled out so the daemon and any future MCP op can
/// resolve the same path. Pure-fn (env reads only); the I/O happens
/// inside [`KeymapConfig::load`].
pub fn user_keymap_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        });
    base.join("sy").join("file-keymap.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_carry_yazi_shaped_set() {
        let cfg = KeymapConfig::defaults();
        assert!(!cfg.is_empty(), "defaults must populate at least once");
        assert_eq!(cfg.action_for("space"), Some("selection.toggle"));
        assert_eq!(cfg.action_for(":"), Some("commandbar.open_palette"));
        assert_eq!(cfg.action_for("escape"), Some("commandbar.close"));
    }

    #[test]
    fn parse_reads_productivised_file_keymap_shape() {
        let body = r#"
[[keymap]]
keys = ["space"]
action = "selection.toggle"

[[keymap]]
keys = ["/"]
action = "commandbar.open_filter"
"#;
        let cfg = KeymapConfig::parse(body).expect("parse");
        assert_eq!(cfg.len(), 2);
        assert_eq!(cfg.action_for("space"), Some("selection.toggle"));
        assert_eq!(cfg.action_for("/"), Some("commandbar.open_filter"));
    }

    #[test]
    fn parse_falls_back_to_defaults_on_empty_body() {
        let cfg = KeymapConfig::parse("").expect("parse empty");
        assert_eq!(cfg.action_for("space"), Some("selection.toggle"));
    }
}
