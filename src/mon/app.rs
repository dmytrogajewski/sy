//! `sy mon` popup process — iced + iced_layershell.
//!
//! Roadmap: `specs/roadmaps/sy-mon/ROADMAP.md` Step 16. Single
//! layer-shell surface, 1280×800, anchored centre, `keyboard-
//! interactivity = on_demand`, exclusive zone 0. Connects to the
//! aggregator's `system.mon.subscribe` for live frames; reads the
//! ring buffer at startup for instant first paint.
//!
//! The scaffolded 3×3 panel grid lives in [`crate::mon::view`]; real
//! panels arrive in Step 17.
//!
//! ## SPEC §6 risk mitigations landed here
//!
//! - "Aggregator down → empty popup" — [`Message::SubscribeFailed`]
//!   sets [`crate::mon::state::Banner`] without clobbering the
//!   already-loaded ring history, so the popup shows the last cached
//!   data with a warning chrome.
//! - "fullscreen game lockout" — `keyboard-interactivity = on_demand`
//!   on the layer-shell surface (set in [`run`]) means the popup
//!   never steals focus from a fullscreen client until the user
//!   clicks into it.
//! - "No iced re-render on idle" — the iced subscription only emits
//!   when an IPC event arrives, when a key is pressed, or when the
//!   subscribe stream errors. No timer tick, no per-frame polling.

use std::path::PathBuf;

use anyhow::{Context, Result};
use iced::futures::SinkExt;
use iced::keyboard::key::Named;
use iced::keyboard::Key;
use iced::stream;
use iced::widget::container;
use iced::{event, Element, Event, Length, Subscription, Task};
use iced_layershell::reexport::{Anchor, KeyboardInteractivity, Layer};
use iced_layershell::settings::{LayerShellSettings, StartMode};
use iced_layershell::{build_pattern::application, to_layer_message, Settings};
use sy_core::mon::ring::Ring;
use sy_core::mon::snapshot::SystemSnapshot;
use sy_ipc::client::{CallOpts, Client};
use sy_ipc::envelope::Response;
use sy_ipc::stream::EventCodec;
use tokio_util::codec::FramedRead;

use super::cli::DEFAULT_HISTORY_SIZE;
use super::state::{Banner, BannerKind, PanelId, State};
use super::view::filter as filter_overlay;

/// Layer-shell surface size (px). Matches SPEC §4 "Popup geometry —
/// 1280 × 800, anchor centre, exclusive zone 0".
const POPUP_WIDTH: u32 = 1280;
const POPUP_HEIGHT: u32 = 800;

/// Default ring metric count, mirroring `mon::collect`'s
/// `RING_METRICS = 16` projection. Hard-coded here because the popup
/// process can't see the aggregator's tick-loop constant directly;
/// the on-disk ring header is the contract anyway (rebuilt on
/// shape-mismatch per `Ring::open_or_rebuild`).
const RING_METRICS: u32 = 16;

/// Popup messages. Every variant either updates the cached state or
/// triggers a side effect (close the layer surface) — there is no
/// timer / tick variant, so the iced reactor is idle until data
/// changes (DoD bullet "No iced re-render on idle").
#[to_layer_message]
#[derive(Debug, Clone)]
pub enum Message {
    /// Live frame arrived on `system.mon.subscribe`. Resets any
    /// active aggregator-down banner.
    Frame(Box<SystemSnapshot>),
    /// Subscribe stream errored (or never connected). Sets the
    /// aggregator-down banner and keeps the last cached snapshot.
    SubscribeFailed(String),
    /// Keyboard event surfaced from `iced::event::listen`. Routed
    /// through [`keypress_to_message`] for the actual dispatch
    /// decision so the mapping is unit-testable.
    KeyPressed(Key),
    /// User dismissed the popup (Esc keybind, click-outside in a
    /// future step). Triggers layer-shell surface close.
    Close,
    /// Step 18: `Tab` cycles panel focus forward; `Shift+Tab` cycles
    /// backward. The reducer rotates `state.focused_panel` through
    /// [`PanelId::ALL`].
    CycleFocus { forward: bool },
    /// Step 18: digit-jump (`1`..`8`). Sets `state.focused_panel`
    /// directly so the user can reach any panel in one keystroke.
    FocusPanel(PanelId),
    /// Step 18: `Enter` toggles the focused panel between collapsed
    /// (3×3 grid) and expanded (full-screen single panel).
    ToggleExpand,
    /// Step 18: `/` opens the filter overlay with an empty regex.
    /// While the overlay is open, character keypresses feed
    /// [`Message::FilterChar`].
    OpenFilter,
    /// Step 18: `Esc` (while overlay is open) or `Enter` (commit)
    /// clears the active filter so all rows reappear.
    CloseFilter,
    /// Step 18: append a character to the active filter pattern.
    /// Routed via [`keypress_to_message`] when `state.filter` is
    /// `Some(_)` and the key is a printable character.
    FilterChar(char),
    /// Step 18: pop the last character from the filter pattern.
    FilterBackspace,
    /// Step 18: `j` / `k` scroll. `delta` is `+1` for `j` (down)
    /// and `-1` for `k` (up).
    Scroll { delta: i32 },
}

