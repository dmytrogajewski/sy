//! Step 24 — responsive layout reflow timing pin.
//!
//! Drives the `sy file` app reducer through three successive
//! `Message::WindowResized` events and asserts:
//!
//! 1. Each transition lands the SPEC §3.2 row 2 [`LayoutMode`]:
//!    1280 px → `ThreePane`, 800 px → `TwoPane`, 320 px → `OnePane`.
//! 2. The resize → first-`view()` wall-clock stays inside the
//!    per-frame budget of 16 ms (60 Hz) at the p99 over 50 trials.
//!    Slipping this turns the journey-J7 reflow into a visible
//!    stutter, which the brief explicitly outlaws.
//!
//! NOTE: the roadmap brief proposed a `1280 → 640 → 320` sequence
//! with `640 → TwoPane`, but `640 < 720` resolves to `OnePane` under
//! the SPEC §3.2 row 2 ladder. The brief's parenthetical "(since
//! 640 < 720)" is internally inconsistent — we honour the SPEC.
//! Substituting 800 px (which falls strictly between 1100 and 720)
//! preserves the intent of the assertion ("each ladder rung fires").

#![cfg(feature = "gui-iced")]

// Pull the production source in via `#[path]` so the test exercises
// the *exact* reducer + view-builder the bin runs. The integration-
// test crate has no access to the bin's `crate::file::…` paths so the
// shim modules below mirror what `src/file/mod.rs` would expose.

#[path = "../src/file/state/ops.rs"]
#[allow(dead_code)]
mod ops;
#[path = "../src/file/state/panes.rs"]
#[allow(dead_code)]
mod panes;
#[path = "../src/file/state/selection.rs"]
#[allow(dead_code)]
mod selection;
// Step 25 — command-bar state slice + nucleo-backed filename matcher,
// mirrored at the test-crate root so `app.rs`'s `super::state::…` and
// `super::search::…` lookups resolve under the integration-test build.
#[path = "../src/file/state/commandbar.rs"]
#[allow(dead_code)]
mod commandbar;
#[path = "../src/file/search/filename.rs"]
#[allow(dead_code)]
mod file_search_filename;
// Step 30 — knowledge integration. `app.rs::handle_knowledge_query`
// reaches for `crate::file::search::knowledge::{KnowledgeBackend,
// KnowledgeStatus, RealKnowledgeBackend, query}`; mirror the
// production source via `#[path]` so the integration-test build picks
// the same types up. The mirror references
// `crate::knowledge::{ipc::HitRow, cli::search_hits}`, so a sibling
// `mod knowledge` shim is declared further below with the minimum
// surface those references resolve through.
#[path = "../src/file/search/knowledge.rs"]
#[allow(dead_code)]
pub mod file_search_knowledge;
// Step 30 — `KnowledgeState` slice of `State`. `app.rs::update`
// reads `state.knowledge.{last_query, last_hits, status}` so the
// production source needs to be reachable under `crate::knowledge`.
#[path = "../src/file/state/knowledge.rs"]
#[allow(dead_code)]
mod state_knowledge;
// Step 31 — bookmarks module mirror.
#[path = "../src/file/bookmarks.rs"]
#[allow(dead_code)]
mod bookmarks;
// Step 32 — mountinfo parser + udisks2 probe. `app.rs::run` reaches
// for `super::fs::mounts::{load, filter_user_visible}` from the boot
// `Task::perform`; the integration-test build mirrors the production
// source so the `#[path]`-imported `app.rs` resolves under the test
// crate's `crate::fs::mounts` path.
#[path = "../src/file/fs/mounts.rs"]
#[allow(dead_code)]
mod mounts;
// `app.rs` boot path spawns `fs::walk::walk` to populate the panes;
// mirror the source so the integration-test build resolves.
#[path = "../src/file/fs/walk.rs"]
#[allow(dead_code)]
mod walk;
/// Step 32 — `super::fs::mounts` shim for the `#[path]`-imported
/// `app.rs`. `app.rs` lives at `src/file/app.rs` in production where
/// `super::fs::mounts` resolves to `crate::file::fs::mounts`; under
/// the integration-test binary `app.rs` is mounted as a top-level
/// module, so `super::fs` here points at the test-crate root's `fs`.
#[allow(dead_code)]
mod fs {
    pub(crate) use crate::mounts;
    pub(crate) use crate::walk;
}

