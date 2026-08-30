//! In-memory state model for `sy file`. Step 14 of the
//! [`sy-file-manager` roadmap][roadmap] fills in `panes`, `selection`,
//! and `ops` submodules so the SPEC §3.1 shape
//! `State { panes, mode, selection, ops, … }` is reachable as pure
//! data — no I/O. Step 15+ binds these to the fs/IPC/UI layers.
//!
//! [roadmap]: ../../../../specs/roadmaps/sy-file-manager/ROADMAP.md

pub mod commandbar;
pub mod knowledge;
pub mod ops;
pub mod panes;
pub mod preview;
pub mod selection;

pub use commandbar::CommandBar;
#[cfg(feature = "gui-iced")]
pub use commandbar::CommandMode;
pub use knowledge::KnowledgeState;
pub use ops::{ConflictPolicy, OpEvent, Operation};
pub use panes::{Entry, EntryKind, Pane, PaneId, Panes};
pub use preview::PreviewState;
#[cfg(feature = "gui-iced")]
pub use preview::{HighlightedLine, HighlightedSpan, PreviewPayload};
pub use selection::{EntryId, SelectionSet};

// `ClipboardMode` and the new `State::clipboard` / `State::range_anchor`
// fields land below in this module. Re-export the `PathBuf` typedef
// alias here so consumers (`app::update`, `cli::Waybar`) don't need
// `std::path::PathBuf` imports just to name the clipboard tuple.

/// Step 28 — clipboard mode discriminator. SPEC §3.3 item 6 + journey
/// J5→J6 hand-off: `y` queues the selection as a *copy* clipboard,
/// `x` queues it as a *move* clipboard, `p` (paste) drives the
/// matching `Operation::Copy` / `Operation::Move`. Keyed by the
/// journey verbs so a future MCP consumer (Step 21+) can drive the
/// same wire shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardMode {
    /// Paste should `Operation::Copy` the clipboard srcs into cwd.
    Copy,
    /// Paste should `Operation::Move` the clipboard srcs into cwd.
    Move,
}

/// Responsive-layout discriminator. SPEC §3.2 row 2 pins the ladder:
/// ≥1100 px = 3-pane, ≥720 px = 2-pane, < 720 px = 1-pane. The mode is
/// part of [`State`] (not a view-only var) because `Ctrl+1/2/3` locks
/// it user-side and IPC consumers (Step 20+) can read/set it.
///
/// The `*Pane` postfix is intentional — the SPEC, the keymap
/// (`Ctrl+1/2/3`), the journey beat **J7** ("3-pane → 2-pane →
/// 1-pane"), and the e2e tests all use the same vocabulary, so
/// renaming the variants just to silence `clippy::enum_variant_names`
/// would diverge the code from the docs. The clippy lint is muted
/// on this enum and only this enum.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// Parent · current · preview. Default on a "normal" desktop tile.
    #[default]
    ThreePane,
    /// Current · preview. Niri half-column.
    TwoPane,
    /// Current only; "p" pulls preview as a transient overlay.
    OnePane,
}

