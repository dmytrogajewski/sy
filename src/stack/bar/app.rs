//! iced + iced_layershell bar — vertical thin always-on strip on the right edge.
//!
//! Multi-window: the main bar is a layer-shell surface; right-clicking a slot
//! spawns an XDG-popup child window anchored to the cursor, themed identically,
//! containing the action buttons (copy / preview / move / link / onto / agent
//! / remove). Sub-pickers (which folder? which agent?) shell out to fuzzel
//! since those lists are dynamic.
//!
//! Glyphs are content-type indicators rendered in a single fg color so the
//! bar reads as "what's here" not "who owns it" (pool is communicated by
//! section position: clip on top, then app, then user).
//!
//! Tick (every 1s):
//!   - Refresh cliphist top 8
//!   - Reload items.json (cheap; small file)
//!   - Drain IPC ops queue (refresh / toggle / reload-theme)

use std::{
    collections::HashMap,
    process::Command,
    sync::{
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use iced::{
    alignment::{Horizontal, Vertical},
    event, mouse,
    widget::{button, column, container, mouse_area, row, text, Space},
    window::Id,
    Background, Border, Color, Element, Event, Font, Length, Padding, Point, Subscription, Task,
};
use iced_layershell::{
    actions::IcedNewPopupSettings,
    build_pattern::daemon,
    reexport::{Anchor, KeyboardInteractivity, Layer, NewLayerShellSettings, OutputOption},
    settings::{LayerShellSettings, StartMode},
    to_layer_message, Settings,
};

use super::theme::Palette;
use crate::stack::{
    clip::{self, ClipEntry},
    ipc::{self, Op},
    state::{self, Item, Items},
    Kind,
};

/// Width of the bar's vertical strip on the right edge.
/// `configs/waybar/config.jsonc` reserves the same width via
/// `"margin-right": 28` so waybar and the bar share the screen without
/// fighting for the top-right corner.
const BAR_WIDTH: u32 = 28;
/// Waybar's `"height"` from `configs/waybar/config.jsonc`. Used by
/// the hover-preview surface to translate the bar-local cursor y
/// back to screen-space (the bar's surface starts at y=24).
const WAYBAR_HEIGHT_PX: i32 = 24;
/// Top offset applied to the bar's surface via the layer-shell
/// `margin` request. Held at 0 because niri DOES deduct waybar's
/// 24-px top exclusive zone from this surface's top edge (the bar
/// is anchored `Right | Top | Bottom`, waybar anchors `Top | Left |
/// Right`, so they share the top edge). The niri spawn-at-startup
/// for `sy stack bar` waits until waybar is up before launching
/// the bar — guaranteeing the deduction is in place at registration
/// time. Any non-zero value here on top of niri's deduction
/// double-counts and the bar ends up at y=24+margin.
const WAYBAR_TOP_MARGIN: i32 = 0;
const SLOT_HEIGHT: u16 = 24;
const POPUP_WIDTH: u32 = 160;
const POPUP_HEIGHT: u32 = 200;
/// Width of the hover preview popup. Wider than the action menu so
/// 24 lines of monospace text and a 256-px image thumbnail both fit
/// without horizontal scrolling.
const HOVER_POPUP_WIDTH: u32 = 320;
/// Height of the hover preview popup. Sized to fit a 256×256 image
/// thumbnail plus a 32-px header strip.
const HOVER_POPUP_HEIGHT: u32 = 300;
/// Edge length of the thumbnail rendered inside the hover popup
/// (vs. the 20-px inline thumbnails on the bar itself).
const HOVER_THUMB_PX: f32 = 256.0;
/// Lines of body text shown in the hover popup for text/code items
/// before the `…` ellipsis marker kicks in.
const HOVER_TEXT_MAX_LINES: usize = 24;
/// How long the user must keep the cursor over a slot before the
/// hover popup opens. Matches GNOME's tooltip delay.
const HOVER_DEBOUNCE_MS: u64 = 250;
/// Polling interval for the hover debounce. Cheaper than spinning
/// up a fresh per-slot subscription on every enter/exit.
const HOVER_TICK_MS: u64 = 50;
const TICK_MS: u64 = 1000;

/// Nerd Font (JetBrainsMono Nerd Font is installed system-wide for the rice;
/// this name lets fontconfig resolve it). Used for the type-indicator glyphs.
const ICON_FONT: Font = Font::with_name("JetBrainsMono Nerd Font");

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotSource {
    Clip,
    Stack,
}

/// Tracks the slot currently under the cursor while the hover-popup
/// debounce is pending. Constructed on `Msg::SlotHoverEnter`, cleared
/// on `Msg::SlotHoverExit` or when a right-click action popup takes
/// precedence.
#[derive(Debug, Clone)]
struct HoverArm {
    source: SlotSource,
    id: String,
    armed_at: Instant,
}

/// Discriminator stored alongside the popup id so `view()` can route
/// to the action-menu layout or the hover-preview layout without
/// inspecting the `PopupCtx` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupKind {
    Action,
    Hover,
}

#[derive(Debug, Clone)]
struct PopupCtx {
    item_id: String,
    source: SlotSource,
    kind: PopupKind,
}

struct Bar {
    palette: Palette,
    clips: Vec<ClipEntry>,
    items: Items,
    visible: bool,
    ipc_rx: Arc<Mutex<Receiver<Op>>>,
    /// The auto-created layer-shell window's id, captured from the first
    /// `WindowOpened` event. None until then; toggle is a no-op until set
    /// (which happens before the first user input).
    bar_id: Option<Id>,
    /// Currently-open popup windows keyed by their layer-shell Id. We close
    /// any existing popup before opening a new one, so this is at most 1
    /// entry — but using a map keeps the lookup in `view()` uniform.
    popups: HashMap<Id, PopupCtx>,
    /// Last known cursor position relative to the bar's surface (parent
    /// origin). Updated from mouse-move events; used to position the
    /// right-click popup so it opens leftward into the screen interior
    /// instead of off-screen to the right of the bar.
    cursor: Point,
    /// Slot currently under the cursor while the hover-popup debounce
    /// is pending. Set on `SlotHoverEnter`, cleared on `SlotHoverExit`
    /// or when an action popup opens.
    hover_armed: Option<HoverArm>,
    /// Layer-shell id of the active hover popup, if any. Distinct
    /// from `popups` so the debounce tick knows whether one is
    /// already shown (avoid spawning a second).
    hover_popup: Option<Id>,
}

#[to_layer_message(multi)]
#[derive(Debug, Clone)]
enum Msg {
    Tick,
    SlotLeftClicked {
        id: String,
        source: SlotSource,
    },
    /// User right-clicked a slot. Bar handles popup creation in update().
    SlotRightClicked {
        id: String,
        source: SlotSource,
    },
    /// User clicked an action inside the popup.
    MenuAction {
        id: String,
        source: SlotSource,
        action: &'static str,
    },
    ClosePopup,
    /// Fired once per layer-shell surface that opens, including the bar
    /// itself. Used to capture `bar_id` for toggle.
    WindowOpened(Id),
    WindowClosed(Id),
    /// Raw iced event — we only inspect mouse-move so we know where the
    /// cursor is when the user right-clicks a slot.
    RawEvent(Event),
    /// Cursor entered a slot — arms the hover-popup debounce.
    SlotHoverEnter {
        id: String,
        source: SlotSource,
    },
    /// Cursor left a slot — disarms the debounce and closes any open
    /// hover popup for that slot.
    SlotHoverExit {
        id: String,
        source: SlotSource,
    },
    /// Polled every `HOVER_TICK_MS` while a popup may need to open.
    /// Checks the armed slot's age against `HOVER_DEBOUNCE_MS`.
    HoverDebounceTick,
}

/// Arm the hover-popup state for a slot. A fast sweep across
/// multiple slots only ever has one debounce timer running because
/// the new arm overwrites the prior one. Re-arming for the *same*
/// slot is a no-op so iced's `mouse_area::on_enter` re-firing
/// across frames (while the cursor stays inside the widget bounds)
/// doesn't keep resetting `armed_at` — without this, the 250-ms
/// threshold is effectively never reached on any hover after the
/// first. Returns `true` when this call introduced a *new* slot,
/// `false` when it was a no-op re-fire — the update handler uses
/// that to decide whether to close any popup left over from a
/// previous slot.
fn arm_hover(state: &mut Option<HoverArm>, source: SlotSource, id: String) -> bool {
    if let Some(a) = state {
        if a.source == source && a.id == id {
            return false;
        }
    }
    *state = Some(HoverArm {
        source,
        id,
        armed_at: Instant::now(),
    });
    true
}

/// Disarm only if the currently-armed slot matches the (source, id)
/// that fired the exit. An exit for slot A while we're armed on B
/// (after a fast sweep) must not cancel the B timer.
fn disarm_hover_if_matches(state: &mut Option<HoverArm>, source: &SlotSource, id: &str) {
    if let Some(a) = state {
        if &a.source == source && a.id == id {
            *state = None;
        }
    }
}

/// Returns the currently-armed slot if the debounce threshold has
/// elapsed since `armed_at`. Caller checks `bar.hover_popup` to
/// decide whether to spawn or skip.
fn hover_should_fire(state: &Option<HoverArm>, threshold: Duration) -> Option<HoverArm> {
    state
        .as_ref()
        .filter(|a| a.armed_at.elapsed() >= threshold)
        .cloned()
}

/// Debug-trace hover events to stderr when `SY_STACK_BAR_TRACE` is
/// set. No-op in production runs so the bar stays quiet.
fn trace_hover(msg: &str) {
    if std::env::var_os("SY_STACK_BAR_TRACE").is_some() {
        eprintln!("sy-stack[hover]: {msg}");
    }
}

pub fn run() -> anyhow::Result<()> {
    // Spawn IPC listener; ops land on a channel we drain inside `update`.
    let (tx, rx) = mpsc::channel();
    let _ = ipc::serve(tx);
    let rx = Arc::new(Mutex::new(rx));

    let palette = super::theme::load().unwrap_or_default();
    let init_rx = rx.clone();
    let init_palette = palette.clone();

    daemon(
        move || Bar {
            palette: init_palette.clone(),
            clips: Vec::new(),
            items: state::load().unwrap_or_default(),
            visible: true,
            ipc_rx: init_rx.clone(),
            bar_id: None,
            popups: HashMap::new(),
            cursor: Point::ORIGIN,
            hover_armed: None,
            hover_popup: None,
        },
        namespace,
        update,
        view,
    )
    .style(style)
    .subscription(subscription)
    .settings(Settings {
        layer_settings: LayerShellSettings {
            size: Some((BAR_WIDTH, 0)),
            // Reserve the 28-px right strip so niri's tiling and
            // fullscreen don't put windows under the bar. Niri
            // accepts a positive value for our anchor combo
            // (`Right | Top | Bottom` — one edge + two
            // perpendiculars).
            exclusive_zone: BAR_WIDTH as i32,
            anchor: Anchor::Right | Anchor::Top | Anchor::Bottom,
            // (top, right, bottom, left) per layershellev's
            // `LayerSurface::set_margin` signature.
            // `WAYBAR_TOP_MARGIN = 0` defers vertical positioning
            // to niri's deduction of waybar's top exclusive zone.
            margin: (WAYBAR_TOP_MARGIN, 0, 0, 0),
            layer: Layer::Top,
            start_mode: StartMode::Active,
            ..Default::default()
        },
        ..Default::default()
    })
    .run()
    .map_err(|e| anyhow::anyhow!("iced_layershell error: {e}"))
}

fn namespace() -> String {
    "sy-stack".into()
}

fn subscription(_: &Bar) -> Subscription<Msg> {
    Subscription::batch(vec![
        iced::time::every(Duration::from_millis(TICK_MS)).map(|_| Msg::Tick),
        iced::time::every(Duration::from_millis(HOVER_TICK_MS)).map(|_| Msg::HoverDebounceTick),
        iced::window::open_events().map(Msg::WindowOpened),
        iced::window::close_events().map(Msg::WindowClosed),
        event::listen().map(Msg::RawEvent),
    ])
}

fn update(bar: &mut Bar, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Tick => {
            // Drain IPC ops first.
            if let Ok(rx) = bar.ipc_rx.lock() {
                while let Ok(op) = rx.try_recv() {
                    match op {
                        Op::Refresh => {}
                        Op::Toggle => {
                            bar.visible = !bar.visible;
                            // Resize the bar surface; note: needs the bar's Id.
                            if let Some(bid) = bar.bar_id {
                                let (anchor, size, zone) = if bar.visible {
                                    (
                                        Anchor::Right | Anchor::Top | Anchor::Bottom,
                                        (BAR_WIDTH, 0),
                                        BAR_WIDTH as i32,
                                    )
                                } else {
                                    (Anchor::Right | Anchor::Top | Anchor::Bottom, (1, 1), 0)
                                };
                                return Task::done(Msg::AnchorSizeChange {
                                    id: bid,
                                    anchor,
                                    size,
                                })
                                .chain(Task::done(
                                    Msg::ExclusiveZoneChange {
                                        id: bid,
                                        zone_size: zone,
                                    },
                                ));
                            }
                        }
                        Op::ReloadTheme => {
                            bar.palette = super::theme::load().unwrap_or_default();
                        }
                    }
                }
            }
            // Refresh data.
            bar.clips = clip::top(8);
            bar.items = state::load().unwrap_or_default();
            Task::none()
        }
        Msg::SlotLeftClicked { id, source } => {
            // Default = copy (the most common action). Preview is in the
            // right-click menu. notify-send pings so the user sees feedback.
            spawn_action(&id, "copy", &source);
            let _ = Command::new("notify-send")
                .args(["-t", "1200", "-a", "sy-stack", "sy stack", "copied"])
                .spawn();
            Task::none()
        }
        Msg::SlotRightClicked { id, source } => {
            // Close any existing popup first (we keep at most one open).
            // The action popup takes priority over any pending hover,
            // so cancel the debounce too.
            bar.hover_armed = None;
            let close_task = close_all_popups(bar);
            let popup_id = Id::unique();
            bar.popups.insert(
                popup_id,
                PopupCtx {
                    item_id: id.clone(),
                    source: source.clone(),
                    kind: PopupKind::Action,
                },
            );
            let actions_count = popup_actions(&source).len() as u32;
            let height = (actions_count * 28 + 80).max(POPUP_HEIGHT);
            // Position the popup to the LEFT of the bar (toward screen
            // interior). Bar is right-anchored, so a negative x in
            // bar-local coords places the popup just outside the bar's
            // left edge. Y follows the cursor so it appears next to the
            // slot the user actually clicked.
            let x = -(POPUP_WIDTH as i32) - 4;
            let y = (bar.cursor.y as i32 - 8).max(0);
            close_task.chain(Task::done(Msg::NewPopUp {
                settings: IcedNewPopupSettings {
                    size: (POPUP_WIDTH, height),
                    position: (x, y),
                },
                id: popup_id,
            }))
        }
        Msg::MenuAction { id, source, action } => {
            spawn_action(&id, action, &source);
            close_all_popups(bar)
        }
        Msg::ClosePopup => close_all_popups(bar),
        Msg::WindowOpened(id) => {
            let in_popups = bar.popups.contains_key(&id);
            // The bar's own surface is the first opened window we have NOT
            // pre-registered as a popup.
            if !in_popups && bar.bar_id.is_none() {
                bar.bar_id = Some(id);
            }
            trace_hover(&format!("WindowOpened id={id:?} in_popups={in_popups}"));
            Task::none()
        }
        Msg::WindowClosed(id) => {
            let was_hover = bar.hover_popup == Some(id);
            bar.popups.remove(&id);
            if was_hover {
                bar.hover_popup = None;
            }
            trace_hover(&format!(
                "WindowClosed id={id:?} was_hover={was_hover} popups_len={}",
                bar.popups.len(),
            ));
            Task::none()
        }
        Msg::RawEvent(ev) => {
            if let Event::Mouse(mouse::Event::CursorMoved { position }) = ev {
                bar.cursor = position;
            }
            Task::none()
        }
        Msg::SlotHoverEnter { id, source } => {
            let armed_new_slot = arm_hover(&mut bar.hover_armed, source.clone(), id.clone());
            trace_hover(&format!(
                "ENTER id={id} src={source:?} new_slot={armed_new_slot} hover_popup={:?} popups_len={}",
                bar.hover_popup,
                bar.popups.len(),
            ));
            // If the user moved to a different slot, kill any
            // hover popup that's still alive from the previous
            // slot. iced's `mouse_area::on_exit` does not fire
            // while the XDG popup holds the input grab, so we
            // can't rely on the exit handler alone to clean up.
            if armed_new_slot {
                if let Some(pid) = bar.hover_popup.take() {
                    bar.popups.remove(&pid);
                    return Task::done(Msg::RemoveWindow(pid));
                }
            }
            Task::none()
        }
        Msg::SlotHoverExit { id, source } => {
            trace_hover(&format!(
                "EXIT id={id} src={source:?} hover_popup={:?} hover_armed={:?}",
                bar.hover_popup, bar.hover_armed,
            ));
            disarm_hover_if_matches(&mut bar.hover_armed, &source, &id);
            // Also close any hover popup that belongs to this slot.
            if let Some(popup_id) = bar.hover_popup {
                let matches = bar
                    .popups
                    .get(&popup_id)
                    .map(|p| p.item_id == id && p.source == source)
                    .unwrap_or(false);
                if matches {
                    bar.popups.remove(&popup_id);
                    bar.hover_popup = None;
                    return Task::done(Msg::RemoveWindow(popup_id));
                }
            }
            Task::none()
        }
        Msg::HoverDebounceTick => {
            // Self-heal: a popup that vanished from the popups map
            // (e.g. compositor dismissed the XDG popup grab without
            // firing WindowClosed, or our SlotHoverExit removed it
            // first) leaves `hover_popup` pointing at a dead id.
            // Clear so the next debounce can open a fresh popup.
            if let Some(pid) = bar.hover_popup {
                if !bar.popups.contains_key(&pid) {
                    trace_hover(&format!("TICK self-heal: clearing stale hover_popup={pid:?}"));
                    bar.hover_popup = None;
                }
            }
            if bar.hover_popup.is_some() {
                return Task::none();
            }
            let Some(arm) =
                hover_should_fire(&bar.hover_armed, Duration::from_millis(HOVER_DEBOUNCE_MS))
            else {
                return Task::none();
            };
            let popup_id = Id::unique();
            trace_hover(&format!(
                "TICK open popup_id={popup_id:?} for arm={arm:?}"
            ));
            bar.popups.insert(
                popup_id,
                PopupCtx {
                    item_id: arm.id,
                    source: arm.source,
                    kind: PopupKind::Hover,
                },
            );
            bar.hover_popup = Some(popup_id);
            // The hover preview is a fresh layer-shell surface, not
            // an XDG popup. XDG popups via `Msg::NewPopUp` re-derive
            // their parent from `ev.current_surface_id()` in
            // layershellev; after the first popup closes, focus
            // stays pinned to the destroyed popup's surface, so the
            // *second* NewPopUp is silently dropped at
            // `layershellev/src/lib.rs:3189` (the `continue;` when
            // no non-popup unit matches). A standalone layer-shell
            // surface has no parent dependency, so the lifecycle is
            // reliable across repeated hovers.
            //
            // Anchor top+right, margin chosen so the surface sits
            // immediately left of the bar at the cursor's y.
            // `bar.cursor.y` is bar-local; add waybar height (24) to
            // map back to screen-space, clamp ≥ waybar height so we
            // never overlap waybar.
            let cursor_screen_y = bar.cursor.y as i32 + WAYBAR_HEIGHT_PX;
            let margin_top = (cursor_screen_y - 8).max(WAYBAR_HEIGHT_PX);
            Task::done(Msg::NewLayerShell {
                settings: NewLayerShellSettings {
                    size: Some((HOVER_POPUP_WIDTH, HOVER_POPUP_HEIGHT)),
                    layer: Layer::Overlay,
                    anchor: Anchor::Top | Anchor::Right,
                    exclusive_zone: Some(0),
                    margin: Some((margin_top, BAR_WIDTH as i32 + 4, 0, 0)),
                    keyboard_interactivity: KeyboardInteractivity::None,
                    output_option: OutputOption::LastOutput,
                    events_transparent: false,
                    namespace: Some("sy-stack-hover".into()),
                },
                id: popup_id,
            })
        }
        // Catch-all for layer-shell control messages emitted by us.
        _ => Task::none(),
    }
}