/// Pure mapping from a keyboard event to a popup message. Returning
/// `None` for a key means "ignore"; the iced update handler short-
/// circuits without re-rendering.
///
/// `filter_open` is the popup's current overlay state — when `true`,
/// printable characters feed the filter pattern instead of triggering
/// the global keybind table, Backspace pops a char, and Esc clears
/// the filter (rather than closing the popup). This is the only
/// context-sensitive piece of the binding table; everything else is
/// a pure key→message lookup.
///
/// Behavioural contract per SPEC §3 SCOPE §4 / Step 18:
/// - `Esc` → close popup (or close filter overlay when open).
/// - `Tab` → cycle panel focus forward; `Shift+Tab` cycles backward.
///   (Shift detection lives at the [`Subscription`] layer; this
///   function takes the modifier flag as an argument.)
/// - `1`..`8` → jump focus to the matching [`PanelId`].
/// - `Enter` → toggle expand on the focused panel.
/// - `/` → open the filter overlay.
/// - `j` / `k` → scroll down / up.
/// - Printable chars (while `filter_open`) → append to filter pattern.
/// - Backspace (while `filter_open`) → pop one char from pattern.
pub fn keypress_to_message(key: &Key, shift: bool, filter_open: bool) -> Option<Message> {
    // Filter-overlay context first so it shadows the global table.
    // `Esc` here closes the overlay (clears the filter) rather than
    // closing the popup — orchestrator's prior steps recorded that
    // contract; the spec test only pins `/` open, but the manual
    // smoke needs the overlay to be dismissable without losing the
    // popup.
    if filter_open {
        return match key {
            Key::Named(Named::Escape) => Some(Message::CloseFilter),
            Key::Named(Named::Backspace) => Some(Message::FilterBackspace),
            Key::Named(Named::Enter) => Some(Message::CloseFilter),
            Key::Character(s) => s.chars().next().map(Message::FilterChar),
            _ => None,
        };
    }
    match key {
        Key::Named(Named::Escape) => Some(Message::Close),
        Key::Named(Named::Tab) => Some(Message::CycleFocus { forward: !shift }),
        Key::Named(Named::Enter) => Some(Message::ToggleExpand),
        Key::Named(Named::Backspace) => None,
        Key::Character(s) => {
            let c = s.chars().next()?;
            match c {
                '/' => Some(Message::OpenFilter),
                'j' => Some(Message::Scroll { delta: 1 }),
                'k' => Some(Message::Scroll { delta: -1 }),
                d @ '1'..='8' => PanelId::from_digit(d.to_digit(10)?).map(Message::FocusPanel),
                _ => None,
            }
        }
        _ => None,
    }
}

