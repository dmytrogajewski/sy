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
    time::Duration,
};

use iced::{
    alignment::{Horizontal, Vertical},
    event,
    mouse,
    widget::{button, column, container, mouse_area, row, text, Space},
    window::Id,
    Background, Border, Color, Element, Event, Font, Length, Padding, Point, Subscription, Task,
};
use iced_layershell::{
    actions::IcedNewPopupSettings,
    build_pattern::daemon,
    reexport::{Anchor, Layer},
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
const SLOT_HEIGHT: u16 = 24;
const POPUP_WIDTH: u32 = 160;
const POPUP_HEIGHT: u32 = 200;
const TICK_MS: u64 = 1000;

/// Nerd Font (JetBrainsMono Nerd Font is installed system-wide for the rice;
/// this name lets fontconfig resolve it). Used for the type-indicator glyphs.
const ICON_FONT: Font = Font::with_name("JetBrainsMono Nerd Font");

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotSource {
    Clip,
    Stack,
}

#[derive(Debug, Clone)]
struct PopupCtx {
    item_id: String,
    source: SlotSource,
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
            exclusive_zone: BAR_WIDTH as i32,
            anchor: Anchor::Right | Anchor::Top | Anchor::Bottom,
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
                                    (
                                        Anchor::Right | Anchor::Top | Anchor::Bottom,
                                        (1, 1),
                                        0,
                                    )
                                };
                                return Task::done(Msg::AnchorSizeChange {
                                    id: bid,
                                    anchor,
                                    size,
                                })
                                .chain(Task::done(Msg::ExclusiveZoneChange {
                                    id: bid,
                                    zone_size: zone,
                                }));
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
            let close_task = close_all_popups(bar);
            let popup_id = Id::unique();
            bar.popups.insert(
                popup_id,
                PopupCtx {
                    item_id: id.clone(),
                    source: source.clone(),
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
            // The bar's own surface is the first opened window we have NOT
            // pre-registered as a popup.
            if !bar.popups.contains_key(&id) && bar.bar_id.is_none() {
                bar.bar_id = Some(id);
            }
            Task::none()
        }
        Msg::WindowClosed(id) => {
            bar.popups.remove(&id);
            Task::none()
        }
        Msg::RawEvent(ev) => {
            if let Event::Mouse(mouse::Event::CursorMoved { position }) = ev {
                bar.cursor = position;
            }
            Task::none()
        }
        // Catch-all for layer-shell control messages emitted by us.
        _ => Task::none(),
    }
}

fn close_all_popups(bar: &mut Bar) -> Task<Msg> {
    let ids: Vec<Id> = bar.popups.keys().copied().collect();
    bar.popups.clear();
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
        popup_view(bar, &p.item_id, &p.source)
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

    // Clip section: VSCode "paste" codicon for text, file-media for image.
    for c in bar.clips.iter() {
        let g = if c.content_kind == "image" {
            "\u{eb58}" // codicon file-media
        } else {
            "\u{ebca}" // codicon paste / clippy
        };
        col = col.push(slot(
            g,
            bar.palette.fg,
            Msg::SlotLeftClicked {
                id: c.id.clone(),
                source: SlotSource::Clip,
            },
            Msg::SlotRightClicked {
                id: c.id.clone(),
                source: SlotSource::Clip,
            },
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
    v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    v
}

fn stack_slot<'a>(palette: &Palette, it: &'a Item) -> Element<'a, Msg> {
    let g = glyph_for_item(it);
    slot(
        g,
        palette.fg,
        Msg::SlotLeftClicked {
            id: it.id.clone(),
            source: SlotSource::Stack,
        },
        Msg::SlotRightClicked {
            id: it.id.clone(),
            source: SlotSource::Stack,
        },
    )
}

/// Build a single slot: a transparent button (icon-only, no tile background)
/// wrapped in a mouse_area for right-click detection.
///
/// No iced tooltip: that widget renders inside the parent surface and the
/// 28-px-wide bar makes it wrap to one char per line. Hover/discovery is
/// handled by left-click → preview window and right-click → action popup
/// (which carries a name/path header).
fn slot<'a>(
    glyph: &'static str,
    color: Color,
    on_left: Msg,
    on_right: Msg,
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
    mouse_area(btn).on_right_press(on_right).into()
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
    let line = container(Space::new().width(Length::Fixed(14.0)).height(Length::Fixed(1.0)))
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

/// Pick the type icon for a stack item — Codicons (VSCode), the natural
/// terminal-IDE feel.
///
/// File items pick by extension; content items pick by sniffed kind.
/// Every codepoint here was probed against the deployed JetBrainsMono Nerd
/// Font and confirmed to render.
fn glyph_for_item(it: &Item) -> &'static str {
    if let Some(p) = &it.path {
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        return match ext.as_str() {
            // codicon file-media (image)
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "svg" => "\u{eb58}",
            // codicon file-pdf
            "pdf" => "\u{eb97}",
            // codicon archive
            "zip" | "tar" | "gz" | "xz" | "bz2" | "7z" | "rar" => "\u{ea98}",
            // codicon file-code
            "rs" | "go" | "py" | "js" | "ts" | "c" | "h" | "cpp" | "hpp" | "sh" | "lua"
            | "rb" | "java" | "kt" | "swift" | "zig" => "\u{eae9}",
            // codicon file-text (markdown/plain)
            "md" | "rst" | "txt" | "log" => "\u{eb44}",
            // codicon settings-gear (config files)
            "toml" | "yaml" | "yml" | "json" | "kdl" | "ini" | "conf" | "cfg" | "env" => {
                "\u{eaf8}"
            }
            // codicon file (generic)
            _ => "\u{ea7b}",
        };
    }
    // Content items: codicon file-text / file-media / file-binary-ish.
    match it.content_kind.as_str() {
        "text" => "\u{eb44}",   // file-text
        "image" => "\u{eb58}",  // file-media
        "binary" => "\u{eae9}", // file-code (closest "raw bytes" feel)
        _ => "\u{ea7b}",        // file
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
        SlotSource::Clip => &[("copy", "copy"), ("preview", "preview"), ("remove", "remove")],
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
                    action: action,
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

fn style(bar: &Bar, _theme: &iced::Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: bar.palette.bg_soft,
        text_color: bar.palette.fg,
    }
}
