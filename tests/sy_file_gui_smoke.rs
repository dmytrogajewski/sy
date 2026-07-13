//! Step 23 unit-shape smoke for the `sy file` iced xdg-toplevel.
//!
//! Drives [`crate::file::app::run_headless_once`] without standing up
//! a winit/wgpu surface. The DoD bullet "test passes on a headless CI
//! worker" rides on this — the harness exercises the literal
//! `boot → update(Tick) → view()` lifecycle the real runtime would
//! step through between `WindowEvent::Created` and the first
//! `RedrawRequested`, but uses no display server, so a CI worker with
//! no compositor still observes a first paint.
//!
//! The journey-J1 latency assertion (250 ms wall-clock) lives in
//! [`tests/sy_file_journey_e2e.rs::step23_gui_paints_first_frame_under_250ms`]
//! — this file pins the *behavioural* contract (the harness returns
//! Ok, the reducer fires at least once) so a regression in the boot
//! path surfaces before the timing budget is checked.

// `sy file`'s `app.rs` lives behind the `gui-iced` feature; the whole
// file is gated so `cargo test --no-default-features` still builds.
#![cfg(feature = "gui-iced")]

// Pull the production source in via `#[path]` so we exercise the
// *exact* function the bin's `cli::run_scaffold` dispatches to. The
// e2e binary uses the same idiom; see its header comment for the
// rationale (Rust integration tests can't import the bin's
// `crate::…` paths because the bin has no `lib.rs`).
//
// app.rs `use`s `super::state::State`, so the `#[path]`-imported file
// looks for `state` as a sibling of itself. We mirror the `src/file/`
// layout by declaring `state` as a top-level module of the test
// binary and re-exporting from a `file::` shim that the app module's
// `super::state` resolves through.

// `app.rs` references `super::state::State` and
// `super::theme::iced_theme()`. The integration-test binary's `super::`
// (when reached from inside the `#[path]`-imported app source) lands
// at the crate root, so we declare `state` and `theme` modules at the
// crate-root level with the shapes the app reads.

#[path = "../src/file/state/ops.rs"]
#[allow(dead_code)]
mod ops;
#[path = "../src/file/state/panes.rs"]
#[allow(dead_code)]
mod panes;
#[path = "../src/file/state/selection.rs"]
#[allow(dead_code)]
mod selection;
// Step 25 — command-bar state slice. `app.rs` reaches for
// `super::state::CommandMode` and `state.commandbar.*` from its
// reducer arms; the smoke test mirrors the same surface so the
// integration-test binary's `super::state::…` resolution lands at
// the test-crate root the same way as in production.
#[path = "../src/file/state/commandbar.rs"]
#[allow(dead_code)]
mod commandbar;
// Step 25 — `nucleo`-backed fuzzy filename matcher referenced from
// `app.rs::update`'s `Message::CommandQueryChanged` arm.
#[path = "../src/file/search/filename.rs"]
#[allow(dead_code)]
mod file_search_filename;
// Step 30 — knowledge search module. `app.rs` reaches for
// `super::search::knowledge::{KnowledgeBackend, KnowledgeStatus,
// RealKnowledgeBackend, query}`. Mirror the production source so the
// integration-test build resolves the same types. The mirror itself
// imports `crate::knowledge::{ipc::HitRow, cli::search_hits}`; the
// `mod knowledge` shim below provides the minimal surface.
#[path = "../src/file/search/knowledge.rs"]
#[allow(dead_code)]
pub mod file_search_knowledge;
// Step 30 — `KnowledgeState` slice. `app.rs::update` reads
// `state.knowledge.*` from the `KnowledgeQuery` / `KnowledgeHits` arms.
#[path = "../src/file/state/knowledge.rs"]
#[allow(dead_code)]
mod state_knowledge;
// Step 31 — bookmarks + `recently-used.xbel`. `app.rs::update`'s
// `BookmarkPin` / `BookmarkJump` arms lock `state.bookmarks`.
#[path = "../src/file/bookmarks.rs"]
#[allow(dead_code)]
mod bookmarks;
// Step 32 — mountinfo parser + udisks2 probe. `app.rs::run`'s boot
// `Task::perform` reaches for `super::fs::mounts::{load,
// filter_user_visible}`; mirror the production source so the
// integration-test build resolves.
#[path = "../src/file/fs/mounts.rs"]
#[allow(dead_code)]
mod mounts;
// `app.rs` boot path spawns `fs::walk::walk` to populate the panes;
// mirror the source so the integration-test build resolves.
#[path = "../src/file/fs/walk.rs"]
#[allow(dead_code)]
mod walk;
/// Step 32 — `super::fs::mounts` shim for the `#[path]`-imported
/// `app.rs`. See the analogous shim in `sy_file_layout_reflow.rs`.
#[allow(dead_code)]
mod fs {
    pub(crate) use crate::mounts;
    pub(crate) use crate::walk;
}