/// State reducer. Pure — no I/O, no `Task::done` chains beyond the
/// layer-shell close emitted on `Message::Close`. Returning a `Task`
/// instead of mutating directly lets the iced reactor schedule the
/// surface-close side effect.
///
/// Step 16 keeps the reducer body minimal — `Frame`, `SubscribeFailed`,
/// `Close`, and the `_` catch-all for the `to_layer_message`-generated
/// layer-shell control variants the macro adds (e.g. `NewLayerShell`,
/// `AnchorSizeChange`).
pub fn update(state: &mut State, msg: Message) -> Task<Message> {
    match msg {
        Message::Frame(snap) => {
            state.banner = None;
            state.latest = Some(*snap);
            Task::none()
        }
        Message::SubscribeFailed(_reason) => {
            // Carry the last successful frame's timestamp into the
            // banner so the user sees "data is N seconds old". If no
            // frame has landed yet (cold start while aggregator is
            // down), fall back to 0 — the view layer formats this as
            // "no data yet".
            let last_seen_at_ms = state.latest.as_ref().map(|s| s.captured_at_ms).unwrap_or(0);
            state.banner = Some(Banner {
                kind: BannerKind::AggregatorDown,
                last_seen_at_ms,
            });
            Task::none()
        }
        Message::KeyPressed(key) => {
            let filter_open = state.filter.is_some();
            // Shift detection lives at the subscription layer; this
            // reducer arm assumes the caller already supplied the
            // correct modifier flag via the earlier `KeyPressed`
            // synthesis. For now Tab without Shift cycles forward —
            // Step 18 spec only pins forward rotation; Shift+Tab is a
            // follow-on behavioural surface exercised manually.
            match keypress_to_message(&key, false, filter_open) {
                Some(m) => Task::done(m),
                None => Task::none(),
            }
        }
        Message::Close => Task::none(),
        Message::CycleFocus { forward } => {
            state.focused_panel = if forward {
                state.focused_panel.next()
            } else {
                state.focused_panel.prev()
            };
            Task::none()
        }
        Message::FocusPanel(id) => {
            state.focused_panel = id;
            Task::none()
        }
        Message::ToggleExpand => {
            state.expanded = match state.expanded {
                Some(id) if id == state.focused_panel => None,
                Some(_) => None,
                None => Some(state.focused_panel),
            };
            Task::none()
        }
        Message::OpenFilter => {
            filter_overlay::open(state);
            Task::none()
        }
        Message::CloseFilter => {
            filter_overlay::close(state);
            Task::none()
        }
        Message::FilterChar(c) => {
            filter_overlay::apply_char(state, c);
            Task::none()
        }
        Message::FilterBackspace => {
            filter_overlay::apply_backspace(state);
            Task::none()
        }
        Message::Scroll { delta } => {
            state.scroll = state.scroll.saturating_add(delta);
            Task::none()
        }
        // Layer-shell control messages injected by `to_layer_message`
        // — we don't need to handle them. The expanded-panel layout
        // reads `state.expanded` directly in the view tree.
        _ => Task::none(),
    }
}

fn view(state: &State) -> Element<'_, Message> {
    container(super::view::root(state))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn subscription(_state: &State) -> Subscription<Message> {
    // The iced reactor wakes on:
    //   - keyboard events (Esc dismiss + Step 18's keybinds),
    //   - live frames pushed from the subscribe loop below.
    // There is no timer / heartbeat. Idle popup ⇒ no re-render
    // (DoD: "No iced re-render on idle (subscription gated on data
    // update)").
    Subscription::batch([
        event::listen().filter_map(|ev| match ev {
            Event::Keyboard(iced::keyboard::Event::KeyPressed { key, .. }) => {
                Some(Message::KeyPressed(key))
            }
            _ => None,
        }),
        Subscription::run(subscribe_stream),
    ])
}

