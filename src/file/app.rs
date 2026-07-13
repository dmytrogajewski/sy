//! `sy file` xdg-toplevel iced application. Roadmap Steps 23–24.
//!
//! Unlike `sy mon` (which is a layer-shell popup, see
//! [`crate::mon::app`]), `sy file` is a normal xdg-toplevel window —
//! it tiles inside niri, takes focus, and can be Mod+E-spawned like
//! any other GUI. We use plain `iced::application` here, NOT
//! `iced_layershell::application`, exactly so niri can tile us
//! alongside Firefox / Alacritty / wezterm.
//!
//! ## Scope through Step 24 (this module)
//!
//! - Spin up the window (1280×800, gruvbox-dark, title
//!   `sy file — <cwd>`), paint the responsive layout ladder (Step 24), exit
//!   cleanly on close.
//! - Provide a headless harness ([`run_headless_once`]) that exercises
//!   the `boot → update(Tick) → view()` lifecycle without a display
//!   server so the journey-J1 250 ms wall-clock budget can be
//!   asserted on a CI worker that has no compositor.
//! - Reduce [`Message::WindowResized`] events to a [`super::state::LayoutMode`]
//!   transition. The width thresholds (SPEC §3.2 row 2) live in
//!   [`super::view::mode_for_width`]; the reducer is the single point
//!   the `state.mode` ever changes.
//!
//! Step 25 adds the statusbar and command bar. The `Message` enum
//! here grows as those steps land.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use iced::event::{self, Event};
use iced::widget::column;
use iced::{Element, Subscription, Task};

use super::dnd::{
    drop_action_from_modifiers, paths_to_uri_list, DragAction, DragSource, DropAction, DropTarget,
};
use super::search::knowledge::{KnowledgeBackend, KnowledgeStatus, RealKnowledgeBackend};
use super::state::{
    ClipboardMode, CommandMode, ConflictPolicy, Entry, EntryId, Operation, PaneId, State,
};

/// Default window dimensions (logical px). Matches the SPEC §3.2
/// "normal desktop tile" the journey-J1 brief assumes — Step 24's
/// responsive ladder will shrink it through the 1100/720 thresholds.
pub const DEFAULT_WIDTH: f32 = 1280.0;
pub const DEFAULT_HEIGHT: f32 = 800.0;

/// Application messages. Step 23 ships the minimum set:
/// - [`Message::Tick`] — synthetic "first frame painted" trigger.
///   The headless harness fires this synchronously during boot so
///   the e2e can observe a "first paint" without driving a real
///   winit event loop.
/// - [`Message::Loaded`] — the initial-path cwd resolved off the bin
///   argv. Reducer sets `state.panes.current.cwd`.
///
/// Step 24+ extends this enum with the responsive-layout reflow
/// signals; Step 25 adds `OpenCommandBar` / `OpenFilter` etc. The
/// xdg-toplevel close button rides on iced's default
/// `exit_on_close_request` machinery so no explicit `Quit` variant
/// is needed today.
#[derive(Debug, Clone)]
pub enum Message {
    /// First-paint proxy. Step 24+ swaps this for the real
    /// frame-arrival sources (`subscription` on a directory watcher,
    /// IPC frames, etc.). For now it's the headless-harness anchor.
    Tick,
    /// Initial cwd from `sy file [PATH]`. The reducer plants it on
    /// the current pane so the title bar's "sy file — `<cwd>`"
    /// rendering ([`view`]) reflects the launch argument.
    Loaded(PathBuf),
    /// Window dimensions changed (`width, height` in logical pixels).
    /// Step 24's reducer translates the new width into a
    /// [`super::state::LayoutMode`] via [`super::view::mode_for_width`],
    /// collapsing the 3-pane layout down to 2-pane (<1100 px) or
    /// 1-pane (<720 px) per the SPEC §3.2 row 2 ladder. The
    /// journey-J7 reflow beat fires this. Height rides through unused
    /// in Step 24; Step 25's statusbar will use it to clamp the
    /// command bar's row count.
    WindowResized(u32, u32),
    /// Step 25 + Step 28 — keyboard event from iced's subscription.
    /// Carries the modifier mask (`Shift`, `Ctrl`, …) so the Step 28
    /// `<Shift>+ArrowDown` range-select arm can branch on modifier
    /// state without a second event variant. Step 25's existing
    /// `/` / `:` / Escape arms ignore the modifier mask and still
    /// fire.
    KeyPressed(iced::keyboard::Key, iced::keyboard::Modifiers),
    /// Step 25 — typed text in the command bar's `text_input`.
    /// Reducer writes it to `state.commandbar.query` and recomputes
    /// filter results / palette completions.
    CommandQueryChanged(String),
    /// Step 25 — user picked a verb in the palette completion list.
    /// Reducer sets `state.commandbar.selected_verb`.
    CommandSelectVerb(String),
    /// Step 25 — user pressed Escape (or otherwise dismissed the
    /// bar). Reducer calls [`super::state::CommandBar::close`].
    CommandClose,
    /// Step 25 — user clicked a breadcrumb segment. Reducer plants
    /// the path on `state.panes.current.cwd`; Step 19's fs::walk
    /// repopulates `entries` on the next reducer turn.
    Navigate(PathBuf),
    /// Step 26 — cursor moved onto an entry. The reducer writes the
    /// path to `state.preview.current_path` so the dispatcher in
    /// `view::preview::preview` knows which entry to paint, and
    /// (under the journey-J3 budget) spawns an async image-load
    /// `Task` if the entry's MIME routes to the image previewer.
    /// Routing is read from the entry's `mime_hint` (Step 19) — no
    /// fs read happens inside the reducer.
    HoverEntry(EntryId),
    /// Step 26 — async image-decode `Task` completed. The payload
    /// carries the originating path so the reducer can ignore a
    /// stale decode the cursor has already moved past.
    PreviewLoaded {
        path: PathBuf,
        handle: iced::widget::image::Handle,
    },
    /// Step 27 — plugin-routed previewer rendered a PNG. The
    /// `PluginBridge` already decoded the base64 payload so the
    /// reducer holds raw bytes; it constructs an
    /// `iced::widget::image::Handle::from_bytes` and stashes it
    /// alongside the matching `current_path` so the view dispatcher
    /// can paint it.
    PreviewLoadedPng { path: PathBuf, bytes: Vec<u8> },
    /// Step 27 — plugin-routed previewer rendered a text body. The
    /// reducer stores it on `state.preview.text_preview` so the view
    /// layer's text dispatcher can read it back.
    PreviewLoadedText { path: PathBuf, content: String },
    /// Step 27 — plugin preview failed (spawn / handshake / crash
    /// / invalid response). The reducer routes to the built-in text
    /// fallback per the DoD `plugin_crash_falls_back_to_built_in_text`.
    PreviewFailed { path: PathBuf, error: String },
    /// Step 29 (SPEC §3.3 item 12) — user initiated a drag from the
    /// current selection. The reducer builds a [`DragSource`] from
    /// `state.selection` resolved against the current pane's cwd and
    /// stashes it on `state.drag_source`; the wayland subsystem reads
    /// it back to populate the `wl_data_device_source` MIME body.
    DragStart(Vec<EntryId>),
    /// Step 29 — the wayland subsystem reports the drag-source offer
    /// MIME to the receiving Wayland client. Carries the advertised
    /// MIME (always [`crate::file::dnd::URI_LIST_MIME`] today).
    DragOffer(String),
    /// Step 29 — a Wayland client dropped a `text/uri-list` payload
    /// onto our window. The reducer routes the carried [`DropTarget`]
    /// into an `Operation::Copy` or `Operation::Move` against the
    /// current pane's cwd.
    DropAccept(DropTarget),
    /// Step 30 — operator fired `:k <query>` from the palette. Reducer
    /// stamps `state.knowledge.last_query` + spawns the async
    /// [`crate::file::search::knowledge::query`] task against the
    /// current pane's cwd; the resolved hits land via
    /// [`Message::KnowledgeHits`].
    KnowledgeQuery(String),
    /// Step 30 — async `query` task resolved. Carries the
    /// `(path, score)` pairs the reducer merges with the current
    /// pane's filename-rank order and stamps onto
    /// `state.knowledge.last_hits`. Direct callers (test stubs,
    /// future MCP bridge) hit this arm without flipping the chip;
    /// the `:k` palette path goes through
    /// [`Message::KnowledgeQueryResolved`] which flips both halves
    /// in one reducer turn.
    KnowledgeHits(Vec<(PathBuf, f32)>),
    /// Step 30 — `(hits, status)` resolved from the async `query`
    /// task. The reducer plants the merged hit list AND flips the
    /// chip status in one turn so the journey-J4 observer can read
    /// the chip and the hit count atomically (no flicker).
    KnowledgeQueryResolved(Vec<(PathBuf, f32)>, KnowledgeStatus),
    /// Step 30 — daemon reachability changed (e.g. an error response
    /// flipped the status to `Unreachable`). Carrier so the
    /// supervisor / IPC plumbing can poke the chip without going
    /// through the `:k` palette path.
    KnowledgeStatusChanged(KnowledgeStatus),
    /// Step 31 (SPEC §3.3 item 15) — `b<key>` second-keypress arm.
    /// Reducer pins the current pane's cwd under `key` in the
    /// `state.bookmarks` registry (locked) and persists. If the
    /// registry isn't wired (`None`), the arm is a no-op so the
    /// headless harness keeps working without a state dir.
    BookmarkPin(char),
    /// Step 31 — `b<key>` jump arm. Reducer looks up `key`; if a
    /// path is bound, it warps the current pane's cwd to it (the
    /// next reducer turn re-walks via `file.cd`). Unbound keys are
    /// a no-op.
    BookmarkJump(char),
    /// Step 32 (SPEC §3.3 item 14) — async
    /// [`crate::file::fs::mounts::load`] task resolved. Reducer
    /// plants the result on `state.mounts` so the 3-pane sidebar +
    /// the `:m` palette overlay both see the fresh list on the next
    /// paint. Direct callers (test harness, future `inotify` watch
    /// on `/proc/self/mountinfo`) hit this arm without going through
    /// the boot path.
    MountsLoaded(Vec<crate::file::fs::mounts::Mount>),
    /// A `fs::walk` resolved for one of the three panes. Reducer
    /// plants the entries on the named pane only if `cwd` still
    /// matches that pane's `cwd` — a stale walk from an aborted nav
    /// doesn't clobber the live state. This is the wire shape Step 15's
    /// `fs::walk` Result rides on its way to `view::pane`.
    EntriesLoaded {
        pane_id: PaneId,
        cwd: PathBuf,
        entries: Vec<Entry>,
    },
    /// Mouse-click on a pane row positions the cursor on that entry.
    /// The reducer clamps the index against the live `entries.len()`
    /// so a stale paint can't index out of bounds.
    CursorTo(EntryId),
    /// Mouse-double-click (or single-click on an already-selected dir
    /// row) opens the entry under the cursor — same semantics as the
    /// `Enter` / `l` keymap arm. Files are a no-op today (Step 33+
    /// wires `xdg-open`).
    ActivateEntry(EntryId),
    /// The async preview resolver finished for `path`. The reducer
    /// caches the paint-ready payload on `state.preview.resolved` (if
    /// the cursor still sits on `path`) so the view paints it without
    /// any I/O. This is what keeps navigation non-blocking — the read
    /// + syntect highlight happen off the UI thread.
    PreviewResolved {
        path: PathBuf,
        payload: super::state::PreviewPayload,
    },
}

