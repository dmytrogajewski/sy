//! Command-bar slice of the [`super::State`]. Roadmap Step 25
//! (SPEC §3.3 item 4 + item 7).
//!
//! The command bar has two open modes:
//!
//! * [`CommandMode::Filter`] — opened by `/`; the typed `query`
//!   narrows the current pane's entries via
//!   [`crate::file::search::filename::matches`]. The journey-J7
//!   "live filter" beat reads this.
//! * [`CommandMode::Palette`] — opened by `:`; the typed `query`
//!   prefix-matches a fixed verb table ([`KNOWN_VERBS`]) and the user
//!   picks one with Tab/Enter. The journey-J4 `:k <query>` affordance
//!   rides on the `k` verb landing here today even though the
//!   knowledge backend doesn't ship until Step 30.
//!
//! [`CommandMode::Closed`] is the bar's default — no chrome painted,
//! reducer ignores typed characters.

/// Discriminator: which mode the command bar sits in. The view-layer
/// reads this off [`CommandBar::mode`] to decide whether to paint the
/// bar (and which prompt to show).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandMode {
    /// Bar is hidden. The journey-J2 default state.
    #[default]
    Closed,
    /// `/` filter — the typed query narrows the current pane live.
    Filter,
    /// `:` palette — the typed query prefix-matches a verb table.
    Palette,
}

/// SPEC §3.3 verb table. The roadmap pins this exact list:
///
/// * `k` — `:k <query>` knowledge search (SPEC §3.3 item 7,
///   journey-J4; backend lands in Step 30).
/// * `copy` / `move` / `trash` / `restore` / `mkdir` / `rename` —
///   file operations enumerated in SPEC §3.3 item 5.
/// * `find` — recursive name search (SPEC §3.3 item 7 cross-tree).
/// * `m` — mounts palette (SPEC §3.3 item 14).
///
/// Kept as a `&'static [&'static str]` so the completion path doesn't
/// allocate and the Step 25 unit test can pin the exact wire shape.
pub const KNOWN_VERBS: &[&str] = &[
    "k", "copy", "move", "trash", "restore", "mkdir", "rename", "find", "m",
];

/// Return the verbs whose name starts with `query`, in the order they
/// appear in [`KNOWN_VERBS`]. Used by the Step 25 view layer to render
/// the completion list under the command bar; the Step 25 unit test
/// `tab_completion_offers_known_verbs` reads this directly so the
/// assertion doesn't need iced introspection.
pub fn completions_for(query: &str) -> Vec<String> {
    KNOWN_VERBS
        .iter()
        .filter(|v| v.starts_with(query))
        .map(|v| (*v).to_owned())
        .collect()
}

/// `:k <q>` palette-prefix discriminator (Step 30). Pulled out as a
/// pure helper here (rather than in `view/commandbar.rs`) so the
/// integration-test crate can read the same predicate without
/// pulling iced in. The view layer + `app::update` reducer both
/// route through this fn.
pub const KNOWLEDGE_VERB_PREFIX: &str = "k ";

/// Whether `palette_query` is a `:k <body>` form. The reducer's
/// `Enter`-in-palette arm reads this to decide whether to fire
/// `Message::KnowledgeQuery(body)` vs. `Message::CommandClose`.
pub fn is_knowledge_query(palette_query: &str) -> bool {
    palette_query.starts_with(KNOWLEDGE_VERB_PREFIX)
}

/// Extract the `<query>` body from a `"k <query>"` palette string.
/// Returns an empty string if the prefix doesn't match. Trims
/// surrounding whitespace so `"k   spaced   "` → `"spaced"`.
pub fn knowledge_query_body(palette_query: &str) -> &str {
    palette_query
        .strip_prefix(KNOWLEDGE_VERB_PREFIX)
        .unwrap_or("")
        .trim()
}