fn close_all_popups(bar: &mut Bar) -> Task<Msg> {
    let ids: Vec<Id> = bar.popups.keys().copied().collect();
    bar.popups.clear();
    bar.hover_popup = None;
    let mut t = Task::none();
    for id in ids {
        t = t.chain(Task::done(Msg::RemoveWindow(id)));
    }
    t
}

fn spawn_action(id: &str, action: &str, source: &SlotSource) {
    // Detached spawn — UI doesn't block on the action's output.
    let src = match source {
        SlotSource::Clip => "clip",
        SlotSource::Stack => "stack",
    };
    let _ = Command::new("sy")
        .args(["stack", "action", id, action, "--source", src])
        .spawn();
}

fn view(bar: &Bar, id: Id) -> Element<'_, Msg> {
    // Popups are the only entries we track; everything else is the bar.
    if let Some(p) = bar.popups.get(&id) {
        match p.kind {
            PopupKind::Action => popup_view(bar, &p.item_id, &p.source),
            PopupKind::Hover => hover_preview_view(bar, &p.item_id, &p.source),
        }
    } else {
        bar_view(bar)
    }
}

fn bar_view(bar: &Bar) -> Element<'_, Msg> {
    if !bar.visible {
        return container(Space::new().width(1).height(1))
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    // No horizontal padding on the column — every child uses Length::Fill
    // and centers its own content, so the icon column and separators share
    // the exact same horizontal extent (the full bar width).
    let mut col = column![].spacing(4).padding([2, 0]);

    // Clip section: image entries render an inline 20×20 thumbnail
    // when cliphist's preview names the extension; text entries
    // (and image entries with no decodable ext) keep the codicon
    // fallback so the bar never goes blank. `resolve_glyph_color`
    // keeps the non-image branch in lockstep with stack slots so
    // theme changes propagate uniformly.
    for c in bar.clips.iter() {
        let left = Msg::SlotLeftClicked {
            id: c.id.clone(),
            source: SlotSource::Clip,
        };
        let right = Msg::SlotRightClicked {
            id: c.id.clone(),
            source: SlotSource::Clip,
        };
        let enter = Msg::SlotHoverEnter {
            id: c.id.clone(),
            source: SlotSource::Clip,
        };
        let exit = Msg::SlotHoverExit {
            id: c.id.clone(),
            source: SlotSource::Clip,
        };
        let (g, key) = if c.content_kind == "image" {
            ("\u{eb58}", ColorKey::Image)
        } else {
            ("\u{ebca}", ColorKey::Text)
        };
        col = col.push(slot(
            g,
            resolve_glyph_color(&bar.palette, key),
            left,
            right,
            enter,
            exit,
        ));
    }

    col = col.push(separator(&bar.palette));

    for it in by_kind(&bar.items, Kind::App) {
        col = col.push(stack_slot(&bar.palette, it));
    }

    col = col.push(separator(&bar.palette));

    for it in by_kind(&bar.items, Kind::User) {
        col = col.push(stack_slot(&bar.palette, it));
    }

    container(col)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn by_kind(items: &Items, kind: Kind) -> Vec<&Item> {
    let mut v: Vec<&Item> = items.items.iter().filter(|i| i.kind == kind).collect();
    v.sort_by_key(|i| std::cmp::Reverse(i.created_at));
    v
}

fn stack_slot<'a>(palette: &Palette, it: &'a Item) -> Element<'a, Msg> {
    let (g, key) = glyph_for_item(it);
    let left = Msg::SlotLeftClicked {
        id: it.id.clone(),
        source: SlotSource::Stack,
    };
    let right = Msg::SlotRightClicked {
        id: it.id.clone(),
        source: SlotSource::Stack,
    };
    let enter = Msg::SlotHoverEnter {
        id: it.id.clone(),
        source: SlotSource::Stack,
    };
    let exit = Msg::SlotHoverExit {
        id: it.id.clone(),
        source: SlotSource::Stack,
    };
    slot(
        g,
        resolve_glyph_color(palette, key),
        left,
        right,
        enter,
        exit,
    )
}

