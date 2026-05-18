# SPEC: stack bar UX — hover previews, type-aware slots, under-waybar alignment

## 1. Summary

Three coupled improvements to the `sy stack bar` right-edge strip
(`src/stack/bar/app.rs`):

1. **Hover preview** — a debounced popup that shows the slot's payload
   (text snippet with monospace font; image thumbnail for images;
   syntax-aware preview for code) anchored to the cursor, dismissed
   on hover-out.
2. **Type-aware slot rendering** — slots visibly differ by content
   type: colored Codicon glyph for text/code/archive/config, and an
   inline 20×20 thumbnail for image slots so users recognise what's
   in a slot without hovering.
3. **Under-waybar alignment** — the bar's top edge sits at y=24
   (waybar height) instead of y=0, using the layer-shell `margin`
   primitive rather than relying on exclusive-zone deduction from
   waybar.

## 2. Background & Research

### Market Context

The Wayland clipboard-manager landscape splits cleanly into two
camps:

- **cliphist + fuzzel/rofi pickers** ([cliphist](https://github.com/sentriz/cliphist)).
  Headless store; pickers handle UI. Image preview is a contrib
  script ([cliphist-fuzzel-img](https://github.com/sentriz/cliphist/blob/master/contrib/cliphist-fuzzel-img))
  that generates thumbnails on demand into
  `$XDG_CACHE_HOME/cliphist/thumbnails/<id>.<ext>` and feeds fuzzel
  with `\0icon\x1f<path>` icon metadata. No hover preview — selection
  drives the action.
- **All-in-one TUI/GUIs** ([clipse](https://github.com/savedra1/clipse),
  [stash](https://github.com/NotAShelf/stash),
  [clipbox](https://www.linuxlinks.com/clipbox-clipboard-manager-wayland/),
  [Clyp](https://biggo.com/news/202508230115_Clyp_Wayland_Support_Issues)).
  Bundled pickers with image + text preview panes. UX modelled on
  Windows 11 / GNOME — full-screen overlay, keyboard-driven.
- **Klipper / CopyQ** ([Klipper](https://userbase.kde.org/Klipper), CopyQ).
  Long-standing desktop integrations; Klipper opens a panel anchored
  to its tray icon and uses hover-tooltips for entries; CopyQ uses a
  detail pane.

**Key takeaway:** every comparable product surfaces type and content
before the user commits to an action — either via preview pane,
icon-with-thumbnail, or hover tooltip. sy's bar currently uses a
single mono-fg Codicon glyph and a click-to-preview window, which
forces the user into a destructive-ish flow (mouse to slot, click,
wait for preview window, dismiss).

### Technical Context

`sy stack bar` is built on
[iced_layershell](https://docs.rs/iced_layershell/latest/iced_layershell/)
on top of [wlr-layer-shell](https://wayland.app/protocols/wlr-layer-shell-unstable-v1).

**Anchor + exclusive zone semantics** (per wlr-layer-shell spec):

- `exclusive_zone > 0` only takes effect on a surface anchored to one
  edge (or one edge + the two perpendicular edges). The current bar
  anchors `Right | Top | Bottom` — valid for right-edge exclusive.
- A new layer surface anchored to an edge that overlaps an existing
  exclusive zone gets pushed inward by that zone — *if the compositor
  honors it*. niri does, but only when the new surface's anchor
  doesn't include both perpendicular edges of the existing zone. Our
  bar anchors `Top + Bottom`; waybar anchors `Top + Left + Right`.
  Result: niri does not always deduct waybar's top zone from our
  bar's top edge, hence the user's screenshot.
- The protocol exposes a `margin` request that pushes the surface
  away from any anchored edge. `iced_layershell::LayerShellSettings`
  exposes this as `margin: (i32, i32, i32, i32)` in `(top, right,
  bottom, left)` order. This is *deterministic* — it does not depend
  on draw order or compositor anchor-deduction heuristics.

**iced popups + hover events.** The bar already uses XDG popups for
right-click action menus
(`src/stack/bar/app.rs:239` → `Msg::SlotRightClicked` →
`Msg::NewPopUp`). iced's
`widget::mouse_area` exposes `on_enter` and `on_exit`; the same
popup-creation primitive can be triggered on a debounced hover event
without changing the surface model.

**Image rendering.** iced ships `iced::widget::image::Image` with
PNG/JPEG/etc. via the `image` crate (no extra system deps; same crate
already in the workspace tree). For 20×20 inline slot thumbnails and
~256×256 hover thumbnails we need a one-shot resize on first display
and a tiny on-disk cache to avoid re-decoding the full image every
tick.

**cliphist binary-data detection.** Today
`src/stack/clip.rs:45-49` uses `preview.starts_with("[[ binary
data")` as a heuristic. cliphist's preview format is documented
(`<id>\t<100-char preview>`) and the binary-data prefix has been
stable across releases since the contrib script was written, so this
is fine but the binary entry doesn't tell us the *extension*. The
contrib script greps `(jpg|jpeg|png|bmp)` from the cliphist preview
string to learn the extension before invoking
`cliphist decode > thumbnails/<id>.<ext>` — we should adopt the same
extraction since we already need it for thumbnailing.

### Deep Dives

- **wlr-layer-shell margin includes exclusive zone, but only on
  edges the surface is anchored to.** The spec is explicit: setting
  margin on a non-anchored edge is a no-op. Our bar anchors top, so
  `margin.top` works deterministically. Drawn from
  [wayland.app/protocols/wlr-layer-shell-unstable-v1](https://wayland.app/protocols/wlr-layer-shell-unstable-v1).
- **iced_layershell popup vs. new layer surface.** Popups are XDG
  child surfaces of a parent layer surface and inherit its layer.
  They're cheaper than a new layer surface (no namespace, no
  reservation), and they auto-close when focus moves away — perfect
  for hover previews. The cost: position is parent-relative, so we
  need cursor-tracking (already done at
  `src/stack/bar/app.rs:284-288`).
- **Codicon glyph color is the cheapest type-signal.** Every slot
  already uses a Codicon (`glyph_for_item` at
  `src/stack/bar/app.rs:480-514`); the palette has `fg`, `fg_dim` —
  extending to a small set of accent colours per glyph family adds
  signal without changing layout. Image thumbnails go one step
  further: replace the glyph entirely for image slots with a
  decoded-and-resized image widget.

## 3. Proposal

### Approach

Land the three improvements in one feature:

1. Add a `margin` field to `LayerShellSettings` invocation in
   `src/stack/bar/app.rs:155-164`. Bind its top component to a new
   `WAYBAR_HEIGHT: u32 = 24` constant, optionally overridable from
   `sy.toml [stack.bar] top_margin = 24`.
2. Add hover-popup state to the `Bar` struct (debounce timer + last
   slot hovered + popup id). Wire `mouse_area::on_enter`/`on_exit`
   into new `Msg::SlotHover{Enter,Exit}` variants. Reuse the existing
   `NewPopUp` / `RemoveWindow` path with a new `popup_kind` tag so
   `view()` dispatches to `hover_preview_view` vs. `popup_view`.
3. Extend `glyph_for_item` to return `(glyph, color_key)` and look up
   colour by key from a new palette extension
   (`code`, `text`, `image`, `archive`, `config`, `file`).
4. Add a `state::thumbnail_path(&Item) -> Option<PathBuf>` helper
   that lazily writes a 20×20 PNG into
   `$XDG_CACHE_HOME/sy/stack/thumbs/<id>.png` for image slots, used
   by both the inline slot widget and the hover popup.
5. Apply the same heuristic for clipboard image entries: parse the
   extension out of cliphist's binary-data preview and feed
   `cliphist decode <id>` through the same thumbnailing path.

### Key Decisions

| Decision | Choice | Reasoning | Alternatives |
|----------|--------|-----------|--------------|
| Under-waybar alignment | `LayerShellSettings.margin = (24, 0, 0, 0)` | Deterministic; protocol-defined; does not depend on compositor anchor-deduction or spawn ordering — directly addresses the bug shown in the screenshot. | Drop `Anchor::Top` (loses full vertical extent); spawn delay (race-prone, already attempted via `spawn-at-startup` ordering and fails per the screenshot); reserve `WAYBAR_HEIGHT` via the bar's own exclusive zone (wrong axis — bar reserves right, not top). |
| Hover preview surface | XDG popup as child of the bar surface | Reuses the existing right-click popup mechanism; auto-closes on focus loss; cheap (no new layer-shell namespace). | New layer-shell `Overlay` surface (more code, namespace pollution, no auto-close); iced `tooltip` widget (rejected per the existing comment at `src/stack/bar/app.rs:399-405` — wraps to one char per line in the 28-px bar). |
| Image thumbnail pipeline | Lazy PNG resize via the `image` crate already in the tree, cached at `$XDG_CACHE_HOME/sy/stack/thumbs/<id>.png` | Single dep, deterministic output, cap-bounded by stack item count. Mirrors cliphist's contrib pattern but on-disk under XDG cache rather than ad hoc. | Use ImageMagick (extra dep, snowflake on minimal hosts); render at full size every tick (CPU); compose with iced's built-in resize (still re-decodes per repaint). |
| Hover debounce | 250 ms `iced::time::every` timer started on `on_enter`, cancelled on `on_exit` | Matches GNOME and KDE tooltip delays; long enough to ignore quick mouse passes, short enough to feel responsive. | Immediate (flickers when sweeping the bar); 500 ms (feels laggy per UX research on tooltip delays). |
| Type indicator approach | Colored glyph for non-image; inline thumbnail for image | Bar is 28 px wide; only image previews are worth the pixel budget. Code/text already distinguishable by Codicon — colour separates code/text/config families. | Per-type background tile (heavier visual weight, fights the existing minimal aesthetic); text snippet beside icon (won't fit in 28 px). |
| Code-snippet preview format | Plain monospace text, first 24 lines, no syntax highlighting | Cheap, deterministic, no new dep. Highlighting adds a `syntect`-sized dep for marginal benefit on a 256×256 popup. | `syntect` highlighting (heavyweight); shell-out to `bat --color=always` and ANSI-render (popup widget doesn't render ANSI). |

### ML (Minimum Loveable)

**IN:**
- `margin.top = 24` (constant first; sy.toml override second pass).
- Hover-on-slot → debounced popup with:
  - **text/code items** → first 24 lines, monospace, fg colour, fg_dim border.
  - **image items** → 256×256 thumbnail (aspect-preserved letterbox).
  - **file items (non-image, no payload to preview)** → name + path + size + mtime.
- Type-aware slot glyph colour for: text (fg), code (accent_blue),
  image (replaced by inline 20×20 thumbnail), archive (accent_yellow),
  config (fg_dim), file/binary (fg).
- Reused thumbnail cache for inline + hover view of the same image.

**OUT:**
- Syntax highlighting in the code preview.
- Animated hover transitions / fade.
- Hover preview for clipboard *file* entries (cliphist doesn't store
  files, only text + images).
- Multi-page preview for PDFs (use the existing right-click → preview
  to shell out to a real viewer).
- User-configurable hover delay (constant first; revisit if asked).
- Sensitive-content masking on hover.

### Anti-Goals

- **Do not replace cliphist** with a sy-native clipboard store. The
  cliphist mirror at `src/stack/clip.rs` is intentionally thin; owning
  the store ourselves duplicates a well-maintained tool and inflates
  scope.
- **No new layer-shell namespace** for the preview. Popups are
  enough; adding an `Overlay` surface complicates focus handling
  and increases the bar's surface count for no gain.
- **No syntax highlighting in v1.** `syntect` pulls 20+ crates; the
  user asked to *understand* type, not to read fully-rendered code in
  the popup.
- **No snowflake host edits.** Both the margin and the cache path
  flow through `configs/` (existing waybar config + new sy.toml
  block) and `$XDG_CACHE_HOME` — no manual install steps.

## 4. Technical Design

### Architecture

Files touched (all in `src/stack/`):

- **`bar/app.rs`** — most of the work:
  - Add `WAYBAR_HEIGHT` const, set in `LayerShellSettings.margin`.
  - Add `hover: Option<HoverState>` to `Bar`.
  - New `Msg::SlotHoverEnter / SlotHoverExit / HoverDebounce`.
  - New `popup_kind: PopupKind { Action, HoverPreview }` to dispatch
    in `view()`.
  - New `hover_preview_view()` that renders text/image/file based on
    `Item.content_kind` / `Item.path` / `ClipEntry`.
  - Wire `image::Handle` into image slots using
    `state::thumbnail_path`.
- **`bar/theme.rs`** — extend `Palette` with `accent_blue`,
  `accent_yellow`, `accent_green`. Defaults derived from existing
  `fg`/`fg_dim` (saturated variants).
- **`state.rs`** — add `thumbnail_path(item: &Item) -> Result<Option<PathBuf>>`
  and a small `thumbs_dir()` helper. Reuse `sniff_kind` for the
  extension table.
- **`clip.rs`** — extend `ClipEntry` with `image_ext: Option<&str>`
  parsed from the cliphist preview line (`(jpg|jpeg|png|bmp|webp|gif)`
  via a tiny regex). Add `decode_to_thumb(id, ext)` that materialises
  a thumbnail into the same cache path.
- **`configs/sy/sy.toml`** (or wherever the existing stack defaults
  live; check at implement time) — new `[stack.bar]` block:
  ```toml
  [stack.bar]
  # Top edge of the stack bar in pixels. Defaults to waybar height.
  top_margin = 24
  ```
  Optional; the constant default lands first.

Data flow:

```
hover-enter ─▶ start 250ms debounce
              │
              ▼ (on timer expiry, still hovering same slot)
              spawn XDG popup with kind=HoverPreview
              │
              ▼
              view() dispatches → hover_preview_view(item)
              │  text  → load_text_preview(item) → 24 lines
              │  image → state::thumbnail_path(item) → image::Handle
              │  file  → metadata header
              ▼
hover-exit ─▶ cancel debounce / Task::done(RemoveWindow(popup_id))
```

### Non-Functional Requirements

- **Performance:**
  - Tick remains 1 s; hover popup creation off the tick path
    (event-driven).
  - Thumbnail generation: on first display per image; subsequent
    paints reuse the cached PNG.
  - Memory ceiling: 256×256 RGBA = 256 KiB per cached image times
    `max(stack_size, 8 clip entries)` ≈ 4–6 MiB worst case. Acceptable.
  - Hover latency: debounce 250 ms + popup creation + first paint
    target < 350 ms p95.
- **Reliability:**
  - Thumbnail decode/resize errors degrade to the generic file-media
    glyph; never crash the bar.
  - Popup spawn fails (compositor) → log to stderr, swallow; no UI
    halt.
  - Concurrent hover-enter on two slots → cancel pending debounce,
    re-arm for the new slot.
- **Security:**
  - Thumbnails live under `$XDG_CACHE_HOME/sy/stack/thumbs/` with the
    user's umask. No new world-readable surfaces.
  - cliphist payloads are already on disk; we don't widen the
    exposure.
- **Observability:**
  - `tracing` spans wrap thumbnail generation and popup spawning.
  - `eprintln!("sy stack: thumbnail decode failed for {id}: {e}")`
    on the error path (matches existing bar-side logging style).

### CLI / MCP Surface

No new subcommands or flags. One new config field
(`[stack.bar].top_margin`), optional, defaults to 24.

The MCP surface (`src/stack/mcp.rs`) is unaffected — hover is a UI
concept, not an agent concept.

### Testing Strategy

- **Unit:**
  - `state::thumbnail_path` round-trips for png/jpeg/webp/gif/bmp:
    given a known image fixture, produces a cached 20×20 and
    256×256 variant; second call hits the cache.
  - `clip::parse_image_ext` extracts the extension from cliphist
    preview lines like
    `[[ binary data 100 KiB png ]]` → `Some("png")`.
  - `bar::glyph_for_item` returns the correct `(glyph, color_key)`
    pair for each `content_kind` × extension combination already
    covered by today's tests; new colour-key cases get assertions.
- **Integration:**
  - `state.rs` end-to-end: push an image file into the stack →
    thumbnail_path resolves → file exists at expected location →
    blob is removed by `delete_blobs` cleanly.
  - cliphist mirror: with a stub `cliphist` shim in `$PATH`, top()
    returns entries whose image_ext is populated for binary lines.
- **E2E / manual recipe:**
  - `sy stack bar` running under niri; push a PNG, JPEG, code file,
    plain text file, and config file; hover each; verify popup
    type matches; verify top edge sits at y=24 with waybar visible;
    verify image slots render an inline thumbnail.

### Migration & Compatibility

- `sy.toml` schema: new optional `[stack.bar]` block. Absent →
  current behaviour (constant default).
- On-disk: new `$XDG_CACHE_HOME/sy/stack/thumbs/` directory. Safe to
  delete; cache rebuilds on demand.
- No change to `items.json` schema.
- No change to the IPC ops (`refresh / toggle / reload-theme`).

### Dependencies

- `image` crate — already in the workspace via iced; we use its
  `imageops::thumbnail` for the resize. No new top-level dep.
- `regex` (or hand-rolled prefix match) — for cliphist extension
  extraction. Prefer hand-rolled to avoid a new dep; the prefix is
  fixed.

## 5. User Journey Sketch

**Actor:** rice user at the keyboard, glancing at their right-edge
strip while working.

**Trigger:** they want to recall what's in slot N of the clipboard /
app / user pool before clicking.

**Phases:**

1. User copies an image from a browser → cliphist stores it →
   sy stack bar's next tick refreshes `clips` → image slot now shows
   a tiny inline thumbnail instead of a generic media glyph.
2. User glances at the bar; thumbnails + colored glyphs let them
   identify content at a distance without hovering. → *visual*.
3. User is unsure about one of three text slots; hovers over slot 2 →
   250 ms later a popup appears beside the cursor with the first 24
   lines of the snippet in monospace. → *hover preview*.
4. User moves off; popup disappears. Moves over a PNG slot → popup
   shows the 256×256 thumbnail. → *image hover*.
5. User left-clicks to copy (default action) or right-clicks for the
   existing action menu — both flows unchanged.

**Visible-state changes after the fix:**
- Bar top edge moves from y=0 to y=24, so the screenshot's clipped
  layout becomes clean: waybar owns the entire top row, bar tucks
  under it.

### Friction Map

| Friction | Phase | Opportunity |
|----------|-------|-------------|
| Mono-fg glyphs make text/code/config indistinguishable at a glance | 2 | Colour-code by family + inline thumbnails for images |
| Click-to-preview opens a separate window that obscures the workspace | 3 | Hover popup is non-modal, no focus change, auto-dismisses |
| Image slots have no visual identity beyond a generic media glyph | 2 | Inline 20×20 thumbnail in the slot itself |
| Bar visually fights waybar for the top-right corner | (always-on) | Top margin pushes the bar below waybar's exclusive zone |
| Hover-tooltip text wraps to one char per line inside the 28-px bar | 3 | Use an XDG popup instead of the iced tooltip widget |

## 6. Risks & Mitigation

| Risk | Impact | Likelihood | Mitigation |
|------|--------|------------|------------|
| Popup spawn on every hover-enter flickers when the user sweeps the bar | UX | High without debounce | 250 ms debounce, cancellation on `on_exit` |
| Image decode/resize hangs the iced runtime | UI freeze | Low (small files, image crate is sync but fast) | Run on first display only; cache hits afterwards; fall back to glyph on error |
| `LayerShellSettings.margin` field order differs from `(top, right, bottom, left)` | Wrong-edge offset | Low (protocol spec defines it; iced_layershell follows) | Verify with a one-liner test; explicit comment in code citing the spec |
| cliphist preview format changes and the binary-data regex breaks | No thumbnails | Low (format stable since 0.7.0) | Fallback to generic media glyph; cliphist version pinned in `configs/` |
| Niri ignores `margin.top` (compositor bug) | Bar still overlaps waybar | Low | Spec-level guarantee; if it happens, fall back to setting `exclusive_zone = -1` and offsetting size manually |
| User has multi-monitor with different waybar heights | Wrong alignment on some monitors | Medium | Config override; second-pass: per-output detection |

## 7. Open Questions

- Where should the `[stack.bar]` block live? `configs/sy/sy.toml`
  template or a runtime-only addition? (Look at how
  `configs/sy/agents.toml` is rendered.)
- Should the hover popup steal keyboard focus to enable Escape-to-
  dismiss? Default: no (matches GNOME/KDE tooltip behaviour).
- Should we keep the click-to-preview right-click action now that
  hover preview exists? Probably yes — hover is for discovery, the
  preview window is for "open this for real".
- Multi-monitor: do we want per-output configuration of the top
  margin? Defer to a v2.

## 8. Hand-off

- Journey: run `/journey` against this spec →
  `specs/journeys/JOURNEY-<dt>.md`.
- Roadmap: run `/roadmap` against the journey →
  `specs/roadmaps/...` — likely 4 steps:
  1. Margin fix (smallest, isolated win).
  2. Type-aware glyph colour.
  3. Inline thumbnails for image slots.
  4. Hover preview popup (text + image + file paths).
- Implement: `/implement` step-by-step.
- No new aiplane Workload or NPU model needed.