/// Resolve the preview for the entry currently under the cursor,
/// off the UI thread. Sets `state.preview.current_path` synchronously
/// (so a stale resolution can be discarded on arrival) and returns a
/// `Task` that performs the file I/O + syntect highlight in the
/// background, landing as `Message::PreviewResolved`. Directories
/// route to an async child-listing walk instead of a previewer.
///
/// This is the fix for "preview blocks navigation": the `view()`
/// callback paints purely from `state.preview.resolved`, never
/// touching the filesystem.
fn resolve_preview(state: &mut State) -> Task<Message> {
    let pane = &state.panes.current;
    let Some(entry) = pane.entries.get(pane.cursor).cloned() else {
        state.preview.current_path = None;
        state.preview.resolved = None;
        return Task::none();
    };
    let path = pane.cwd.join(&entry.name);
    // Already resolved for this exact path? Nothing to do — avoids a
    // re-read storm when the cursor lands back on a cached entry.
    if state.preview.resolved.as_ref().map(|(p, _)| p) == Some(&path) {
        state.preview.current_path = Some(path);
        return Task::none();
    }
    state.preview.current_path = Some(path.clone());
    // Directories: list children in the preview pane (async walk).
    if matches!(entry.kind, super::state::EntryKind::Dir) {
        state.panes.preview.cwd = path.clone();
        return Task::perform(walk_for_pane(PaneId::Preview, path), |m| m);
    }
    let mime = super::view::preview::mime_for_entry(&entry, &path);
    match super::view::preview::kind_for(&mime) {
        super::view::preview::PreviewKind::Image => {
            let p = path.clone();
            Task::perform(
                super::view::preview::image::load(path),
                move |res| match res {
                    Ok((path, _handle)) => Message::PreviewResolved {
                        path,
                        payload: super::state::PreviewPayload::Image,
                    },
                    Err(_) => Message::PreviewResolved {
                        path: p.clone(),
                        payload: super::state::PreviewPayload::Info(
                            super::view::preview::format_file_info(&p),
                        ),
                    },
                },
            )
        }
        super::view::preview::PreviewKind::Text => {
            let p = path.clone();
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        super::view::preview::text::highlight_path(&p)
                    })
                    .await
                    .unwrap_or_default()
                },
                move |lines| Message::PreviewResolved {
                    path: path.clone(),
                    payload: super::state::PreviewPayload::Text(lines),
                },
            )
        }
        super::view::preview::PreviewKind::NoBuiltin => {
            let p = path.clone();
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || super::view::preview::format_file_info(&p))
                        .await
                        .unwrap_or_default()
                },
                move |body| Message::PreviewResolved {
                    path: path.clone(),
                    payload: super::state::PreviewPayload::Info(body),
                },
            )
        }
    }
}

/// Spawn the parent + current pane refreshes for the new cwd. The
/// preview pane refresh is driven separately by the cursor reducer
/// (it depends on the entry the cursor lands on, not on the cwd).
fn refresh_panes(cwd: &std::path::Path) -> Task<Message> {
    let current = cwd.to_path_buf();
    let parent = cwd.parent().map(|p| p.to_path_buf());
    let mut tasks = vec![Task::perform(
        walk_for_pane(PaneId::Current, current),
        |m| m,
    )];
    if let Some(p) = parent {
        tasks.push(Task::perform(walk_for_pane(PaneId::Parent, p), |m| m));
    }
    Task::batch(tasks)
}

async fn walk_for_pane(pane_id: PaneId, cwd: PathBuf) -> Message {
    let entries = super::fs::walk::walk(&cwd, false).await.unwrap_or_default();
    Message::EntriesLoaded {
        pane_id,
        cwd,
        entries,
    }
}