/// Step 32 (SPEC §3.3 item 14) — mounts-palette predicate. Returns
/// `true` when the operator typed `m` (bare) or `m <filter>` in the
/// palette; the view layer reads this off [`CommandBar::query`] (in
/// `Palette` mode) to switch the completion list for a mounts
/// overlay. Same shape contract as [`is_knowledge_query`] so the e2e
/// can pin both verbs against one introspection surface.
pub const MOUNTS_VERB: &str = "m";

/// Whether the palette is currently in mounts-overlay mode. The
/// view layer + the `:m` overlay both read this predicate.
pub fn is_mounts_query(palette_query: &str) -> bool {
    palette_query == MOUNTS_VERB || palette_query.starts_with("m ")
}

/// Extract the optional filter body from a `"m <filter>"` palette
/// query. Returns an empty string when the operator typed bare `"m"`
/// (no space yet), or trimmed body otherwise. The mounts overlay
/// reads this against [`crate::file::fs::mounts::Mount::mount_point`]
/// to narrow the list.
pub fn mounts_filter_body(palette_query: &str) -> &str {
    if palette_query == MOUNTS_VERB {
        return "";
    }
    palette_query.strip_prefix("m ").unwrap_or("").trim()
}

/// Command-bar state slice. Owned by [`super::State`]. The reducer in
/// `crate::file::app::update` is the only mutation path; the view
/// layer ([`crate::file::view::commandbar`]) reads it read-only.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CommandBar {
    /// Active mode (Closed by default).
    pub mode: CommandMode,
    /// Typed text. Empty when [`CommandMode::Closed`].
    pub query: String,
    /// Indices into `state.panes.current.entries` matching `query`
    /// (best score first). Only meaningful when
    /// `mode == CommandMode::Filter`; empty otherwise.
    pub filter_results: Vec<usize>,
    /// Currently-highlighted verb in the palette completion list.
    /// `None` when no completion is offered (Closed mode, or no verb
    /// starts with `query`).
    pub selected_verb: Option<String>,
}

impl CommandBar {
    /// Open the bar in `/` filter mode. The reducer calls this on a
    /// `Key::Character("/")` keypress. Clears any stale query so the
    /// user sees an empty input.
    pub fn open_filter(&mut self) {
        self.mode = CommandMode::Filter;
        self.query.clear();
        self.filter_results.clear();
        self.selected_verb = None;
    }

    /// Open the bar in `:` palette mode. The reducer calls this on a
    /// `Key::Character(":")` keypress. Pre-selects the first verb so
    /// the user can press Enter immediately to fire it.
    pub fn open_palette(&mut self) {
        self.mode = CommandMode::Palette;
        self.query.clear();
        self.filter_results.clear();
        self.selected_verb = KNOWN_VERBS.first().map(|v| (*v).to_owned());
    }

    /// Close the bar — reducer dispatches this on Escape.
    pub fn close(&mut self) {
        self.mode = CommandMode::Closed;
        self.query.clear();
        self.filter_results.clear();
        self.selected_verb = None;
    }

    /// Whether the bar is currently visible. The view layer reads
    /// this to decide whether to paint the `text_input` row.
    pub fn is_open(&self) -> bool {
        self.mode != CommandMode::Closed
    }

    /// Update the typed query. The reducer recomputes
    /// [`Self::filter_results`] (in Filter mode) or
    /// [`Self::selected_verb`] (in Palette mode) by calling this from
    /// the `Message::CommandQueryChanged` arm.
    pub fn set_query(&mut self, q: String) {
        self.query = q;
        if self.mode == CommandMode::Palette {
            // Auto-select the first verb whose name starts with the
            // new query so the user can Enter immediately.
            self.selected_verb = completions_for(&self.query).first().cloned();
        }
    }