/// Step 30 — minimal `crate::knowledge` shim. The smoke harness never
/// fires the knowledge arm; the shim makes the `#[path]`-imported
/// `file_search_knowledge` resolve its `crate::knowledge::ipc::HitRow`
/// + `crate::knowledge::cli::search_hits` references at compile time.
#[allow(dead_code)]
pub mod knowledge {
    pub mod ipc {
        #[derive(Debug, Clone)]
        pub struct HitRow {
            pub score: f32,
            pub chunk_id: String,
            pub file_path: String,
            pub chunk_index: u32,
            pub chunk_text: String,
            pub embed_score: Option<f32>,
        }
    }
    pub mod cli {
        use super::ipc::HitRow;
        use anyhow::Result;
        pub fn search_hits(_q: &str, _k: usize, _prefix: Option<&str>) -> Result<Vec<HitRow>> {
            anyhow::bail!("knowledge shim: not wired in smoke harness")
        }
    }
}

/// Synthetic mirror of `src/file/state/mod.rs` for the
/// integration-test binary. The bin's `state/mod.rs` declares
/// `pub mod ops; pub mod panes; pub mod selection;` — pulling that
/// file directly would re-declare the same `#[path]`-imported
/// submodules above and trigger a duplicate-mod compile error.
/// Mirroring `State` + `LayoutMode` inline preserves the shape
/// `app.rs` reads (`state.panes.current.cwd`) without that clash.
///
/// Step 25 grew `State` with a `commandbar: CommandBar` field — the
/// shim mirrors that so the headless harness's reducer trace can
/// observe the bar slice the same way as production.
#[path = "../src/file/state/preview.rs"]
#[allow(dead_code)]
mod state_preview;

#[allow(dead_code, unused_imports)]
mod state {
    pub use super::commandbar::{CommandBar, CommandMode};
    pub use super::ops::{ConflictPolicy, OpEvent, Operation};
    pub use super::panes::{Entry, EntryKind, Pane, PaneId, Panes};
    pub use super::selection::{EntryId, SelectionSet};

    #[allow(clippy::enum_variant_names)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum LayoutMode {
        #[default]
        ThreePane,
        TwoPane,
        OnePane,
    }

    // Preview state `#[path]`-imported from source so the async-preview
    // `resolved` slot + `PreviewPayload` stay in sync automatically.
    pub use super::state_preview::{
        HighlightedLine, HighlightedSpan, PreviewPayload, PreviewState,
    };

    /// Step 28 — clipboard mode mirror.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ClipboardMode {
        Copy,
        Move,
    }

    #[derive(Debug, Default)]
    pub struct State {
        pub panes: Panes,
        pub mode: LayoutMode,
        pub selection: SelectionSet,
        pub ops: Vec<Operation>,
        pub commandbar: CommandBar,
        pub preview: PreviewState,
        /// Step 27 — plugin-routed previewer bridge. Mirrors the
        /// production field shape; the smoke harness leaves it
        /// `None`.
        pub plugin_bridge: Option<std::sync::Arc<crate::plugin_bridge::PluginBridge>>,
        /// Step 28 mirror — clipboard slot.
        pub clipboard: Option<(ClipboardMode, Vec<std::path::PathBuf>)>,
        /// Step 28 mirror — range anchor for `<Shift>+arrow`.
        pub range_anchor: Option<EntryId>,
        /// Step 29 mirror — drag-source slot. Production lives on
        /// `src/file/state/mod.rs::State::drag_source`.
        pub drag_source: Option<crate::dnd::DragSource>,
        /// Step 30 mirror — knowledge slice. `app.rs::update`'s
        /// `KnowledgeQuery` / `KnowledgeHits` arms read this.
        pub knowledge: crate::state_knowledge::KnowledgeState,
        /// Step 31 mirror — pinned-bookmark registry. `app.rs`'s
        /// `BookmarkPin` / `BookmarkJump` arms lock-and-call into
        /// the registry.
        pub bookmarks: Option<std::sync::Arc<std::sync::Mutex<crate::bookmarks::Bookmarks>>>,
        /// Step 31 mirror — two-key `b<key>` chord state.
        pub pending_key_chord: Option<char>,
        /// Step 32 mirror — mountinfo snapshot. The smoke harness
        /// leaves this empty; the field exists so `#[path]`-imported
        /// `app.rs` compiles.
        pub mounts: Vec<crate::file::fs::mounts::Mount>,
    }
}