/// Build a single slot: a transparent button (icon-only, no tile background)
/// wrapped in a mouse_area that routes left-click, right-click, and
/// pointer enter/exit through to `update()`.
///
/// We don't use iced's `tooltip` widget — it renders inside the parent
/// surface and the 28-px-wide bar wraps it to one char per line.
/// Hover discovery is provided by the debounced hover popup spawned
/// from `SlotHoverEnter`; the right-click action popup remains the
/// canonical "open for real" affordance.
fn slot<'a>(
    glyph: &'static str,
    color: Color,
    on_left: Msg,
    on_right: Msg,
    on_enter: Msg,
    on_exit: Msg,
) -> Element<'a, Msg> {
    let label = text(glyph)
        .font(ICON_FONT)
        .color(color)
        .size(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Center);
    let btn: Element<Msg> = button(label)
        .on_press(on_left)
        .padding(0)
        .width(Length::Fill)
        .height(Length::Fixed(SLOT_HEIGHT as f32))
        .style(transparent_button)
        .into();
    mouse_area(btn)
        .on_right_press(on_right)
        .on_enter(on_enter)
        .on_exit(on_exit)
        .into()
}

/// Slot button: no fill, no border, fg-colored icon. Hover lightens via the
/// container background of the bar itself rather than the button.
fn transparent_button(_: &iced::Theme, status: button::Status) -> button::Style {
    let dim = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: if dim {
            Some(Background::Color(Color {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.06,
            }))
        } else {
            None
        },
        text_color: Color::WHITE,
        border: Border::default(),
        ..button::Style::default()
    }
}