/// State reducer. Pure — no I/O. The headless harness calls into this
/// directly so the journey-J1 timing assertion exercises the same code
/// path the real winit reactor would.
pub fn update(state: &mut State, msg: Message) -> Task<Message> {
    match msg {
        Message::Tick => {
            // Step 23: no-op. Step 24+ uses Tick to drive the
            // directory-watcher refresh — the reducer body grows
            // there. Returning `Task::none()` keeps the iced reactor
            // idle until the next event.
            Task::none()
        }
        Message::Loaded(path) => {
            state.panes.current.cwd = path.clone();
            if let Some(p) = path.parent() {
                state.panes.parent.cwd = p.to_path_buf();
            }
            refresh_panes(&path)
        }
        Message::WindowResized(width, _height) => {
            // SPEC §3.2 row 2: the layout ladder is purely a function
            // of the window width. `view::mode_for_width` is the single
            // source of truth for the thresholds (1100 / 720); routing
            // through it means the unit tests in `view::tests::*` and
            // the journey-J7 e2e read the same table.
            state.mode = super::view::mode_for_width(width);
            Task::none()
        }
        Message::KeyPressed(key, mods) => handle_key(state, &key, mods),
        Message::CommandQueryChanged(q) => {
            state.commandbar.set_query(q);
            if state.commandbar.mode == CommandMode::Filter {
                state.commandbar.filter_results = super::search::filename::matches(
                    &state.commandbar.query,
                    &state.panes.current.entries,
                );
            }
            Task::none()
        }
        Message::CommandSelectVerb(verb) => {
            state.commandbar.select_verb(verb);
            Task::none()
        }
        Message::CommandClose => {
            state.commandbar.close();
            Task::none()
        }
        Message::Navigate(path) => {
            state.panes.current.cwd = path.clone();
            if let Some(p) = path.parent() {
                state.panes.parent.cwd = p.to_path_buf();
            }
            refresh_panes(&path)
        }
        Message::HoverEntry(id) => handle_hover(state, id),
        Message::PreviewLoaded { path, handle } => {
            // Step 26: today the iced renderer caches the decoded
            // image handle in its own GPU pool keyed by the path
            // embedded in `Handle::Bytes`'s `Id`, so the reducer
            // doesn't have to hold the handle itself. We still pin
            // `current_path` so a stale decode that finishes after
            // the user has moved on can be ignored at paint time.
            // The handle's `id()` is referenced so a future change
            // that drops it from the wire shape is forced through
            // this site — Step 27 will store the handle when the
            // plugin-routed dispatch needs to round-trip a `Vec<u8>`.
            let _ = handle.id();
            if state.preview.current_path.as_deref() != Some(&path) {
                // Cursor moved on; drop the stale decode silently.
                return Task::none();
            }
            Task::none()
        }
        Message::PreviewLoadedPng { path, bytes } => {
            if state.preview.current_path.as_deref() != Some(&path) {
                // Cursor moved on; drop the stale plugin render.
                return Task::none();
            }
            // Touch the bytes through an iced handle so the future
            // GPU-cache wiring is forced through one site.
            let _ = iced::widget::image::Handle::from_bytes(bytes).id();
            // Clear any stale text-preview slot — this hover is an
            // image render now.
            state.preview.text_preview = None;
            Task::none()
        }
        Message::PreviewLoadedText { path, content } => {
            if state.preview.current_path.as_deref() != Some(&path) {
                return Task::none();
            }
            state.preview.text_preview = Some((path, content));
            Task::none()
        }
        Message::PreviewFailed { path, error } => {
            // Step 27 DoD `plugin_crash_falls_back_to_built_in_text`:
            // when no plugin claims the MIME (the common case — zero
            // previewer plugins installed), fall back to the built-in
            // file-info card. This is the expected steady state, NOT
            // an error, so it logs at `debug` (a `warn` per cursor
            // move would flood an interactive session).
            tracing::debug!(
                target = "sy::file::preview",
                path = %path.display(),
                error = %error,
                "no plugin previewer; using built-in file-info card"
            );
            if state.preview.current_path.as_deref() == Some(&path) {
                state.preview.text_preview = None;
                // Resolve a built-in file-info card off-thread so the
                // pane shows something useful instead of "loading…".
                let p = path.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            super::view::preview::format_file_info(&p)
                        })
                        .await
                        .unwrap_or_default()
                    },
                    move |body| Message::PreviewResolved {
                        path: path.clone(),
                        payload: super::state::PreviewPayload::Info(body),
                    },
                );
            }
            Task::none()
        }
        Message::DragStart(ids) => {
            handle_drag_start(state, ids);
            Task::none()
        }
        Message::DragOffer(mime) => {
            // The wayland subsystem reports the advertised MIME. We
            // log it so a future test that doesn't drive the source
            // directly can observe the offer landed, and we keep the
            // drag-source state untouched (the source lives until the
            // drop / cancel signal closes it).
            tracing::debug!(
                target = "sy::file::dnd",
                mime = %mime,
                "drag-source offered MIME"
            );
            Task::none()
        }
        Message::DropAccept(target) => {
            handle_drop_accept(state, target);
            Task::none()
        }
        Message::KnowledgeQuery(q) => handle_knowledge_query(state, q),
        Message::KnowledgeHits(hits) => {
            handle_knowledge_hits(state, hits);
            Task::none()
        }
        Message::KnowledgeQueryResolved(hits, status) => {
            state.knowledge.status = status;
            handle_knowledge_hits(state, hits);
            Task::none()
        }
        Message::KnowledgeStatusChanged(status) => {
            state.knowledge.status = status;
            Task::none()
        }
        Message::BookmarkPin(key) => {
            handle_bookmark_pin(state, key);
            Task::none()
        }
        Message::BookmarkJump(key) => {
            handle_bookmark_jump(state, key);
            Task::none()
        }
        Message::CursorTo(id) => {
            if let Some(idx) = state.panes.current.entries.iter().position(|e| e.id == id) {
                state.panes.current.cursor = idx;
            }
            resolve_preview(state)
        }
        Message::ActivateEntry(id) => {
            let pane = &state.panes.current;
            if let Some(entry) = pane.entries.iter().find(|e| e.id == id) {
                if matches!(entry.kind, super::state::EntryKind::Dir) {
                    let target = pane.cwd.join(&entry.name);
                    return Task::done(Message::Navigate(target));
                }
            }
            Task::none()
        }
        Message::EntriesLoaded {
            pane_id,
            cwd,
            entries,
        } => {
            // The active child the parent pane should highlight (yazi
            // breadcrumb chain) — the basename of the current cwd.
            let active_child = if pane_id == PaneId::Parent {
                state
                    .panes
                    .current
                    .cwd
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
            } else {
                None
            };
            let target = match pane_id {
                PaneId::Parent => &mut state.panes.parent,
                PaneId::Current => &mut state.panes.current,
                PaneId::Preview => &mut state.panes.preview,
            };
            if target.cwd == cwd {
                target.set_entries(entries);
                // Position the parent pane's cursor on the directory the
                // user is currently inside, so the whole active chain is
                // highlighted as they descend.
                if let Some(child) = active_child {
                    if let Some(idx) = target.entries.iter().position(|e| e.name == child) {
                        target.cursor = idx;
                    }
                }
            }
            // A `Current`-pane refresh may have clamped the cursor onto a
            // new entry; re-resolve the preview so the pane to the right
            // tracks the cursor without a manual hover.
            if pane_id == PaneId::Current {
                return resolve_preview(state);
            }
            Task::none()
        }
        Message::MountsLoaded(mounts) => {
            state.mounts = mounts;
            Task::none()
        }
        Message::PreviewResolved { path, payload } => {
            // Discard if the cursor moved on while the resolver ran.
            if state.preview.current_path.as_deref() == Some(&path) {
                state.preview.resolved = Some((path, payload));
            }
            Task::none()
        }
    }
}