/// Side-shim mirror of `src/file/theme.rs`. The `app.rs` source the
/// `#[path]` import below pulls in references both `Palette` (for the
/// container-style hook) and `iced_theme` (for the app builder),
/// plus `crate::mon::theme::load_or_ink` from inside `ready_style`.
/// We mirror all three here so the integration-test binary doesn't
/// need to drag in `src/mon/theme.rs`.
#[allow(dead_code)]
mod theme {
    use iced::Color;

    /// Minimal mirror of the four-slot `Palette` `app.rs` reads from
    /// in `ready_style`. Only `bg` + `ink` are touched today; the
    /// remaining slots are kept for shape parity with
    /// `src/file/theme.rs`.
    #[derive(Debug, Clone, Copy)]
    pub struct Palette {
        pub bg: Color,
        pub bg2: Color,
        pub accent: Color,
        pub ink: Color,
        pub ok: Color,
        pub warn: Color,
        pub bad: Color,
    }

    pub fn iced_theme() -> iced::Theme {
        // Smoke harness mirrors the production projection: build a
        // `Theme::Custom` from the bar palette so the smoke surface
        // exercises the same code path the live window uses.
        let p = crate::mon::theme::load_or_ink();
        iced::Theme::custom(
            "sy".to_string(),
            iced::theme::Palette {
                background: p.bg,
                text: p.ink,
                primary: p.accent,
                success: p.ok,
                warning: p.warn,
                danger: p.bad,
            },
        )
    }
}

/// `crate::mon::theme::load_or_ink` shim — `app.rs::ready_style`
/// reaches across plane boundaries to load the bar palette. The
/// test-crate mirror returns a hard-coded gruvbox-shaped palette so
/// the headless harness never depends on filesystem state.
#[allow(dead_code)]
mod mon {
    pub mod theme {
        pub fn load_or_ink() -> super::super::theme::Palette {
            use iced::Color;
            let c = |r: u8, g: u8, b: u8| Color {
                r: r as f32 / 255.0,
                g: g as f32 / 255.0,
                b: b as f32 / 255.0,
                a: 1.0,
            };
            super::super::theme::Palette {
                bg: c(40, 40, 40),
                bg2: c(60, 56, 54),
                accent: c(254, 128, 25),
                ink: c(235, 219, 178),
                ok: c(184, 187, 38),
                warn: c(250, 189, 47),
                bad: c(251, 73, 52),
            }
        }
    }
}

/// Step 24 added `super::view::root` + `super::view::mode_for_width`
/// references inside `app.rs`. The smoke test doesn't paint a real
/// pane tree (no compositor) so the shim returns an empty container
/// and a pure-math mode resolver — enough to satisfy the type system
/// while keeping the headless smoke surface unchanged.
#[allow(dead_code)]
mod view {
    use crate::state::LayoutMode;

    pub fn mode_for_width(width_px: u32) -> LayoutMode {
        if width_px >= 1100 {
            LayoutMode::ThreePane
        } else if width_px >= 720 {
            LayoutMode::TwoPane
        } else {
            LayoutMode::OnePane
        }
    }