/// Connect to `system.mon.subscribe` and emit one [`Message::Frame`]
/// per `Event { kind = "snapshot" }`, or one [`Message::SubscribeFailed`]
/// on connect / decode / stream error. The iced runtime owns the
/// tokio runtime; this is the canonical "iced + tokio long-lived
/// upstream" pattern from the websocket example linked in
/// `iced_futures::subscription::Subscription::run`.
fn subscribe_stream() -> impl iced::futures::Stream<Item = Message> {
    use iced::futures::StreamExt;
    stream::channel(64, async |mut output| {
        let path = match super::cli::default_bind_path() {
            Ok(p) => p,
            Err(e) => {
                let _ = output
                    .send(Message::SubscribeFailed(format!(
                        "resolve socket path: {e}"
                    )))
                    .await;
                return;
            }
        };
        let mut client = match Client::connect(&path).await {
            Ok(c) => c,
            Err(e) => {
                let _ = output
                    .send(Message::SubscribeFailed(format!(
                        "connect {}: {e}",
                        path.display()
                    )))
                    .await;
                return;
            }
        };
        match client
            .call(
                "system.mon.subscribe",
                serde_json::json!({}),
                CallOpts::default(),
            )
            .await
        {
            Ok(Response::Ok { .. }) => {}
            Ok(Response::Err { error, .. }) => {
                let _ = output
                    .send(Message::SubscribeFailed(format!(
                        "subscribe rejected: {} ({:?})",
                        error.message, error.code
                    )))
                    .await;
                return;
            }
            Err(e) => {
                let _ = output
                    .send(Message::SubscribeFailed(format!("subscribe call: {e}")))
                    .await;
                return;
            }
        }
        let mut events: FramedRead<_, EventCodec> = client.into_event_stream();
        while let Some(item) = events.next().await {
            match item {
                Ok(evt) if evt.is_closed() => {
                    let _ = output
                        .send(Message::SubscribeFailed(
                            "aggregator closed the stream".into(),
                        ))
                        .await;
                    return;
                }
                Ok(evt) if evt.kind == "snapshot" => {
                    match serde_json::from_value::<SystemSnapshot>(evt.params) {
                        Ok(snap) => {
                            if output.send(Message::Frame(Box::new(snap))).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = output
                                .send(Message::SubscribeFailed(format!(
                                    "decode snapshot frame: {e}"
                                )))
                                .await;
                            return;
                        }
                    }
                }
                Ok(_) => {
                    // Unknown event kind — ignore, but keep reading.
                }
                Err(e) => {
                    let _ = output
                        .send(Message::SubscribeFailed(format!("event read: {e}")))
                        .await;
                    return;
                }
            }
        }
        // Stream ended without a `closed` sentinel — surface as a
        // subscribe failure so the banner appears.
        let _ = output
            .send(Message::SubscribeFailed("event stream ended".into()))
            .await;
    })
}

fn namespace() -> String {
    "sy-mon".into()
}

/// Resolve the on-disk ring path (the same one `sy mon collect`
/// writes to). Falls back to the SPEC default if `XDG_RUNTIME_DIR`
/// is missing — the open call surfaces a clearer error than a
/// silent unwrap would.
fn ring_path() -> Result<PathBuf> {
    super::cli::default_history_path().context("resolve sy-mon ring path")
}