/// Step 31 — `b<key>` pin arm. Locks the bookmarks registry (if
/// wired) and persists the current pane's cwd under `key`. Lock
/// poisoning falls through silently — the daemon's tracing warns
/// already-rode out of `Bookmarks::save`.
fn handle_bookmark_pin(state: &mut State, key: char) {
    let cwd = state.panes.current.cwd.clone();
    let Some(reg) = state.bookmarks.clone() else {
        return;
    };
    let Ok(mut guard) = reg.lock() else {
        return;
    };
    if let Err(e) = guard.pin(key, cwd, None) {
        tracing::warn!(
            target = "sy::file::bookmarks",
            key = %key,
            error = %e,
            "bookmark pin failed"
        );
    }
}

/// Step 31 — `b<key>` jump arm. Locks the bookmarks registry (if
/// wired), looks up `key`, and warps `state.panes.current.cwd` to
/// the bound path. Unbound keys are a no-op.
fn handle_bookmark_jump(state: &mut State, key: char) {
    let Some(reg) = state.bookmarks.clone() else {
        return;
    };
    let Ok(guard) = reg.lock() else {
        return;
    };
    if let Some(path) = guard.jump(key) {
        state.panes.current.cwd = path.to_path_buf();
    }
}

/// Step 30 — reducer arm for `Message::KnowledgeQuery`. Stamps the
/// query on `state.knowledge.last_query`, optimistically flips the
/// chip to `Reachable` (so a prior `Unreachable` clears the second
/// the operator retries), and spawns the async `query` task against
/// [`RealKnowledgeBackend`]; the resolved hits land via
/// [`Message::KnowledgeHits`] and the chip status via
/// [`Message::KnowledgeStatusChanged`] in batch.
fn handle_knowledge_query(state: &mut State, q: String) -> Task<Message> {
    state.knowledge.last_query = Some(q.clone());
    let cwd = state.panes.current.cwd.clone();
    let backend: std::sync::Arc<dyn KnowledgeBackend> = std::sync::Arc::new(RealKnowledgeBackend);
    const KNOWLEDGE_PALETTE_LIMIT: usize = 12;
    let fut = super::search::knowledge::query(backend, cwd, q, KNOWLEDGE_PALETTE_LIMIT);
    Task::perform(fut, |res| match res {
        Ok(outcome) => Message::KnowledgeQueryResolved(outcome.hits, outcome.status),
        // `query` already collapses backend errors / timeouts to an
        // `Ok(outcome)`; an outer Err can only come from a panic —
        // surface as `Unreachable` so the chip dim-greys.
        Err(_) => Message::KnowledgeStatusChanged(KnowledgeStatus::Unreachable),
    })
}

/// Step 30 — reducer arm for `Message::KnowledgeHits`. Stores the
/// hit list on `state.knowledge.last_hits` (merged with the filename
/// matches the live `/` filter produced); the statusbar chip reads
/// the count to switch from `"knowledge: idle"` to `"knowledge: N
/// hits"`. An empty list from a backend that was reachable is
/// distinguished from `Unreachable` by leaving `status` unchanged
/// (the `Reachable` chip remains active).
fn handle_knowledge_hits(state: &mut State, qdrant_hits: Vec<(PathBuf, f32)>) {
    // Filename-only fallback rank pulled from the current pane: every
    // entry's index becomes a synthetic negative-score so qdrant
    // entries naturally rank above (per `merge`'s contract).
    let cwd = state.panes.current.cwd.clone();
    let filename_hits: Vec<(PathBuf, f32)> = state
        .panes
        .current
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (cwd.join(&e.name), -(i as f32) / 1000.0))
        .collect();
    let merged = super::search::knowledge::merge(qdrant_hits, filename_hits);
    // Land the cursor on the top hit so the journey-J4 "Enter →
    // top result" beat reads true. Find the merged top path inside
    // the current pane's entries and move the cursor there.
    if let Some((top_path, _)) = merged.first() {
        if let Some(idx) = state
            .panes
            .current
            .entries
            .iter()
            .position(|e| state.panes.current.cwd.join(&e.name) == *top_path)
        {
            state.panes.current.cursor = idx;
        }
    }
    state.knowledge.last_hits = merged;
}

/// Step 29 — translate a `DragStart` into a [`DragSource`] stashed on
/// `state.drag_source`. The MIME body the wayland subsystem reads off
/// the source is built via [`paths_to_uri_list`] in the bin's
/// wayland adapter (out of scope for the pure reducer); the reducer's
/// job is to resolve [`EntryId`]s against the current pane's cwd and
/// pick the action ([`DragAction::Copy`] default).
fn handle_drag_start(state: &mut State, ids: Vec<EntryId>) {
    let cwd = state.panes.current.cwd.clone();
    let paths: Vec<PathBuf> = state
        .panes
        .current
        .entries
        .iter()
        .filter(|e| ids.contains(&e.id))
        .map(|e| cwd.join(&e.name))
        .collect();
    if paths.is_empty() {
        return;
    }
    state.drag_source = Some(DragSource {
        paths,
        action: DragAction::Copy,
    });
}

/// Step 29 — translate a `DropAccept` into the matching
/// `Operation::Copy` / `Operation::Move` queued on `state.ops`. The
/// `target.action` field carries the modifier-derived choice from
/// [`drop_action_from_modifiers`]; the destination is the current
/// pane's cwd.
fn handle_drop_accept(state: &mut State, target: DropTarget) {
    if target.paths.is_empty() {
        return;
    }
    let dst = state.panes.current.cwd.clone();
    let op = match target.action {
        DropAction::Copy => Operation::Copy {
            srcs: target.paths,
            dst,
            conflict: ConflictPolicy::Skip,
        },
        DropAction::Move => Operation::Move {
            srcs: target.paths,
            dst,
            conflict: ConflictPolicy::Skip,
        },
    };
    state.ops.push(op);
}

/// Step 29 — compose the `text/uri-list` body for the current
/// drag-source. Pulled out so the wayland adapter (or the integration
/// test) can read the same shape the bin will hand `wl_data_device`.
/// Returns an empty string when no drag is in flight.
pub fn current_drag_uri_list(state: &State) -> String {
    match &state.drag_source {
        Some(src) => paths_to_uri_list(&src.paths),
        None => String::new(),
    }
}

/// Step 29 — convenience wrapper so the wayland adapter (and the
/// integration test) can convert an `iced::keyboard::Modifiers` mask
/// straight into the typed [`DropAction`]. Re-exports the pure-fn for
/// callers that have already `use`-d `super::app::*`.
pub fn drop_action(mods: &iced::keyboard::Modifiers) -> DropAction {
    drop_action_from_modifiers(mods)
}