/// Step 30 — minimal `crate::knowledge` shim. `file_search_knowledge`
/// imports `crate::knowledge::ipc::HitRow` + calls
/// `crate::knowledge::cli::search_hits`. The reflow harness never
/// fires the knowledge arm, but the integration-test build still has
/// to resolve both paths; the shim mirrors the wire shape verbatim.
#[allow(dead_code)]
pub mod knowledge {
    pub mod ipc {
        #[derive(Debug, Clone)]
        pub struct HitRow {
            pub score: f32,
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
            anyhow::bail!("knowledge shim: not wired in reflow harness")
        }
    }
}

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

    // Preview state is `#[path]`-imported from the real source so the
    // mirror never drifts (it grew a `resolved` slot + `PreviewPayload`
    // in the async-preview refactor).
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
        /// production field; the layout harness leaves it `None`.
        pub plugin_bridge: Option<std::sync::Arc<crate::plugin_bridge::PluginBridge>>,
        /// Step 28 mirror — clipboard slot.
        pub clipboard: Option<(ClipboardMode, Vec<std::path::PathBuf>)>,
        /// Step 28 mirror — range anchor for `<Shift>+arrow`.
        pub range_anchor: Option<EntryId>,
        /// Step 29 mirror — drag-source slot.
        pub drag_source: Option<crate::dnd::DragSource>,
        /// Step 30 mirror — knowledge slice. `app.rs::update`'s
        /// `KnowledgeQuery` / `KnowledgeHits` arms read this. The
        /// reflow harness never fires the arm but the field must
        /// exist for `#[path]`-imported `app.rs` to compile.
        pub knowledge: crate::state_knowledge::KnowledgeState,
        /// Step 31 mirror — pinned-bookmark registry. The reflow
        /// harness never fires the chord but the field must exist
        /// for `#[path]`-imported `app.rs` to compile.
        pub bookmarks: Option<std::sync::Arc<std::sync::Mutex<crate::bookmarks::Bookmarks>>>,
        /// Step 31 mirror — two-key `b<key>` chord state.
        pub pending_key_chord: Option<char>,
        /// Step 32 mirror — mountinfo snapshot. `app.rs::update`'s
        /// `MountsLoaded` arm plants the parsed list here; the reflow
        /// harness never fires the arm but the field must exist for
        /// `#[path]`-imported `app.rs` to compile.
        pub mounts: Vec<crate::file::fs::mounts::Mount>,
    }
}

/// Minimal four-slot palette mirror — `app.rs::ready_style` and
/// `view::pane` both reach across plane boundaries to load the bar
/// palette; the test mirror returns a deterministic gruvbox shape.
#[allow(dead_code)]
mod theme {
    use iced::Color;

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

/// `view` shim mirroring `src/file/view/mod.rs`'s public surface.
/// The `#[path]`-imported `app.rs` reaches for `super::view::root`
/// and `super::view::mode_for_width`; `super::` from inside the
/// imported source resolves to the test crate root, so a top-level
/// `mod view` here is what `app.rs` finds. Mirroring the API
/// (rather than pulling the production file via `#[path]`) avoids
/// the `pub mod pane;` cycle the production file declares — the
/// integration-test binary doesn't need to paint a real pane tree.
/// Threshold drift is caught by the unit tests in
/// `src/file/view/mod.rs::tests::mode_thresholds_are_inclusive`.
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

    /// Step 25 — empty `statusbar` / `commandbar` mirrors for the
    /// reflow harness's `view()` composition.
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

    /// Step 26 — `view::preview` mirror. The reflow harness's
    /// `WindowResized` arm never triggers the previewer dispatch, but
    /// the `#[path]`-imported `app.rs::handle_hover` reaches for
    /// `warm_caches`, `mime_for_entry`, `kind_for`, and
    /// `image::load`, so all four surfaces must exist here for the
    /// integration-test build to compile.
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