fn separator(palette: &Palette) -> Element<'_, Msg> {
    // 1px solid line in fg, narrower than the bar. The Codicon glyphs sit
    // a couple of pixels right of their advance-box centre; we offset the
    // line by the same amount so it lines up visually with the icons.
    const VISUAL_OFFSET_RIGHT: f32 = 4.0; // moves the line +2 px relative to centre
    let c = palette.fg;
    let line = container(
        Space::new()
            .width(Length::Fixed(14.0))
            .height(Length::Fixed(1.0)),
    )
    .style(move |_t: &iced::Theme| container::Style {
        background: Some(Background::Color(c)),
        border: Border::default(),
        ..Default::default()
    });
    row![
        Space::new().width(Length::Fill),
        line,
        Space::new().width(Length::Fill),
    ]
    .width(Length::Fill)
    .padding(Padding::ZERO.left(VISUAL_OFFSET_RIGHT))
    .into()
}

/// Content-family classification used to colour-code slot glyphs.
///
/// Mapped to a concrete `Color` by `resolve_glyph_color` against the
/// active `Palette`. Reusing the palette's pre-existing accent set
/// (`blue`/`orange`/`fg_dim`) keeps themed setups consistent without
/// inventing new schema fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorKey {
    Text,
    Code,
    Image,
    Archive,
    Config,
    File,
}