/// Translate a `HoverEntry` event into the preview-pane state
/// mutation. Pulled out so the reducer stays scrollable and the
/// integration test can call `handle_hover` directly without driving
/// the full iced `Task` machinery. The journey-J3 first-byte budget
/// is asserted against the `Task::perform`'d future this function
/// returns.
fn handle_hover(state: &mut State, id: EntryId) -> Task<Message> {
    let Some(entry) = state
        .panes
        .current
        .entries
        .iter()
        .find(|e| e.id == id)
        .cloned()
    else {
        return Task::none();
    };
    let path = state.panes.current.cwd.join(&entry.name);
    state.preview.current_path = Some(path.clone());
    // Only the image previewer benefits from off-runtime decode;
    // text previewer reads on the iced thread inside `text::preview`
    // (the 64 KiB read is cheap enough for the synchronous path).
    let mime = super::view::preview::mime_for_entry(&entry, &path);
    match super::view::preview::kind_for(&mime) {
        super::view::preview::PreviewKind::Image => {
            Task::perform(super::view::preview::image::load(path), |res| match res {
                Ok((path, handle)) => Message::PreviewLoaded { path, handle },
                // Decode failure: silently drop. Step 27's plugin
                // dispatch will surface a user-visible error.
                Err(_) => Message::Tick,
            })
        }
        super::view::preview::PreviewKind::NoBuiltin => {
            // Step 27: route the unknown MIME through the plugin
            // bridge if one is wired. Headless / tests that didn't
            // attach a bridge silently fall through to the view's
            // "no built-in preview" fallback (the reducer can't
            // do better without I/O).
            let Some(bridge) = state.plugin_bridge.clone() else {
                return Task::none();
            };
            let target_mime = mime.clone();
            let target_path = path.clone();
            use super::plugin_bridge::PreviewResult;
            Task::perform(
                async move { bridge.preview_for(&target_mime, &target_path).await },
                move |res| match res {
                    Ok(PreviewResult::Png(bytes)) => Message::PreviewLoadedPng {
                        path: path.clone(),
                        bytes,
                    },
                    Ok(PreviewResult::Text(content)) => Message::PreviewLoadedText {
                        path: path.clone(),
                        content,
                    },
                    Err(e) => Message::PreviewFailed {
                        path: path.clone(),
                        error: e.to_string(),
                    },
                },
            )
        }
        super::view::preview::PreviewKind::Text => Task::none(),
    }
}

/// Translate a `KeyPressed` event into a command-bar / selection /
/// bulk-op state mutation. Pulled out so the reducer body stays
/// scrollable and the unit / integration tests can hit the key arms
/// directly.
///
/// Step 25 (SPEC §3.3 item 4 + item 7):
/// * `/` opens the filter, `:` opens the palette.
/// * `Escape` closes whatever's open.
///
/// Step 28 (SPEC §3.3 item 6 + item 16 — journey J5 → J6):
/// * `Space` — toggle selection on the entry under the cursor.
/// * `Shift+ArrowDown` — extend selection downward inclusive of the
///   current cursor (`SelectionSet::add_range`).
/// * `Shift+ArrowUp` — same, upward.
/// * `*` — select every entry in the current pane.
/// * `a` — invert the selection within the current pane.
/// * `y` — copy: stash the selected absolute paths in
///   `state.clipboard` with `ClipboardMode::Copy`.
/// * `x` — move: same but `ClipboardMode::Move`.
/// * `d` — trash: queue an `Operation::Trash` against the selected paths.
/// * `p` — paste: drain `state.clipboard` into an `Operation::Copy`
///   (or `Move`) targeting cwd.
pub fn handle_key(
    state: &mut State,
    key: &iced::keyboard::Key,
    mods: iced::keyboard::Modifiers,
) -> Task<Message> {
    use iced::keyboard::{key::Named, Key};
    // Step 31 — two-key `b<key>` chord. When the prior keypress was a
    // bare `b` (i.e. `pending_key_chord == Some('b')`), the *next*
    // character keypress drives `BookmarkJump(<key>)`. Pinning a
    // bookmark requires the `B<key>` chord (capital B) so a stray
    // `b` followed by an arrow / Escape doesn't accidentally pin.
    if let Some('b') = state.pending_key_chord {
        state.pending_key_chord = None;
        if let Key::Character(c) = key {
            if let Some(ch) = c.chars().next() {
                handle_bookmark_jump(state, ch);
            }
        }
        // Non-character second key (Escape, Arrow, …) cancels the chord.
        return Task::none();
    }
    if let Some('B') = state.pending_key_chord {
        state.pending_key_chord = None;
        if let Key::Character(c) = key {
            if let Some(ch) = c.chars().next() {
                handle_bookmark_pin(state, ch);
            }
        }
        return Task::none();
    }
    match key {
        Key::Character(c) if c.as_str() == "/" => {
            state.commandbar.open_filter();
            Task::none()
        }
        Key::Character(c) if c.as_str() == ":" => {
            state.commandbar.open_palette();
            Task::none()
        }
        Key::Character(c) if c.as_str() == "b" => {
            state.pending_key_chord = Some('b');
            Task::none()
        }
        Key::Character(c) if c.as_str() == "B" => {
            state.pending_key_chord = Some('B');
            Task::none()
        }
        Key::Named(Named::Escape) => {
            state.commandbar.close();
            state.range_anchor = None;
            state.pending_key_chord = None;
            Task::none()
        }
        Key::Named(Named::Space) => {
            handle_space_toggle(state);
            Task::none()
        }
        Key::Named(Named::ArrowDown) if mods.shift() => {
            handle_range_extend(state, 1);
            Task::none()
        }
        Key::Named(Named::ArrowUp) if mods.shift() => {
            handle_range_extend(state, -1);
            Task::none()
        }
        // Plain arrow / vim-style cursor movement. The cursor is
        // clamped against the current pane's entry count so the
        // reducer can't index past the end.
        Key::Named(Named::ArrowDown) => {
            handle_cursor_move(state, 1);
            resolve_preview(state)
        }
        Key::Named(Named::ArrowUp) => {
            handle_cursor_move(state, -1);
            resolve_preview(state)
        }
        Key::Character(c) if c.as_str() == "j" => {
            handle_cursor_move(state, 1);
            resolve_preview(state)
        }
        Key::Character(c) if c.as_str() == "k" => {
            handle_cursor_move(state, -1);
            resolve_preview(state)
        }
        // Enter / `l` / ArrowRight — open the entry under the cursor.
        // Dirs warp the current pane via `Message::Navigate`; files
        // are a no-op today (Step 33+ wires `xdg-open` for non-dirs).
        Key::Named(Named::Enter) => handle_cursor_open(state).unwrap_or_else(Task::none),
        Key::Named(Named::ArrowRight) => handle_cursor_open(state).unwrap_or_else(Task::none),
        Key::Character(c) if c.as_str() == "l" => {
            handle_cursor_open(state).unwrap_or_else(Task::none)
        }
        // Backspace / `h` / ArrowLeft — `cd ..`. No-op at filesystem
        // root.
        Key::Named(Named::Backspace) => handle_cursor_up(state).unwrap_or_else(Task::none),
        Key::Named(Named::ArrowLeft) => handle_cursor_up(state).unwrap_or_else(Task::none),
        Key::Character(c) if c.as_str() == "h" => {
            handle_cursor_up(state).unwrap_or_else(Task::none)
        }
        Key::Character(c) if c.as_str() == "*" => {
            handle_select_all(state);
            Task::none()
        }
        Key::Character(c) if c.as_str() == "a" => {
            handle_invert(state);
            Task::none()
        }
        Key::Character(c) if c.as_str() == "y" => {
            handle_clipboard_stash(state, ClipboardMode::Copy);
            Task::none()
        }
        Key::Character(c) if c.as_str() == "x" => {
            handle_clipboard_stash(state, ClipboardMode::Move);
            Task::none()
        }
        Key::Character(c) if c.as_str() == "d" => {
            handle_trash(state);
            Task::none()
        }
        Key::Character(c) if c.as_str() == "p" => {
            handle_paste(state);
            Task::none()
        }
        _ => Task::none(),
    }
}