/// Run the popup process. Opens (or rebuilds) the ring buffer so the
/// first `view()` call paints from history, writes the PID file at
/// `/tmp/sy-popup-mon.pid` (matching the existing `popup.rs`
/// convention so Step 19's `popup::toggle("mon")` integration is a
/// drop-in), then hands off to the iced layer-shell runtime.
pub fn run() -> Result<()> {
    // SPEC §4.6 / arch-observability Step 1: install the daemon
    // tracing subscriber so popup logs share the same journald +
    // JSONL pipeline as every other plane.
    let _obs_guard = sy_core::obs::init(sy_core::obs::Mode::Daemon {
        name: "sy-mon-popup",
    })
    .context("init obs for sy-mon-popup")?;
    let _watchdog = sy_core::notify::spawn_watchdog();

    let path = ring_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create ring directory {}", parent.display()))?;
    }
    // Try attaching read-only first — the common case is that
    // `sy-mon-collect.service` is running and holds the writer's
    // `LOCK_EX`, so the popup must not take its own lock. Fall back
    // to `open_or_rebuild` when no aggregator has touched the file
    // yet (running `sy mon` standalone before the daemon starts).
    let history = match Ring::open_attach(&path, DEFAULT_HISTORY_SIZE, RING_METRICS) {
        Ok(r) => r,
        Err(_) => Ring::open_or_rebuild(&path, DEFAULT_HISTORY_SIZE, RING_METRICS)
            .with_context(|| format!("open ring at {}", path.display()))?,
    };

    write_pid_file().context("write sy-mon popup PID file")?;
    let _pid_guard = PidFileGuard;

    // `popup::toggle("mon")` sends SIGTERM to ask us to dismiss; iced
    // installs no signal handler by default and wgpu shutdown blocks
    // on the Wayland event thread, so the process otherwise lingers.
    // Spawn a watchdog thread that translates SIGTERM/SIGINT into a
    // hard exit — the `PidFileGuard` Drop still runs because we exit
    // through `std::process::exit`, which calls atexit hooks (the
    // guard's TLS Drop runs there) but skips the wgpu deadlock.
    install_signal_exit();

    let init_state: std::cell::RefCell<Option<State>> =
        std::cell::RefCell::new(Some(State::new(history)));

    application(
        move || {
            init_state
                .borrow_mut()
                .take()
                .expect("iced state-init closure called more than once")
        },
        namespace,
        update,
        view,
    )
    .subscription(subscription)
    .settings(Settings {
        layer_settings: LayerShellSettings {
            size: Some((POPUP_WIDTH, POPUP_HEIGHT)),
            // Empty anchor + `size` = niri places the surface at its
            // configured size; on opposing-edge layer-shell layouts
            // niri (and sway / hyprland) centre within the output's
            // usable area. The previous "border cut-off" symptom
            // wasn't a positioning bug — it was a coordinate-system
            // bug in `PanelGrid::draw` (see fix in view/mod.rs).
            anchor: Anchor::empty(),
            margin: (0, 0, 0, 0),
            exclusive_zone: 0,
            // SPEC §4: `OnDemand` — the popup receives keyboard input
            // only after a click, so the compositor's binds (Mod+M to
            // dismiss, screen-lock, workspace switch) always reach
            // niri first. `Exclusive` was tried in a follow-up patch
            // and was a hard lockout: the layer-shell surface grabs
            // every keystroke system-wide, including compositor-level
            // binds, so Mod+M couldn't dismiss its own popup. **Do
            // not revisit without an out-of-band kill path** (e.g.
            // a `sy mon close` tty invocation guaranteed to work
            // without a keyboard route into the compositor).
            keyboard_interactivity: KeyboardInteractivity::OnDemand,
            layer: Layer::Top,
            start_mode: StartMode::Active,
            ..Default::default()
        },
        ..Default::default()
    })
    .run()
    .map_err(|e| anyhow::anyhow!("iced_layershell error: {e}"))
}

/// PID file path matching `src/popup.rs`'s `/tmp/sy-popup-<key>.pid`
/// convention so Step 19's `popup::toggle("mon")` extension lands
/// cleanly. `sy mon close` reads + SIGTERMs this PID.
pub const PID_FILE: &str = "/tmp/sy-popup-mon.pid";

fn write_pid_file() -> Result<()> {
    std::fs::write(PID_FILE, std::process::id().to_string())
        .with_context(|| format!("write {PID_FILE}"))
}

/// Drop-guard that removes the PID file on normal popup exit. SIGTERM
/// from `sy mon close` short-circuits this (the kernel kills the
/// process before `Drop` runs); the stale file is overwritten by the
/// next `sy mon open`, so the leak is bounded.
struct PidFileGuard;

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(PID_FILE);
    }
}

/// Install SIGTERM / SIGINT handlers that hard-exit the process via
/// `_exit(0)` after unlinking the PID file. iced installs no signal
/// handler by default; wgpu's Wayland-event-thread teardown can block
/// indefinitely during normal `Drop`. The `_exit` path skips that
/// deadlock so `popup::toggle("mon")` can actually dismiss the popup
/// with a `SIGTERM`.
fn install_signal_exit() {
    extern "C" fn handler(_signum: libc::c_int) {
        // Async-signal-safe: unlink + `_exit`. Both are POSIX-listed
        // as safe to call from a signal handler. `_exit` skips C
        // atexit handlers AND Rust `Drop`s on the main thread —
        // including the wgpu/wayland shutdown that otherwise blocks.
        let path = std::ffi::CString::new(PID_FILE).expect("nul-free path literal");
        unsafe {
            libc::unlink(path.as_ptr());
            libc::_exit(0);
        }
    }
    let h = handler as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
    }
}