/// Pick the type icon and colour-key for a stack item — Codicons
/// (VSCode), the natural terminal-IDE feel.
///
/// File items pick by extension; content items pick by sniffed kind.
/// Every codepoint here was probed against the deployed JetBrainsMono Nerd
/// Font and confirmed to render. The returned `ColorKey` feeds
/// `resolve_glyph_color` so the bar communicates type at a glance.
fn glyph_for_item(it: &Item) -> (&'static str, ColorKey) {
    if let Some(p) = &it.path {
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        return match ext.as_str() {
            // codicon file-media (image)
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "svg" => {
                ("\u{eb58}", ColorKey::Image)
            }
            // codicon file-pdf
            "pdf" => ("\u{eb97}", ColorKey::File),
            // codicon archive
            "zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" => ("\u{ea98}", ColorKey::Archive),
            // codicon file-code
            "rs" | "go" | "py" | "js" | "ts" | "c" | "h" | "cpp" | "hpp" | "sh" | "lua" | "rb"
            | "java" | "kt" | "swift" | "zig" => ("\u{eae9}", ColorKey::Code),
            // codicon file-text (markdown/plain)
            "md" | "rst" | "txt" | "log" => ("\u{eb44}", ColorKey::Text),
            // codicon settings-gear (config files)
            "toml" | "yaml" | "yml" | "json" | "kdl" | "ini" | "conf" | "cfg" | "env" => {
                ("\u{eaf8}", ColorKey::Config)
            }
            // codicon file (generic)
            _ => ("\u{ea7b}", ColorKey::File),
        };
    }
    // Content items: codicon file-text / file-media / file-binary-ish.
    match it.content_kind.as_str() {
        "text" => ("\u{eb44}", ColorKey::Text),
        "image" => ("\u{eb58}", ColorKey::Image),
        "binary" => ("\u{eae9}", ColorKey::Code),
        _ => ("\u{ea7b}", ColorKey::File),
    }
}