/// Step 28+: move the cursor on the current pane by `delta` rows
/// (negative = up). Clamps against `entries.len() - 1`. Pure state
/// mutation; the next paint re-renders the row highlight.
fn handle_cursor_move(state: &mut State, delta: i32) {
    let pane = &mut state.panes.current;
    let len = pane.entries.len();
    if len == 0 {
        pane.cursor = 0;
        return;
    }
    let next = (pane.cursor as i32 + delta).clamp(0, (len - 1) as i32);
    pane.cursor = next as usize;
}

/// Enter the entry under the cursor. Dirs warp the current pane via
/// `Message::Navigate`; files are a no-op today (Step 33+ wires the
/// `xdg-open` ladder). Returns `Some(task)` when the navigation
/// fires, `None` otherwise so the keymap caller can fall through.
fn handle_cursor_open(state: &State) -> Option<Task<Message>> {
    let pane = &state.panes.current;
    let entry = pane.entries.get(pane.cursor)?;
    if !matches!(entry.kind, super::state::EntryKind::Dir) {
        return None;
    }
    let target = pane.cwd.join(&entry.name);
    Some(Task::done(Message::Navigate(target)))
}

/// `cd ..` on the current pane. No-op at filesystem root. Returns the
/// `Message::Navigate` task on success so the caller can chain it
/// into the keymap match arms.
fn handle_cursor_up(state: &State) -> Option<Task<Message>> {
    let parent = state.panes.current.cwd.parent()?.to_path_buf();
    Some(Task::done(Message::Navigate(parent)))
}

/// Step 28 J5 — `<Space>` toggles selection on the entry under the
/// cursor. Idempotent (a second press untoggles). Pure state mutation;
/// the pane render reads the result on the next paint.
fn handle_space_toggle(state: &mut State) {
    let cursor = state.panes.current.cursor;
    if let Some(entry) = state.panes.current.entries.get(cursor) {
        state.selection.toggle(entry.id);
    }
}

/// Step 28 J5 — `<Shift>+ArrowDown/Up` extends the multi-select
/// inclusive of the prior anchor and the new cursor. `delta = +1`
/// pushes the cursor down, `-1` up; the cursor is clamped against the
/// pane's row count so the reducer doesn't index past the end.
fn handle_range_extend(state: &mut State, delta: i32) {
    let pane = &mut state.panes.current;
    if pane.entries.is_empty() {
        return;
    }
    let prev = pane.cursor;
    // Plant the anchor on the FIRST shift+arrow if not already set so
    // a subsequent `Space` releases it (the yazi convention).
    if state.range_anchor.is_none() {
        if let Some(e) = pane.entries.get(prev) {
            state.range_anchor = Some(e.id);
        }
    }
    let new_cursor = if delta > 0 {
        (prev + 1).min(pane.entries.len() - 1)
    } else {
        prev.saturating_sub(1)
    };
    pane.cursor = new_cursor;
    let anchor = state.range_anchor;
    let cursor_id = pane.entries.get(new_cursor).map(|e| e.id);
    if let (Some(a), Some(b)) = (anchor, cursor_id) {
        state.selection.add_range(a, b);
    }
}

/// Step 28 J5 — `*` selects every entry in the current pane.
fn handle_select_all(state: &mut State) {
    let ids: Vec<EntryId> = state.panes.current.entries.iter().map(|e| e.id).collect();
    state.selection.all(&ids);
}

/// Step 28 J5 — `a` inverts the selection within the current pane.
fn handle_invert(state: &mut State) {
    let ids: Vec<EntryId> = state.panes.current.entries.iter().map(|e| e.id).collect();
    state.selection.invert(&ids);
}

/// Step 28 J5 — `y` / `x` stash the currently-selected entries on
/// `state.clipboard` along with the [`ClipboardMode`] discriminator
/// the paste reducer reads. Resolves each [`EntryId`] to an absolute
/// path against the current pane's cwd so the paste arm doesn't have
/// to re-resolve names.
fn handle_clipboard_stash(state: &mut State, mode: ClipboardMode) {
    let cwd = state.panes.current.cwd.clone();
    let paths: Vec<std::path::PathBuf> = state
        .panes
        .current
        .entries
        .iter()
        .filter(|e| state.selection.contains(e.id))
        .map(|e| cwd.join(&e.name))
        .collect();
    if !paths.is_empty() {
        state.clipboard = Some((mode, paths));
    }
}

/// Step 28 J6 — `d` queues an `Operation::Trash` against every
/// currently-selected entry. The op is pushed onto `state.ops` so the
/// statusbar's ops chip reflects the queue depth; the daemon's IPC
/// path (Step 20) is what actually drives `fs::trash::trash`.
fn handle_trash(state: &mut State) {
    let cwd = state.panes.current.cwd.clone();
    let srcs: Vec<std::path::PathBuf> = state
        .panes
        .current
        .entries
        .iter()
        .filter(|e| state.selection.contains(e.id))
        .map(|e| cwd.join(&e.name))
        .collect();
    if !srcs.is_empty() {
        state.ops.push(Operation::Trash { srcs });
    }
}

/// Step 28 J6 — `p` drains `state.clipboard` into an
/// `Operation::Copy` (or `Move`) targeting the current pane's cwd.
/// Yazi convention: paste clears the clipboard so a second paste
/// needs a fresh `y` / `x`.
fn handle_paste(state: &mut State) {
    let Some((mode, srcs)) = state.clipboard.take() else {
        return;
    };
    let dst = state.panes.current.cwd.clone();
    let op = match mode {
        ClipboardMode::Copy => Operation::Copy {
            srcs,
            dst,
            conflict: ConflictPolicy::Skip,
        },
        ClipboardMode::Move => Operation::Move {
            srcs,
            dst,
            conflict: ConflictPolicy::Skip,
        },
    };
    state.ops.push(op);
}

/// Step 25 view: wrap the Step 24 `view::root` composition in a top-
/// level [`column!`] with the statusbar pinned above and the command
/// bar pinned below. The view callback stays pure — it never reads
/// fs or IPC; the reducer is the only mutation path.
pub fn view(state: &State) -> Element<'_, Message> {
    column![
        super::view::statusbar::statusbar(state),
        super::view::statusbar::ops_drawer(state),
        super::view::root(state),
        super::view::commandbar::commandbar(state),
    ]
    .spacing(4)
    .into()
}

/// Window title. Reads the current pane's cwd so an operator running
/// multiple `sy file` instances can tell them apart at a glance.
pub fn title(state: &State) -> String {
    let cwd = state.panes.current.cwd.display();
    format!("sy file — {cwd}")
}

/// Iced theme hook. Returns the gruvbox-dark built-in per Step 23 DoD.
pub fn theme(_state: &State) -> iced::Theme {
    super::theme::iced_theme()
}

/// Step 31 — resolve `$XDG_STATE_HOME/sy/file/` (falls back to
/// `$HOME/.local/state/sy/file/` per the freedesktop XDG basedir spec).
/// Pulled out as a fn so the production `app::run` and a future MCP
/// op can resolve the same path. Pure-fn (env reads only); the
/// `bookmarks::load` call is the side-effecting consumer.
pub fn bookmarks_state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("state")
        });
    base.join("sy").join("file")
}

/// Step 31 — resolve `$XDG_DATA_HOME/` (falls back to
/// `$HOME/.local/share/` per the freedesktop XDG basedir spec). The
/// `recently-used.xbel` log lands here so other DEs see the same
/// recent-dirs list (Nautilus, Dolphin, the GTK file-chooser).
pub fn bookmarks_xbel_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
}