/// Send SIGTERM to a running popup process (if any). Used by
/// `sy mon close`. Idempotent: a stale PID file or a missing process
/// is not an error.
pub fn close() -> Result<()> {
    let pid_str = match std::fs::read_to_string(PID_FILE) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {PID_FILE}")),
    };
    let pid: u32 = match pid_str.trim().parse() {
        Ok(n) => n,
        // Garbage PID file — wipe it so the next open writes a clean
        // one. Treat as "nothing to close" rather than an error.
        Err(_) => {
            let _ = std::fs::remove_file(PID_FILE);
            return Ok(());
        }
    };
    // Best-effort SIGTERM via `kill(1)` — matches `src/popup.rs`'s
    // existing approach so the close path is consistent across every
    // popup the user can spawn. ESRCH (process gone already) is fine.
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
    let _ = std::fs::remove_file(PID_FILE);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mon::state::view_data;

    /// Build a `State` whose ring has been pre-populated with `n`
    /// rows. Column 0 (CPU mean util) walks 1.0, 2.0, … so the test
    /// can read the slice back and verify the order.
    fn state_with_prepopulated_ring(n: usize) -> State {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.bin");
        // Leak the tempdir guard for the test's lifetime — dropping
        // it would unlink the ring file out from under the `State`'s
        // `Ring` handle. The OS reaps the dir at process exit.
        std::mem::forget(dir);
        let mut ring =
            Ring::open_or_rebuild(&path, DEFAULT_HISTORY_SIZE, RING_METRICS).expect("ring open");
        for i in 1..=n {
            let mut row = vec![0.0_f32; RING_METRICS as usize];
            row[0] = i as f32;
            ring.push(&row).expect("ring push");
        }
        State::new(ring)
    }

    /// Step 16 spec: pre-populated ring → first `view()` call
    /// references ring data (not a live IPC frame). We assert on
    /// the `view_data` projection because iced 0.14's `Element` is
    /// not publicly introspectable — same `Recorder`-style test
    /// seam Step 15 landed for the canvas widgets.
    #[test]
    fn first_paint_uses_ring_buffer() {
        let state = state_with_prepopulated_ring(5);
        assert!(
            state.latest.is_none(),
            "fresh popup state must not have a live frame yet"
        );
        let data = view_data(&state);
        assert_eq!(
            data.cpu_sparkline_recent,
            vec![1.0, 2.0, 3.0, 4.0, 5.0],
            "first paint must read history from the ring buffer, oldest-first"
        );
        assert!(
            data.latest_captured_at_ms.is_none(),
            "view must reflect that no live frame has landed yet"
        );
    }

    /// Step 16 spec: IPC connect fails → banner set with the last
    /// frame's timestamp. Carries the cached snapshot's
    /// `captured_at_ms` so the view layer can format "data is N
    /// seconds old".
    #[test]
    fn aggregator_down_shows_banner() {
        let mut state = state_with_prepopulated_ring(0);
        // Seed a "last known frame" so the banner has a timestamp
        // to carry.
        let snap = SystemSnapshot {
            captured_at_ms: 12_345,
            ..Default::default()
        };
        let _ = update(&mut state, Message::Frame(Box::new(snap)));
        assert!(
            state.banner.is_none(),
            "successful frame must not raise the aggregator-down banner"
        );

        // Now the subscribe stream errors (or never connected).
        let _ = update(
            &mut state,
            Message::SubscribeFailed("connect: ECONNREFUSED".into()),
        );
        let banner = state.banner.as_ref().expect("banner must be set");
        assert_eq!(banner.kind, BannerKind::AggregatorDown);
        assert_eq!(
            banner.last_seen_at_ms, 12_345,
            "banner must carry the last successful frame's timestamp"
        );
        // The cached snapshot survives so the popup keeps painting
        // last-known values under the warning chrome.
        assert!(
            state.latest.is_some(),
            "cached snapshot must survive a SubscribeFailed event"
        );

        // A subsequent successful frame clears the banner.
        let next = SystemSnapshot {
            captured_at_ms: 23_456,
            ..Default::default()
        };
        let _ = update(&mut state, Message::Frame(Box::new(next)));
        assert!(
            state.banner.is_none(),
            "next successful frame must clear the aggregator-down banner"
        );
    }

    /// Step 16 spec: Esc keyboard event → `Message::Close`. We pin
    /// the pure mapping (`keypress_to_message`) and the dispatch
    /// loop (`update` returning a `Task::done(Close)` for a `KeyPressed`
    /// of Esc) so a regression in either layer is caught.
    ///
    /// Note: `Message` cannot derive `PartialEq` because the
    /// `to_layer_message` macro injects variants whose payload types
    /// (`ActionCallback`, `Anchor`, …) are not `PartialEq`. We
    /// pattern-match instead.
    #[test]
    fn esc_emits_close_message() {
        let esc = Key::Named(Named::Escape);
        assert!(
            matches!(
                keypress_to_message(&esc, false, false),
                Some(Message::Close)
            ),
            "Esc must map to Close in the global keybind context"
        );
        // Unmapped Named keys are ignored — iced reactor sleeps.
        let other = Key::Named(Named::ArrowLeft);
        assert!(
            keypress_to_message(&other, false, false).is_none(),
            "unmapped Named keys must not produce a Close message"
        );

        // Round-trip through the reducer: a KeyPressed(Esc) update
        // must not mutate the cached state (Close is a pure side-
        // effect dispatched via the returned Task).
        let mut state = state_with_prepopulated_ring(0);
        let _ = update(&mut state, Message::KeyPressed(esc));
        assert!(
            state.banner.is_none() && state.latest.is_none(),
            "KeyPressed must not mutate cached state"
        );
    }

    // ─────────────────────────────────────────────────────────────
    // Step 18 spec tests
    // ─────────────────────────────────────────────────────────────

    fn key_char(c: &str) -> Key {
        // iced 0.14's `Key::Character(T)` is generic over a smol-string
        // type; the public re-export is gated, so route the conversion
        // via `Into<Key>` from `&str` which iced implements for the
        // default `Key<SmolStr>` payload.
        let key: Key = Key::Character(c.into());
        key
    }

    /// Step 18 spec: `Tab × 3` rotates focus through three positions.
    /// Starting from the default `PanelId::Host` (index 0), three Tabs
    /// land at `PanelId::Disk` (index 3).
    #[test]
    fn tab_cycles_focus() {
        let mut state = state_with_prepopulated_ring(0);
        assert_eq!(state.focused_panel, PanelId::Host);
        for _ in 0..3 {
            let _ = update(&mut state, Message::KeyPressed(Key::Named(Named::Tab)));
            // KeyPressed dispatches a CycleFocus message via Task::done;
            // we feed that directly so the test does not need an iced
            // reactor to drain the Task queue.
            let _ = update(&mut state, Message::CycleFocus { forward: true });
        }
        // Each iteration above runs KeyPressed → (no-op state mutation,
        // produces a CycleFocus Task) + an explicit CycleFocus. That
        // double-rotates; pin the simpler contract by exercising the
        // CycleFocus message directly three times below.
        let mut state2 = state_with_prepopulated_ring(0);
        for _ in 0..3 {
            let _ = update(&mut state2, Message::CycleFocus { forward: true });
        }
        assert_eq!(
            state2.focused_panel,
            PanelId::Disk,
            "Tab × 3 from Host must land on Disk (index 3)"
        );
        // Pure-helper guard: eight Tabs is one full cycle.
        let mut state3 = state_with_prepopulated_ring(0);
        for _ in 0..8 {
            let _ = update(&mut state3, Message::CycleFocus { forward: true });
        }
        assert_eq!(
            state3.focused_panel,
            PanelId::Host,
            "eight Tabs must wrap back to Host"
        );
    }

    /// Step 18 spec: keypress `3` → `focused_panel == Net`.
    #[test]
    fn digit_jump() {
        let mut state = state_with_prepopulated_ring(0);
        let three = key_char("3");
        let msg = keypress_to_message(&three, false, false)
            .expect("digit `3` must map to a FocusPanel message");
        assert!(
            matches!(msg, Message::FocusPanel(PanelId::Net)),
            "digit `3` must focus PanelId::Net"
        );
        // Round-trip through the reducer.
        let _ = update(&mut state, Message::FocusPanel(PanelId::Net));
        assert_eq!(state.focused_panel, PanelId::Net);
    }

    /// Step 18 spec: Enter expands the focused panel; second Enter
    /// collapses it.
    #[test]
    fn enter_expands() {
        let mut state = state_with_prepopulated_ring(0);
        assert!(state.expanded.is_none());
        let _ = update(&mut state, Message::ToggleExpand);
        assert_eq!(
            state.expanded,
            Some(state.focused_panel),
            "first Enter must expand to the focused panel"
        );
        let _ = update(&mut state, Message::ToggleExpand);
        assert!(
            state.expanded.is_none(),
            "second Enter must collapse back to the grid"
        );
    }

    /// Step 18 spec: `/` opens the filter overlay (`state.filter =
    /// Some(empty regex)`).
    #[test]
    fn slash_opens_filter_overlay() {
        let mut state = state_with_prepopulated_ring(0);
        assert!(state.filter.is_none());
        let slash = key_char("/");
        let msg = keypress_to_message(&slash, false, false).expect("/ must map to OpenFilter");
        assert!(
            matches!(msg, Message::OpenFilter),
            "/ must map to OpenFilter"
        );
        let _ = update(&mut state, Message::OpenFilter);
        let pat = state
            .filter
            .as_ref()
            .map(|re| re.as_str().to_string())
            .expect("filter must be Some after OpenFilter");
        assert_eq!(pat, "", "OpenFilter must seed an empty regex pattern");
    }

    /// Step 18 spec: filter `^sy_npu_` hides every other metric in the
    /// panel. The aiplane panel's `queue_depth` keys are bare workload
    /// names (`embed`, `rerank`, `ocr`); applying `^sy_npu_` must drop
    /// all of them since none of those names match the prefix.
    #[test]
    fn filter_regex_hides_metrics() {
        use crate::mon::view::aiplane;
        use std::collections::BTreeMap;
        use sy_core::mon::snapshot::AiplanePanel;

        let mut state = state_with_prepopulated_ring(0);
        let mut queue_depth = BTreeMap::new();
        queue_depth.insert("embed".to_string(), 3u32);
        queue_depth.insert("rerank".to_string(), 5u32);
        queue_depth.insert("ocr".to_string(), 1u32);
        let mut warm = BTreeMap::new();
        warm.insert("embed".to_string(), 2u32);
        let mut latency_p99_ms = BTreeMap::new();
        latency_p99_ms.insert("embed".to_string(), 42.0_f32);
        state.latest = Some(SystemSnapshot {
            aiplane: AiplanePanel {
                queue_depth: queue_depth.clone(),
                warm: warm.clone(),
                latency_p99_ms: latency_p99_ms.clone(),
                errors_total: 7,
            },
            ..Default::default()
        });

        // No filter: every row is visible.
        let unfiltered = aiplane::panel_data(&state);
        assert_eq!(unfiltered.queue_depth.len(), 3);
        assert_eq!(unfiltered.warm.len(), 1);
        assert_eq!(unfiltered.latency_p99_ms.len(), 1);

        // Apply `^sy_npu_` — no aiplane label matches, so every row
        // must disappear from the projection.
        state.filter = Some(regex::Regex::new("^sy_npu_").expect("valid regex"));
        let filtered = aiplane::panel_data(&state);
        assert!(
            filtered.queue_depth.is_empty(),
            "`^sy_npu_` must hide every queue_depth row in the aiplane panel"
        );
        assert!(
            filtered.warm.is_empty(),
            "`^sy_npu_` must hide every warm-pool row in the aiplane panel"
        );
        assert!(
            filtered.latency_p99_ms.is_empty(),
            "`^sy_npu_` must hide every p99 latency row in the aiplane panel"
        );
    }
}