/// Resolve a `ColorKey` against the active palette. Code → blue,
/// archive → orange (gruvbox-material's warm accent; reads well next
/// to blue including under dichromat conditions), config → fg_dim,
/// text/image/file → fg as the neutral default. Image stays neutral
/// here because Step 3 swaps the glyph for an inline thumbnail.
fn resolve_glyph_color(palette: &Palette, key: ColorKey) -> Color {
    match key {
        ColorKey::Code => palette.blue,
        ColorKey::Archive => palette.orange,
        ColorKey::Config => palette.fg_dim,
        ColorKey::Text | ColorKey::Image | ColorKey::File => palette.fg,
    }
}

fn short(s: &str) -> String {
    let mut out: String = s.chars().take(200).collect();
    if s.chars().count() > 200 {
        out.push('…');
    }
    out
}

fn popup_actions(source: &SlotSource) -> &'static [(&'static str, &'static str)] {
    match source {
        SlotSource::Clip => &[
            ("copy", "copy"),
            ("preview", "preview"),
            ("remove", "remove"),
        ],
        SlotSource::Stack => &[
            ("copy", "copy"),
            ("preview", "preview"),
            ("move to…", "move"),
            ("link", "link"),
            ("onto…", "onto"),
            ("call agent…", "agent"),
            ("remove", "remove"),
        ],
    }
}