/// Step 25 subscription: listen for `KeyPressed` events so `/` and
/// `:` open the command bar. Idle keys ride through unconsumed. The
/// subscription is shape-symmetric with `sy mon`'s — same `event::
/// listen()` filter pattern (see `crate::mon::app::subscription`).
pub fn subscription(_state: &State) -> Subscription<Message> {
    // Step 25 keymap channel + Step 29 wayland-drop channel. Both are
    // `event::listen()` filters that return `None` for events the
    // other consumes, so combining them via `Subscription::batch`
    // keeps each filter's responsibility narrow.
    let keys = event::listen().filter_map(|ev| match ev {
        Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            Some(Message::KeyPressed(key, modifiers))
        }
        // SPEC §3.2 row 2 / journey-J7: the responsive 3→2→1 ladder
        // is purely a function of window width. iced's
        // `Event::Window(Window::Resized)` carries the new logical
        // size; the reducer's `WindowResized` arm plumbs it into
        // `view::mode_for_width`.
        Event::Window(iced::window::Event::Resized(size)) => Some(Message::WindowResized(
            size.width as u32,
            size.height as u32,
        )),
        _ => None,
    });
    let drops = super::dnd::dnd_subscription().map(Message::DropAccept);
    Subscription::batch([keys, drops])
}

/// Launch the `sy file` xdg-toplevel window. Blocks until the user
/// dismisses the window. Step 24+ extends the iced builder with the
/// responsive subscription and the real pane tree.
///
/// Gated on `gui-iced` at the module level; this function is only
/// callable under the feature.
pub fn run(initial_path: PathBuf) -> Result<()> {
    // Step 26: warm the syntect + cosmic-text caches before the iced
    // reactor boots so the journey-J3 first-byte budget is measured
    // against a steady-state warm process. Cold-start cost (~30 ms on
    // a warm SSD for the syntect bundle) would otherwise poison the
    // first hover-preview. Idempotent: a second call is a no-op via
    // the `OnceLock` shortcut.
    super::view::preview::warm_caches();
    // Step 27: discover the plugin registry + wrap a
    // `PluginBridge` so the `NoBuiltin` arm of the preview dispatcher
    // can route hover events through plugins. Discovery failure
    // shouldn't block the file manager from starting — the user can
    // still navigate / open files without previewers — so on error
    // we log + continue with `plugin_bridge = None`.
    let plugin_bridge: Option<std::sync::Arc<super::plugin_bridge::PluginBridge>> = {
        let registry =
            std::sync::Arc::new(crate::plugin::registry::discover().unwrap_or_else(|err| {
                tracing::warn!(
                    target = "sy::file::app",
                    error = %err,
                    "plugin registry discovery failed; previewer plugins disabled"
                );
                crate::plugin::registry::discover_empty()
            }));
        let (bridge, _notify_rx, _preview_rx) =
            super::plugin_bridge::build_with_channels(registry, serde_json::Value::Null);
        // Surface the previewer count once at startup so the operator
        // can confirm the registry picked up their plugins. Reads
        // through `bridge.registry()` so a future change that
        // narrows the Registry accessor surface forces this site
        // (and the file plane's diagnostics in general) to update.
        tracing::debug!(
            target = "sy::file::app",
            plugins = bridge.registry().plugin_ids().count(),
            "plugin bridge ready"
        );
        Some(bridge)
    };
    // Step 27 — `shutdown_all` is the bridge-side cooperative tear-
    // down (drain the cache, send each supervisor a `shutdown`
    // request). iced 0.14's xdg-toplevel reactor doesn't expose a
    // pre-exit hook, so the production path relies on the kernel
    // reaping the child processes on window close. `shutdown_all`
    // stays on the public surface for integration tests
    // (`tests/sy_file_plugin_preview.rs`) and for a future Step 28+
    // `Ctrl+Q` handler that triggers it from inside the iced
    // runtime. Touched here so the dead-code lint doesn't fire on
    // the bin's compile graph.
    let _ = super::plugin_bridge::PluginBridge::shutdown_all;
    // Step 29 — wayland DnD surface. iced 0.14's xdg-toplevel reactor
    // does NOT expose `wl_data_device_source` initiation through its
    // public subscription API; the cross-toolkit DnD source-side
    // requires a lower-level winit hook (or a separate
    // `smithay-client-toolkit` adapter). The pure-Rust uri-list +
    // modifier helpers ship today (the source-side wire shape is
    // verified by `tests/sy_file_dnd.rs` + the journey e2e); the
    // adapter that bridges iced's window handle into
    // `wl_data_device_manager_create_source` is a follow-up. The
    // drop-target side IS reachable via the existing `subscription`
    // (the `event::Window(FileDropped)` channel), so inbound DnD from
    // a Wayland compositor that surfaces drops over xdg-toplevel
    // works today. The references below pin the dead-code lint off
    // the public surface so the manual recipe in the module
    // docstring stays the canonical wire shape regardless of the
    // adapter layer the operator wires up.
    let _ = super::dnd::URI_LIST_MIME;
    let _ = super::dnd::paths_to_uri_list as fn(&[PathBuf]) -> String;
    let _ = super::dnd::parse_uri_list as fn(&str) -> Vec<PathBuf>;
    let _ = current_drag_uri_list as fn(&State) -> String;
    let _ = drop_action as fn(&iced::keyboard::Modifiers) -> super::dnd::DropAction;
    // Construct each currently-unreached variant once so the dead-
    // code lint doesn't fire on the source-side surface that ships
    // ahead of the iced-0.14 wayland-source adapter. The journey-e2e
    // and the unit tests already exercise the reducer arm for each,
    // so the variant set is wire-shape-stable.
    let _ = Message::DragStart(Vec::<EntryId>::new());
    let _ = Message::DragOffer(String::new());
    let _ = super::dnd::DragAction::Move;
    let _ = super::dnd::DragAction::Link;
    // Step 30 — `KnowledgeHits` is the direct-injection arm used by
    // integration tests + future MCP bridges to plant a pre-merged
    // hit list without going through the async `query` task. The
    // production `:k` path lands via `KnowledgeQueryResolved`; touch
    // the variant here so the dead-code lint stays happy on the
    // bin's compile while keeping the arm reachable for callers
    // that don't want the chip-status side effect.
    let _ = Message::KnowledgeHits(Vec::new());
    // Step 31 — touch the chord arms once so the dead-code lint stays
    // happy on production builds; the reducer + the e2e exercise the
    // shape, but the keymap drives both arms via `handle_key` rather
    // than producing them directly, so the variants would otherwise
    // be flagged.
    let _ = Message::BookmarkPin('_');
    let _ = Message::BookmarkJump('_');
    // `HoverEntry` is now test-only: production preview follows the
    // cursor (click / keyboard) via `resolve_preview`, not the mouse
    // pointer, so no widget constructs `HoverEntry`. The journey-J3
    // e2e drives it directly to exercise the plugin-preview path;
    // touch-pin it here so the bin's dead-code lint stays clean.
    let _ = Message::HoverEntry(0);
    // Step 31 — pinned-bookmark registry. Loads
    // `$XDG_STATE_HOME/sy/file/bookmarks.toml` (creating the dir if
    // necessary on the first save) and reads/writes
    // `$XDG_DATA_HOME/recently-used.xbel` per the freedesktop spec.
    // A load failure (e.g. permission denied on the state dir)
    // shouldn't kill the file manager — log + continue with the
    // None slot so `b<key>` is a no-op until the operator fixes the
    // permissions. The corrupt-TOML path is handled inside `load`.
    let bookmarks: Option<std::sync::Arc<std::sync::Mutex<super::bookmarks::Bookmarks>>> = {
        let state_dir = bookmarks_state_dir();
        let xbel_dir = bookmarks_xbel_dir();
        match super::bookmarks::load(&state_dir, &xbel_dir) {
            Ok(bm) => Some(std::sync::Arc::new(std::sync::Mutex::new(bm))),
            Err(e) => {
                tracing::warn!(
                    target = "sy::file::bookmarks",
                    state_dir = %state_dir.display(),
                    error = %e,
                    "bookmarks load failed; b<key> chord disabled"
                );
                None
            }
        }
    };
    iced::application(
        move || {
            // Boot returns a State pre-seeded with the bare-form
            // path, plus a `Loaded` Task so the same wire shape the
            // headless harness exercises in `run_headless_once`
            // round-trips through the real iced reactor. Keeping the
            // two paths symmetric means the journey-J1 timing test
            // measures the same reducer trace the production runtime
            // hits between window-create and first-paint.
            let state = State {
                plugin_bridge: plugin_bridge.clone(),
                bookmarks: bookmarks.clone(),
                ..State::default()
            };
            // Step 32 — kick off the async mountinfo loader so the
            // 3-pane sidebar paints with real data once the first
            // `MountsLoaded` reducer turn lands. The load completes
            // inside ~5 ms on a Fedora host (mountinfo parse) + up to
            // 250 ms more for the udisks2 D-Bus probe (which times
            // out gracefully on CI).
            let boot = Task::batch([
                Task::done(Message::Loaded(initial_path.clone())),
                Task::perform(super::fs::mounts::load(), |res| match res {
                    Ok(m) => {
                        // Sidebar only paints the user-visible disks
                        // (drops `/proc`, `/sys`, `cgroup2`, …) so the
                        // operator sees a short, scannable list.
                        let kept: Vec<_> = super::fs::mounts::filter_user_visible(&m)
                            .into_iter()
                            .cloned()
                            .collect();
                        Message::MountsLoaded(kept)
                    }
                    // mountinfo read errors are highly unusual; fall
                    // through to an empty list so the sidebar paints
                    // a "no mounts" affordance instead of crashing.
                    Err(_) => Message::MountsLoaded(Vec::new()),
                }),
            ]);
            (state, boot)
        },
        update,
        view,
    )
    .title(title)
    .theme(theme)
    .subscription(subscription)
    .window_size(iced::Size::new(DEFAULT_WIDTH, DEFAULT_HEIGHT))
    .run()
    .map_err(|e| anyhow::anyhow!("iced application error: {e}"))
}