    pub fn root(_state: &crate::state::State) -> iced::Element<'static, crate::app::Message> {
        iced::widget::container(iced::widget::text("")).into()
    }

    /// Step 25 — empty `statusbar` / `commandbar` mirrors so the
    /// smoke harness's `view()` composition lines up with production
    /// without driving iced's widget tree.
    pub mod statusbar {
        pub fn statusbar(
            _state: &crate::state::State,
        ) -> iced::Element<'static, crate::app::Message> {
            iced::widget::container(iced::widget::text("")).into()
        }
        /// Step 28 — empty `ops_drawer` mirror.
        pub fn ops_drawer(
            _state: &crate::state::State,
        ) -> iced::Element<'static, crate::app::Message> {
            iced::widget::container(iced::widget::text("")).into()
        }
    }
    pub mod commandbar {
        pub fn commandbar(
            _state: &crate::state::State,
        ) -> iced::Element<'static, crate::app::Message> {
            iced::widget::container(iced::widget::text("")).into()
        }
    }

    /// Step 26 — `view::preview` mirror covering the four surfaces
    /// `app.rs` reaches for (`warm_caches`, `mime_for_entry`,
    /// `kind_for`, `image::load`). The smoke harness never triggers
    /// any of them but the `#[path]`-imported source must resolve.
    pub mod preview {
        use std::path::{Path, PathBuf};

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum PreviewKind {
            Image,
            Text,
            NoBuiltin,
        }

        pub fn warm_caches() {}

        pub fn kind_for(mime: &str) -> PreviewKind {
            if mime.starts_with("image/") {
                PreviewKind::Image
            } else if mime.starts_with("text/") || mime == "application/json" {
                PreviewKind::Text
            } else {
                PreviewKind::NoBuiltin
            }
        }

        pub fn mime_for_entry(entry: &crate::state::Entry, _path: &Path) -> String {
            entry
                .mime_hint
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string())
        }

        pub mod image {
            use super::PathBuf;
            use anyhow::Result;

            pub async fn load(path: PathBuf) -> Result<(PathBuf, iced::widget::image::Handle)> {
                let bytes = tokio::fs::read(&path).await?;
                let handle = iced::widget::image::Handle::from_bytes(bytes);
                Ok((path, handle))
            }
        }

        // Async-preview surfaces the `#[path]`-imported `app.rs` reaches.
        pub fn format_file_info(path: &Path) -> String {
            path.display().to_string()
        }

        pub mod text {
            use super::Path;
            pub use crate::state::HighlightedLine;

            pub fn highlight_path(_path: &Path) -> Vec<HighlightedLine> {
                Vec::new()
            }
        }
    }
}

/// Step 25 — `crate::search` shim. Mirrors the production
/// `crate::file::search::filename::matches` so `app.rs::update`'s
/// `Message::CommandQueryChanged` arm resolves under the
/// integration-test binary's `super::search::…` lookup.
#[allow(dead_code)]
mod search {
    pub mod filename {
        pub use crate::file_search_filename::matches;
    }
    /// Step 30 — `crate::search::knowledge` shim for the smoke
    /// harness. Routes `super::search::knowledge::…` resolutions from
    /// inside `#[path]`-imported `app.rs` to the
    /// `file_search_knowledge` mirror.
    pub(crate) use crate::file_search_knowledge as knowledge;
}

/// Step 25 — `crate::file::state::{Entry, EntryKind}` shim. The
/// `#[path]`-imported `filename.rs` does
/// `use crate::file::state::Entry;`; under the integration-test
/// binary `crate::file` is absent, so the shim makes the resolution
/// land on the same `Entry` / `EntryKind` types as production.
#[allow(dead_code)]
mod file {
    pub mod state {
        pub use crate::panes::{Entry, EntryKind};
        pub(crate) use crate::{panes, selection};
    }
    /// Step 30 — `crate::file::search::knowledge` shim. The
    /// `#[path]`-imported `state/knowledge.rs` does
    /// `use crate::file::search::knowledge::KnowledgeStatus;`; route
    /// it to the test-crate's `file_search_knowledge` mirror.
    pub mod search {
        pub use crate::file_search_knowledge as knowledge;
    }
    /// Step 32 — `crate::file::fs::mounts` shim. The State mirror's
    /// `mounts: Vec<crate::file::fs::mounts::Mount>` field rides on
    /// this to find the production `Mount` struct.
    pub mod fs {
        pub(crate) use crate::mounts;
    }
}