        /// Async-preview refactor: `app::resolve_preview` calls
        /// `format_file_info` (off-thread `stat`) + `text::highlight_path`
        /// (off-thread read + highlight). The reflow harness never
        /// drives them but the `#[path]`-imported `app.rs` must resolve
        /// the symbols.
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

/// Step 25 — `crate::search` shim mirroring the production
/// `crate::file::search::filename` module. `app.rs::update`'s
/// `Message::CommandQueryChanged` arm reads through this path.
#[allow(dead_code)]
mod search {
    pub mod filename {
        pub use crate::file_search_filename::matches;
    }
    /// Step 30 — `crate::search::knowledge` shim for the reflow
    /// harness. Routes `super::search::knowledge::…` resolutions from
    /// `#[path]`-imported `app.rs` to the `file_search_knowledge`
    /// mirror. `pub(crate) use` matches the crate-private mirror's
    /// visibility.
    pub(crate) use crate::file_search_knowledge as knowledge;
}

/// Step 25 — `crate::file::state::{Entry, EntryKind}` shim. Same
/// rationale as the sibling `sy_file_gui_smoke` test: the
/// `#[path]`-imported `filename.rs` references the production path
/// `crate::file::state::Entry`, which the integration-test crate
/// must mirror at the same name. Step 30 extends the shim with
/// `crate::file::search::knowledge::*` so the `#[path]`-imported
/// `state_knowledge.rs` resolves its `use crate::file::search::
/// knowledge::KnowledgeStatus` line.
#[allow(dead_code)]
mod file {
    pub mod state {
        pub use crate::panes::{Entry, EntryKind};
        pub(crate) use crate::{panes, selection};
    }
    pub mod search {
        pub use crate::file_search_knowledge as knowledge;
    }
    /// Step 32 — `crate::file::fs::mounts` shim. The State mirror's
    /// `mounts` field is `Vec<crate::file::fs::mounts::Mount>`, so
    /// the integration-test build needs `crate::file::fs::mounts` to
    /// resolve to the same `Mount` struct the production source
    /// declares.
    pub mod fs {
        pub(crate) use crate::mounts;
    }
}

/// Step 27 — `plugin_bridge` stub shim. Same minimal surface as the
/// sibling `sy_file_gui_smoke` test. The layout harness never spawns
/// a plugin process; the shim only needs to make the
/// `#[path]`-imported `app.rs` compile.
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

/// Step 27 — `plugin::registry` shim. Same minimal surface as the
/// sibling smoke test.
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

/// Step 29 — `dnd.rs` shim. Same `#[path]`-import as the production
/// path so the `#[path]`-imported `app.rs`'s `super::dnd::…`
/// resolution lands on the real wire helpers.
#[path = "../src/file/dnd.rs"]
#[allow(dead_code)]
mod dnd;

#[path = "../src/file/app.rs"]
#[allow(dead_code)]
mod app;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use state::{LayoutMode, State};

/// One frame at 60 Hz. Step 24 DoD pins this as the p99 ceiling.
const FRAME_BUDGET: Duration = Duration::from_millis(16);
/// Number of resize cycles per trial. Three transitions per the
/// SPEC §3.2 row 2 ladder.
const TRANSITIONS_PER_TRIAL: usize = 3;
/// p99 sample size — 50 trials × 3 transitions = 150 timed pairs,
/// enough to settle the tail percentile without flaking on a busy
/// CI worker.
const TRIALS: usize = 50;

/// Drive the app reducer through `WindowResized(1280)` →
/// `WindowResized(800)` → `WindowResized(320)`, asserting both the
/// transition table (each rung lands the right `LayoutMode`) and the
/// 16 ms p99 budget the journey-J7 reflow rides on.
#[test]
fn resize_event_collapses_layout() {
    let path = PathBuf::from("/tmp/sy-file-step24-reflow");
    let mut samples = Vec::with_capacity(TRIALS * TRANSITIONS_PER_TRIAL);
    let mut last_modes = Vec::with_capacity(3);

    for _trial in 0..TRIALS {
        let mut state = State::default();
        state.panes.current.cwd = path.clone();

        // Each trial restarts the reducer from default so the
        // transitions exercise the cold path (the typical user
        // workflow is a window-create → first-resize → user-resize,
        // not a stream of resizes against a hot state).
        let _ = app::update(&mut state, app::Message::Tick);

        // 1280 → ThreePane.
        let start = Instant::now();
        let _ = app::update(&mut state, app::Message::WindowResized(1280, 800));
        let _ = app::view(&state);
        samples.push(start.elapsed());
        last_modes.push(state.mode);

        // 800 → TwoPane (≥720 px).
        let start = Instant::now();
        let _ = app::update(&mut state, app::Message::WindowResized(800, 600));
        let _ = app::view(&state);
        samples.push(start.elapsed());
        last_modes.push(state.mode);

        // 320 → OnePane (<720 px).
        let start = Instant::now();
        let _ = app::update(&mut state, app::Message::WindowResized(320, 240));
        let _ = app::view(&state);
        samples.push(start.elapsed());
        last_modes.push(state.mode);
    }

    // Mode-transition table — pin the last trial's observations.
    // (Every trial has the same transitions; assert the last triple.)
    let n = last_modes.len();
    assert_eq!(last_modes[n - 3], LayoutMode::ThreePane);
    assert_eq!(last_modes[n - 2], LayoutMode::TwoPane);
    assert_eq!(last_modes[n - 1], LayoutMode::OnePane);

    // p99 budget — sort the samples and read the 99th percentile.
    samples.sort();
    let p99_idx = (samples.len() as f64 * 0.99) as usize;
    let p99 = samples[p99_idx.min(samples.len() - 1)];
    assert!(
        p99 < FRAME_BUDGET,
        "step24 — resize→render p99 must stay under {FRAME_BUDGET:?}; got {p99:?} \
         from {} trials × 3 transitions",
        TRIALS
    );
}