/// Headless harness — drives one full `boot → update(Tick) → view()`
/// cycle without standing up a winit / wgpu surface. Returns the
/// number of `Message::Tick` reductions observed and the wall-clock
/// from harness entry to the first `view()` call.
///
/// The journey-J1 brief budgets 250 ms from "user typed `sy file ~`"
/// to "first paint". The iced builder's boot closure + the reducer's
/// `Tick` path + a single `view()` invocation are the literal code
/// the real runtime executes between the winit `WindowEvent::Created`
/// and the wgpu `RedrawRequested` for the first frame, so timing this
/// path is a faithful proxy for the journey assertion.
///
/// Returns `Ok((tick_count, elapsed))` so the e2e can assert both
/// "the boot reducer fired at least once" (the proxy for "first frame
/// painted") and "elapsed < 250 ms".
pub fn run_headless_once(initial_path: PathBuf) -> Result<(u64, Duration)> {
    let start = Instant::now();
    let mut state = State::default();
    state.panes.current.cwd = initial_path.clone();
    // Synthetic boot Task: production code returns `Task::done(Tick)`
    // from the boot closure; we materialise the same dispatch here.
    let _ = update(&mut state, Message::Tick);
    let mut ticks: u64 = 1;
    // Loaded message is dispatched by the bin's CLI shim today (Step
    // 23's `cli::run_scaffold` -> `app::run(path)`); fold it in here
    // so the headless harness mirrors the same dispatch order.
    let _ = update(&mut state, Message::Loaded(initial_path));
    // Step 24: the real iced reactor fires a `WindowEvent::Resized`
    // immediately after window creation (the compositor hands us our
    // initial size). The harness materialises the same dispatch so
    // `state.mode` is settled before the first `view()` paint — the
    // journey-J2 e2e otherwise observes the `LayoutMode::default()`
    // value, which already happens to be `ThreePane`, but pinning
    // the resize round-trip keeps the harness honest if the default
    // ever changes.
    let _ = update(
        &mut state,
        Message::WindowResized(DEFAULT_WIDTH as u32, DEFAULT_HEIGHT as u32),
    );
    // First `view()` call — the closest in-process analogue of the
    // wgpu `RedrawRequested` the real runtime would emit for frame 0.
    let element = view(&state);
    drop(element);
    // A second Tick proves the reducer remains idempotent post-paint
    // — the journey-J1 brief's "no re-render on idle" rider rides on
    // this property (Step 18 of sy-mon's ROADMAP carried the same
    // invariant).
    let _ = update(&mut state, Message::Tick);
    ticks += 1;
    Ok((ticks, start.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Message::Loaded` sets the current pane's cwd. Pinning this
    /// keeps the headless harness's cwd-roundtrip honest (without it
    /// the e2e wouldn't be able to assert the window title carries
    /// the right path).
    #[test]
    fn loaded_message_sets_current_cwd() {
        let mut state = State::default();
        let _ = update(&mut state, Message::Loaded(PathBuf::from("/tmp/sy-file")));
        assert_eq!(state.panes.current.cwd, PathBuf::from("/tmp/sy-file"));
    }

    /// `title()` formats `sy file — <cwd>`. Stable wire shape so a
    /// Step 24+ window-manager test can grep the right xdg-toplevel.
    #[test]
    fn title_carries_cwd() {
        let mut state = State::default();
        state.panes.current.cwd = PathBuf::from("/home/agent");
        let t = title(&state);
        assert!(t.starts_with("sy file"), "title prefix must be stable: {t}");
        assert!(t.contains("/home/agent"), "title must carry cwd: {t}");
    }

    /// `theme()` returns the sy palette projection — `Theme::Custom`
    /// with `name == "sy"`. The seven-slot palette is loaded from the
    /// bar theme so the file plane shares colour tokens with the rest
    /// of sy.
    #[test]
    fn theme_is_sy_custom() {
        let state = State::default();
        let label = format!("{:?}", theme(&state));
        assert!(
            label.contains("Custom") && label.contains("sy"),
            "expected sy Custom theme, got {label}"
        );
    }

    /// Step 24: the reducer walks the SPEC §3.2 row 2 ladder
    /// (≥1100 px → ThreePane, ≥720 px → TwoPane, <720 px → OnePane).
    /// Pinning all three transitions through the reducer (not just
    /// `view::mode_for_width`) makes sure no caller skips the
    /// dispatch on the assumption "the field defaults to the right
    /// mode anyway".
    #[test]
    fn window_resized_collapses_layout_through_view_thresholds() {
        // Reach for `LayoutMode` through `super::super::state::*`
        // (one nesting level up = the `app` module, then
        // `super::state::State` is what the reducer signature uses).
        // Keeping the path symmetric with the reducer's import
        // avoids a type-mismatch when integration-test shims
        // re-alias the `state` module under a different name.
        use super::super::state::LayoutMode;
        let mut state = State::default();
        let _ = update(&mut state, Message::WindowResized(1280, 800));
        assert_eq!(state.mode, LayoutMode::ThreePane);
        let _ = update(&mut state, Message::WindowResized(800, 600));
        assert_eq!(state.mode, LayoutMode::TwoPane);
        let _ = update(&mut state, Message::WindowResized(320, 240));
        assert_eq!(state.mode, LayoutMode::OnePane);
    }
}