/// Step 27 — `plugin_bridge` shim. The smoke harness never spawns a
/// plugin process, but the `#[path]`-imported `app.rs` references the
/// bridge surface in three places (the `HoverEntry` reducer arm's
/// `PreviewResult` match, the `app::run` build-time wiring, and the
/// `shutdown_all` warm-up touch). The shim provides the minimum
/// surface those references need to compile.
#[allow(dead_code)]
mod plugin_bridge {
    use std::path::Path;
    use std::sync::Arc;

    pub enum PreviewResult {
        Png(Vec<u8>),
        Text(String),
    }

    #[derive(Debug)]
    pub struct PluginBridge {
        registry: crate::plugin::registry::Registry,
    }

    impl PluginBridge {
        pub fn registry(&self) -> &crate::plugin::registry::Registry {
            &self.registry
        }
        pub async fn preview_for(
            &self,
            _mime: &str,
            _path: &Path,
        ) -> Result<PreviewResult, BridgeError> {
            Err(BridgeError::NoMatch)
        }
        pub async fn shutdown_all(&self) {}
    }

    pub enum BridgeError {
        NoMatch,
    }
    impl std::fmt::Display for BridgeError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "no plugin bridge")
        }
    }

    pub fn build_with_channels(
        _registry: Arc<crate::plugin::registry::Registry>,
        _theme: serde_json::Value,
    ) -> (
        Arc<PluginBridge>,
        tokio::sync::mpsc::Receiver<()>,
        tokio::sync::mpsc::Receiver<()>,
    ) {
        let (_a, rx_a) = tokio::sync::mpsc::channel(1);
        let (_b, rx_b) = tokio::sync::mpsc::channel(1);
        (
            Arc::new(PluginBridge {
                registry: crate::plugin::registry::Registry,
            }),
            rx_a,
            rx_b,
        )
    }
}

/// Step 27 — `plugin::registry` shim. Only `discover` /
/// `discover_empty` (and the `Registry` marker) are referenced from
/// `app.rs`; both are no-ops here.
#[allow(dead_code)]
mod plugin {
    pub mod registry {
        #[derive(Debug)]
        pub struct Registry;
        impl Registry {
            pub fn plugin_ids(&self) -> std::iter::Empty<&'static str> {
                std::iter::empty()
            }
        }
        pub fn discover() -> anyhow::Result<Registry> {
            Ok(Registry)
        }
        pub fn discover_empty() -> Registry {
            Registry
        }
    }
}

/// Step 29 — `#[path]`-import the real production `dnd.rs` so the
/// `app.rs` reducer arm's `super::dnd::…` resolution lands on the
/// same wire shape the bin uses. Pure-Rust + `iced::keyboard::
/// Modifiers` only — no compositor dependency.
#[path = "../src/file/dnd.rs"]
#[allow(dead_code)]
mod dnd;

#[path = "../src/file/app.rs"]
#[allow(dead_code)]
mod app;

use std::path::PathBuf;
use std::time::Duration;

/// Journey-J1 budget. The roadmap Step 23 brief calls 250 ms the
/// "first paint" wall-clock budget; the smoke test uses a much
/// looser ceiling (one second) because the in-process headless
/// harness is dominated by the integration-test binary's tokio
/// runtime warm-up, not iced. The e2e in `sy_file_journey_e2e.rs`
/// asserts the tight 250 ms budget against the same code path.
const SMOKE_BUDGET: Duration = Duration::from_secs(1);

/// Step 23 DoD bullet "test passes on a headless CI worker" — calls
/// the headless harness, asserts at least one `Message::Tick`
/// reduced, and pins the elapsed wall-clock below a loose smoke
/// budget so a catastrophic regression (e.g. accidental
/// `std::thread::sleep` in the boot path) trips here.
#[test]
fn headless_run_paints_first_frame() {
    let path = PathBuf::from("/tmp/sy-file-gui-smoke");
    let (ticks, elapsed) = app::run_headless_once(path).expect("run_headless_once must succeed");
    assert!(
        ticks >= 1,
        "boot must dispatch at least one Message::Tick, got {ticks}"
    );
    assert!(
        elapsed < SMOKE_BUDGET,
        "headless smoke must finish under {SMOKE_BUDGET:?}, took {elapsed:?}"
    );
}