fn popup_view<'a>(bar: &'a Bar, item_id: &'a str, source: &'a SlotSource) -> Element<'a, Msg> {
    // Header: show what the user is acting on.
    //   stack item with path → file basename + dim full path
    //   stack content item   → name + dim head of payload
    //   clip entry           → "clip" + dim preview
    let (title, sub) = match source {
        SlotSource::Clip => {
            let preview = bar
                .clips
                .iter()
                .find(|c| c.id == item_id)
                .map(|c| short(&c.preview))
                .unwrap_or_default();
            ("clipboard".to_string(), preview)
        }
        SlotSource::Stack => bar
            .items
            .items
            .iter()
            .find(|i| i.id == item_id)
            .map(|i| {
                let sub = i
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| {
                        state::read_payload(&i.id)
                            .ok()
                            .and_then(|b| String::from_utf8(b).ok())
                            .map(|s| short(&s))
                            .unwrap_or_default()
                    });
                (i.name.clone(), sub)
            })
            .unwrap_or_else(|| (format!("item {item_id}"), String::new())),
    };

    let mut col = column![
        text(short(&title)).size(12).color(bar.palette.fg),
        text(sub).size(10).color(bar.palette.fg_dim),
        Space::new().width(Length::Fill).height(Length::Fixed(4.0)),
    ]
    .spacing(2)
    .padding(8);

    for (label_text, action) in popup_actions(source) {
        col = col.push(
            button(text(*label_text).size(12).color(bar.palette.fg))
                .on_press(Msg::MenuAction {
                    id: item_id.to_string(),
                    source: source.clone(),
                    action,
                })
                .padding([4, 8])
                .width(Length::Fill)
                .style(transparent_button),
        );
    }

    col = col.push(
        button(text("close").size(11).color(bar.palette.fg_dim))
            .on_press(Msg::ClosePopup)
            .padding([4, 8])
            .width(Length::Fill)
            .style(transparent_button),
    );

    let pal = bar.palette.clone();
    container(col)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_t: &iced::Theme| container::Style {
            background: Some(Background::Color(pal.bg)),
            border: Border {
                color: pal.fg_dim,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// Render the hover-preview popup body. Branches on the slot's
/// content type:
///   - image (stack item or cliphist binary) → 256×256 thumbnail.
///   - stack content item with `content_kind = "text"` → first
///     `HOVER_TEXT_MAX_LINES` lines of the stored payload.
///   - stack file item with a readable path → first lines of the
///     file (best-effort; binary reads fall through to metadata).
///   - cliphist text entry → the preview string already on the
///     `ClipEntry`.
///   - anything else → name + path + size + mtime header.
fn hover_preview_view<'a>(
    bar: &'a Bar,
    item_id: &'a str,
    source: &'a SlotSource,
) -> Element<'a, Msg> {
    let pal = bar.palette.clone();
    let body: Element<Msg> = match source {
        SlotSource::Clip => hover_clip_body(bar, item_id),
        SlotSource::Stack => hover_stack_body(bar, item_id),
    };
    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(8)
        .style(move |_t: &iced::Theme| container::Style {
            background: Some(Background::Color(pal.bg)),
            border: Border {
                color: pal.fg_dim,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn hover_clip_body<'a>(bar: &'a Bar, id: &'a str) -> Element<'a, Msg> {
    let c = match bar.clips.iter().find(|c| c.id == id) {
        Some(c) => c,
        None => return text("(clip entry vanished)").color(bar.palette.fg_dim).into(),
    };
    if let Some(ext) = c.image_ext {
        if let Ok(thumb) = clip::decode_to_thumb(&c.id, ext, HOVER_THUMB_PX as u32) {
            return iced::widget::image(iced::widget::image::Handle::from_path(thumb))
                .width(Length::Fixed(HOVER_THUMB_PX))
                .height(Length::Fixed(HOVER_THUMB_PX))
                .into();
        }
    }
    text(short(&c.preview))
        .size(11)
        .font(Font::MONOSPACE)
        .color(bar.palette.fg)
        .into()
}

fn hover_stack_body<'a>(bar: &'a Bar, id: &'a str) -> Element<'a, Msg> {
    let it = match bar.items.items.iter().find(|i| i.id == id) {
        Some(it) => it,
        None => return text("(stack item vanished)").color(bar.palette.fg_dim).into(),
    };
    let (_, key) = glyph_for_item(it);
    if matches!(key, ColorKey::Image) {
        if let Ok(Some(thumb)) = state::thumbnail_path(it, HOVER_THUMB_PX as u32) {
            return iced::widget::image(iced::widget::image::Handle::from_path(thumb))
                .width(Length::Fixed(HOVER_THUMB_PX))
                .height(Length::Fixed(HOVER_THUMB_PX))
                .into();
        }
    }
    if let Some(bytes) = read_item_text(it) {
        return text(state::text_preview(&bytes, HOVER_TEXT_MAX_LINES))
            .size(11)
            .font(Font::MONOSPACE)
            .color(bar.palette.fg)
            .into();
    }
    let header = it.name.clone();
    let sub = it
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    column![
        text(short(&header)).size(12).color(bar.palette.fg),
        text(short(&sub)).size(10).color(bar.palette.fg_dim),
        text(format!("{} bytes", it.size))
            .size(10)
            .color(bar.palette.fg_dim),
    ]
    .spacing(2)
    .into()
}

/// Best-effort read for the hover preview's text branch. Returns
/// `None` for items that aren't representable as UTF-8 text — the
/// caller falls through to a metadata-only view in that case.
fn read_item_text(it: &Item) -> Option<Vec<u8>> {
    if it.content_kind == "text" {
        return state::read_payload(&it.id).ok();
    }
    let p = it.path.as_ref()?;
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let textish = matches!(
        ext.as_str(),
        "rs" | "go"
            | "py"
            | "js"
            | "ts"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "sh"
            | "lua"
            | "rb"
            | "java"
            | "kt"
            | "swift"
            | "zig"
            | "md"
            | "rst"
            | "txt"
            | "log"
            | "toml"
            | "yaml"
            | "yml"
            | "json"
            | "kdl"
            | "ini"
            | "conf"
            | "cfg"
            | "env"
    );
    if !textish {
        return None;
    }
    std::fs::read(p).ok()
}

fn style(bar: &Bar, _theme: &iced::Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: bar.palette.bg_soft,
        text_color: bar.palette.fg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn waybar_top_margin_does_not_double_count_niri_deduction() {
        // Niri deducts waybar's 24-px top exclusive zone from this
        // surface's top edge automatically (the bar anchors `Right
        // | Top | Bottom`, waybar `Top | Left | Right` — shared top
        // edge). A non-zero margin on top of that puts the bar at
        // y=24+margin, leaving a visible gap. The compile-time
        // assertion guards against an accidental re-introduction of
        // a positive margin in this code path.
        const _: () = assert!(WAYBAR_TOP_MARGIN == 0);
    }

    fn file_item(name: &str) -> Item {
        Item {
            id: "fake".into(),
            kind: Kind::User,
            path: Some(PathBuf::from(name)),
            name: name.into(),
            created_at: 0,
            content_kind: "file".into(),
            size: 0,
        }
    }

    fn content_item(content_kind: &str) -> Item {
        Item {
            id: "fake".into(),
            kind: Kind::User,
            path: None,
            name: "snippet".into(),
            created_at: 0,
            content_kind: content_kind.into(),
            size: 0,
        }
    }

    #[test]
    fn glyph_for_item_rust_source_is_code() {
        let (_, key) = glyph_for_item(&file_item("main.rs"));
        assert_eq!(key, ColorKey::Code);
    }

    #[test]
    fn glyph_for_item_tarball_is_archive() {
        let (_, key) = glyph_for_item(&file_item("dump.tar.gz"));
        assert_eq!(key, ColorKey::Archive);
    }

    #[test]
    fn glyph_for_item_toml_is_config() {
        let (_, key) = glyph_for_item(&file_item("Cargo.toml"));
        assert_eq!(key, ColorKey::Config);
    }

    #[test]
    fn glyph_for_item_png_is_image() {
        let (_, key) = glyph_for_item(&file_item("snap.png"));
        assert_eq!(key, ColorKey::Image);
    }

    #[test]
    fn glyph_for_item_content_text_is_text() {
        let (_, key) = glyph_for_item(&content_item("text"));
        assert_eq!(key, ColorKey::Text);
    }

    #[test]
    fn hover_state_arms_on_enter_disarms_on_exit() {
        let mut s: Option<HoverArm> = None;
        arm_hover(&mut s, SlotSource::Stack, "abc".into());
        assert!(s.is_some());
        disarm_hover_if_matches(&mut s, &SlotSource::Stack, "abc");
        assert!(s.is_none());
    }

    #[test]
    fn arm_hover_is_idempotent_for_same_slot() {
        // iced's mouse_area::on_enter can fire across frames while
        // the cursor stays inside the widget. We must not reset
        // armed_at on re-fires, otherwise the 250-ms debounce
        // never elapses and the popup never reopens after the
        // first close.
        let mut s = None;
        arm_hover(&mut s, SlotSource::Stack, "a".into());
        let first_at = s.as_ref().unwrap().armed_at;
        std::thread::sleep(Duration::from_millis(5));
        arm_hover(&mut s, SlotSource::Stack, "a".into());
        let second_at = s.as_ref().unwrap().armed_at;
        assert_eq!(
            first_at, second_at,
            "re-arming for the same slot must NOT reset armed_at"
        );
    }

    #[test]
    fn hover_state_swaps_on_enter_of_different_slot() {
        let mut s = None;
        arm_hover(&mut s, SlotSource::Stack, "a".into());
        arm_hover(&mut s, SlotSource::Stack, "b".into());
        assert_eq!(s.as_ref().unwrap().id, "b");
        // Exit for slot A must not disarm; we're armed on B.
        disarm_hover_if_matches(&mut s, &SlotSource::Stack, "a");
        assert_eq!(s.as_ref().unwrap().id, "b");
    }

    #[test]
    fn hover_debounce_fires_only_when_still_hovering() {
        let mut s = None;
        arm_hover(&mut s, SlotSource::Stack, "a".into());
        // Threshold too high to have elapsed yet → no fire.
        assert!(hover_should_fire(&s, Duration::from_secs(60)).is_none());
        // Zero threshold → fires.
        assert!(hover_should_fire(&s, Duration::from_secs(0)).is_some());
        // Disarmed → no fire even with zero threshold.
        disarm_hover_if_matches(&mut s, &SlotSource::Stack, "a");
        assert!(hover_should_fire(&s, Duration::from_secs(0)).is_none());
    }

    #[test]
    fn resolve_glyph_color_distinguishes_code_archive_config() {
        // Three of the five non-image families must resolve to
        // distinct colours so a glance picks them apart on the bar.
        // Image stays at fg in Step 2 (replaced by a thumbnail in
        // Step 3); Text/File share fg as the neutral default.
        let p = Palette::default();
        let code = resolve_glyph_color(&p, ColorKey::Code);
        let archive = resolve_glyph_color(&p, ColorKey::Archive);
        let config = resolve_glyph_color(&p, ColorKey::Config);
        assert_ne!(code, archive);
        assert_ne!(code, config);
        assert_ne!(archive, config);
    }
}