/// Top-level in-memory model the `sy file` plane mutates. SPEC §3.1
/// `state/mod.rs` shape; no I/O lives on the type, so the journey-J2
/// through J6 state machine is testable without a running fs.
#[derive(Debug, Default)]
pub struct State {
    /// The three-pane bundle (parent · current · preview).
    pub panes: Panes,
    /// Active layout mode (Step 23's responsive ladder writes this on
    /// `WindowEvent::Resized`).
    pub mode: LayoutMode,
    /// Multi-select set keyed by [`EntryId`] (journey-J5).
    pub selection: SelectionSet,
    /// Queued or in-flight file operations (journey-J6). Step 16+
    /// drains this from an async executor task; today it's an
    /// append-only Vec.
    pub ops: Vec<Operation>,
    /// Command-bar slice — `/` filter + `:` palette state. Step 25
    /// (SPEC §3.3 item 4 + item 7) lands the surface; the reducer in
    /// `app::update` is the only mutation path.
    pub commandbar: CommandBar,
    /// Preview-pane slice — Step 26 (SPEC §3.3 item 8). Carries the
    /// path of the currently-previewed entry so the async image-load
    /// `Task` can reconcile `Message::PreviewLoaded` with the cursor
    /// when the user has moved on before the decode finishes. Pure
    /// data (no I/O) so `--no-default-features` builds still see it.
    pub preview: PreviewState,
    /// Step 27 — plugin-routed previewer bridge. `None` for headless /
    /// non-GUI contexts (the Step 23-26 callers keep working without
    /// having to opt-in); `Some` once `app::run` has discovered the
    /// registry and wrapped a [`crate::file::plugin_bridge::PluginBridge`]
    /// in an `Arc`. The reducer reads this on `HoverEntry` to decide
    /// whether to spawn an async preview task against a plugin.
    pub plugin_bridge: Option<std::sync::Arc<crate::file::plugin_bridge::PluginBridge>>,
    /// Step 28 — clipboard slot. `Some(...)` after the user presses
    /// `y` (copy) or `x` (move); `p` (paste) drives the matching
    /// `Operation::Copy` / `Operation::Move`. Cleared on paste so a
    /// second paste needs a fresh `y` / `x` (the yazi convention the
    /// journey-J5 brief follows). Holds the cwd-relative source paths
    /// so the paste reducer doesn't need a fresh pane snapshot.
    pub clipboard: Option<(ClipboardMode, Vec<std::path::PathBuf>)>,
    /// Step 28 — anchor cursor for `<Shift>+arrow` range selection.
    /// Set when the user first holds Shift; subsequent arrow moves
    /// drive `SelectionSet::add_range(anchor, cursor)`. Cleared when
    /// Shift releases or the user toggles individually with Space.
    pub range_anchor: Option<EntryId>,
    /// Step 29 — wayland `wl_data_device` drag-source state.
    /// `Some(DragSource)` while the user holds a drag in flight (from
    /// `Message::DragStart` until the drop / cancel signal); `None`
    /// otherwise. Lives behind `gui-iced` because the type is defined
    /// in [`crate::file::dnd`], which itself is `gui-iced`-gated.
    #[cfg(feature = "gui-iced")]
    pub drag_source: Option<crate::file::dnd::DragSource>,
    /// Step 30 — `:k <query>` integration with `sy-knowledge.service`
    /// (SPEC §3.3 item 10). The reducer writes
    /// `KnowledgeState::status` from the
    /// [`crate::file::app::Message::KnowledgeStatusChanged`] arm and
    /// `last_hits` from the [`crate::file::app::Message::KnowledgeHits`]
    /// arm; the statusbar chip + commandbar `:index .` hint both
    /// read from here. Not `gui-iced`-gated because the slice is
    /// pure data and the headless harness reads it during the
    /// journey-J4 reducer trace.
    pub knowledge: KnowledgeState,
    /// Step 31 — `b<key>` bookmarks + `recently-used.xbel` log
    /// (SPEC §3.3 item 15). `None` for headless / unit-test contexts
    /// that don't need an on-disk bookmark store; `Some(...)` in the
    /// production `app::run` after [`crate::file::bookmarks::load`]
    /// resolves `$XDG_STATE_HOME/sy/file/` + `$XDG_DATA_HOME/`. The
    /// reducer's `BookmarkPin` / `BookmarkJump` arms lock and call
    /// into the registry.
    pub bookmarks: Option<std::sync::Arc<std::sync::Mutex<crate::file::bookmarks::Bookmarks>>>,
    /// Step 31 — the two-key `b<key>` chord state. `Some('b')` after
    /// the operator presses `b`; the next character keypress drives
    /// the `BookmarkPin` (write) or `BookmarkJump` (read) reducer arm
    /// and clears the chord. Pure data — Escape clears it without
    /// firing a chord arm.
    pub pending_key_chord: Option<char>,
    /// Step 32 (SPEC §3.3 item 14) — mountinfo snapshot. Populated by
    /// the [`crate::file::app::Message::MountsLoaded`] arm after the
    /// async [`crate::file::fs::mounts::load`] task resolves; the
    /// 3-pane sidebar paints it and the `:m` palette filters against
    /// it. Pure data so headless unit tests can plant a fixture
    /// without touching `/proc`.
    pub mounts: Vec<crate::file::fs::mounts::Mount>,
    /// Step 34 (SPEC §3.3 item 17 + item 18) — live keymap loaded from
    /// `$XDG_CONFIG_HOME/sy/file-keymap.toml`. The daemon's SIGHUP
    /// handler swaps this slot in place on receipt so the operator's
    /// edits land without a restart. Defaults to the yazi-shaped
    /// built-ins ([`crate::file::keymap::KeymapConfig::defaults`])
    /// when no override exists.
    pub keymap: crate::file::keymap::KeymapConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Step 13's smoke test: a fresh `State::default()` is
    /// constructable. Step 14 made the type non-marker but kept the
    /// `Default` derive so the test (and every Step 14+ call site that
    /// does `State::default()`) remains green.
    #[test]
    fn state_marker_is_constructable() {
        let s = State::default();
        // Step 14 invariants: the fields are reachable from outside the
        // module and the empty defaults are observable.
        assert!(s.selection.is_empty(), "fresh selection must be empty");
        assert!(s.ops.is_empty(), "fresh ops queue must be empty");
        assert_eq!(s.mode, LayoutMode::ThreePane, "default layout is 3-pane");
    }
}