    /// Pin the highlighted verb (Tab / arrow keys land here in a
    /// future step). Step 25 only needs the field reachable so the
    /// completion test can assert against it.
    pub fn select_verb(&mut self, verb: String) {
        self.selected_verb = Some(verb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `KNOWN_VERBS` is the wire shape the journey-J4 `:k` affordance
    /// rides on. Pinning the table here defends against silent
    /// re-ordering — the prompt list paints in the same order.
    #[test]
    fn known_verbs_table_starts_with_k() {
        assert_eq!(KNOWN_VERBS.first().copied(), Some("k"));
    }

    /// `completions_for("k")` returns the `k` verb at the head — the
    /// Step 25 commandbar test reads this. Pulling the assertion here
    /// pins the prefix-match contract independent of the view layer.
    #[test]
    fn completions_for_k_includes_k_first() {
        let comps = completions_for("k");
        assert!(!comps.is_empty(), "k prefix must match at least 'k'");
        assert_eq!(comps[0], "k", "k must rank first under the 'k' prefix");
    }

    /// `open_palette` pre-selects the first verb so the user can
    /// Enter immediately — the journey-J4 `:k` keystroke ladder is
    /// `:` → `k` → Enter, which only works if `selected_verb` is
    /// already populated when the bar opens.
    #[test]
    fn open_palette_preselects_first_verb() {
        let mut bar = CommandBar::default();
        bar.open_palette();
        assert_eq!(bar.mode, CommandMode::Palette);
        assert_eq!(bar.selected_verb.as_deref(), Some("k"));
    }

    /// Step 30 — `is_knowledge_query` returns true iff the palette
    /// query starts with `"k "` (verb-then-space). The reducer arm
    /// rides on this for the `Enter` → `KnowledgeQuery` dispatch.
    #[test]
    fn is_knowledge_query_requires_verb_then_space() {
        assert!(is_knowledge_query("k tuned override"));
        assert!(is_knowledge_query("k "));
        // bare verb without space is the completion-select case; the
        // reducer's `Enter` arm collapses to `CommandClose`.
        assert!(!is_knowledge_query("k"));
        assert!(!is_knowledge_query("copy foo"));
        assert!(!is_knowledge_query(""));
    }

    /// Step 30 — `knowledge_query_body` strips the `"k "` prefix and
    /// trims surrounding whitespace. The reducer dispatches the
    /// trimmed body as the qdrant query string.
    #[test]
    fn knowledge_query_body_strips_prefix() {
        assert_eq!(knowledge_query_body("k tuned override"), "tuned override");
        assert_eq!(knowledge_query_body("k   spaced   "), "spaced");
        assert_eq!(knowledge_query_body("k "), "");
        // No prefix → empty (the reducer never reaches this arm but
        // pin the contract anyway).
        assert_eq!(knowledge_query_body("copy"), "");
    }

    /// Step 32 — `is_mounts_query` matches the bare `m` verb and the
    /// `m <filter>` prefix. The view layer's mounts-overlay arm reads
    /// this off `state.commandbar.query` (in `Palette` mode).
    #[test]
    fn is_mounts_query_matches_bare_verb_and_prefix() {
        assert!(is_mounts_query("m"));
        assert!(is_mounts_query("m home"));
        assert!(is_mounts_query("m /"));
        assert!(!is_mounts_query("mkdir"));
        assert!(!is_mounts_query("move"));
        assert!(!is_mounts_query(""));
    }

    /// Step 32 — `mounts_filter_body` strips the `"m "` prefix and
    /// trims. Bare `"m"` → empty filter (paint all mounts).
    #[test]
    fn mounts_filter_body_strips_prefix() {
        assert_eq!(mounts_filter_body("m"), "");
        assert_eq!(mounts_filter_body("m home"), "home");
        assert_eq!(mounts_filter_body("m   spaced   "), "spaced");
    }

    /// `close` returns the bar to the default state — verifies the
    /// journey-J2 "Escape clears the bar" reducer contract.
    #[test]
    fn close_resets_to_default() {
        let mut bar = CommandBar::default();
        bar.open_filter();
        bar.set_query("Cargo".to_owned());
        bar.close();
        assert_eq!(bar.mode, CommandMode::Closed);
        assert!(bar.query.is_empty());
        assert!(bar.filter_results.is_empty());
        assert!(bar.selected_verb.is_none());
        assert!(!bar.is_open());
    }
}
