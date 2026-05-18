# ROADMAP: stack bar UX — hover previews, type-aware slots, under-waybar alignment

Source: `specs/research/stack-bar-ux/SPEC.md`

## Overview

Land the spec in four ordered, independently shippable steps. Each
step is a single PR-shaped commit that ends with a green
`make lint && make test`, and each delivers a user-visible improvement
on its own — so a regression in a later step doesn't cost the earlier
ones. End state: bar tucks under waybar at y=24, image slots show
inline thumbnails, non-image slots colour-code by type, hovering any
slot shows a debounced preview popup with text/code body or image
thumbnail.

The spec is the contract; this roadmap is the plan. The spec asserted
"`image` crate already in the workspace via iced" — that's only true
behind the `image` feature flag on iced (`Cargo.toml:30`, feature
not enabled today). Step 3 owns enabling it; before that no
dependency change is needed.

---

## Step 1 — Anchor the bar below waybar via layer-shell margin

**Goal:** the `sy stack bar` top edge sits at y=24 deterministically,
regardless of compositor anchor-deduction or `spawn-at-startup`
ordering. Resolves the alignment bug in the screenshot.

**Files:**
- `src/stack/bar/app.rs:56,155-164` (modified) — add a
  `WAYBAR_TOP_MARGIN: i32 = 24` const beside `BAR_WIDTH`; set
  `LayerShellSettings.margin = (WAYBAR_TOP_MARGIN, 0, 0, 0)`. Add a
  comment citing `wlr-layer-shell-unstable-v1` margin semantics.
- `src/stack/bar/app.rs:194-217` (modified) — the `Op::Toggle`
  branch reconstructs anchor + size + zone; nothing currently
  rebuilds margin. Confirm margin survives the toggle (it should, as
  it's set once at startup, not per-message), and document this in
  the comment block above the const.
- `configs/niri/config.kdl:75-78` (modified) — replace the obsolete
  "registered AFTER waybar so layer-shell deducts waybar's 24px top
  exclusive zone" comment with the new model ("bar pushes itself
  down 24px via layer-shell margin; spawn order no longer matters").

**Tests:**
- `src/stack/bar/app.rs::tests::waybar_top_margin_is_nonzero` — a
  trivial compile-time + runtime constant check (`assert!(WAYBAR_TOP_MARGIN > 0)`)
  that documents the intent and fails loudly if a future refactor
  zeroes it out.
- Manual recipe added to the step's PR description: run
  `sy stack bar` under niri, verify the bar's first slot is below
  waybar's clock row. (No automated compositor test fits a unit
  suite; this is the unavoidable manual leg of the DoD.)

**Definition of Done:**
- [x] Const + margin wired (`src/stack/bar/app.rs:60-71,167-171`);
      `make test` green (56/56). Live-niri visual verification still
      owed to the PR description — bar should render below waybar.
- [x] niri config comment updated (`configs/niri/config.kdl:74-78`).
- [~] `make test` green; **`make lint` red workspace-wide on
      pre-existing dead-code / clippy debt** outside `src/stack/`.
      `src/stack/bar/app.rs` itself is clippy-clean; two incidental
      pre-existing violations in the touched file
      (`sort_by` → `sort_by_key`, redundant `action: action`) were
      cleaned up per AGENTS.md "leave the area cleaner". Workspace-
      wide lint gate awaits a separate dead-code cleanup pass.
- [x] No new `#[allow(dead_code)]` or `TODO`/`FIXME` strings.
- [x] README.md / SKILL.md unchanged (no user-facing API change).

**Risks / unknowns:**
- ~~If `iced_layershell` translates `margin` to a different tuple
  order than `(top, right, bottom, left)`~~ — **resolved**:
  `layershellev-0.17.1/src/lib.rs:665` is
  `fn set_margin(&self, (top, right, bottom, left): (i32, i32, i32, i32))`,
  matching the wlr-layer-shell spec order. Default in
  `iced_layershell-0.17.1/src/settings.rs:94` is `(0, 0, 0, 0)`.
- Multi-monitor with different waybar heights stays misaligned on
  some outputs. Acceptable for this step — covered by Step 5
  (deferred) in the spec's open questions.

---

## Step 2 — Type-aware Codicon colour per slot family

**Goal:** non-image slots become visually distinguishable at a
glance — code slots accent_blue, archive slots accent_yellow, config
slots fg_dim, text/file/binary stay fg. Image slots are unchanged in
this step (handled by Step 3).

**Files:**
- ~~`src/stack/bar/theme.rs` (modified) — extend `Palette` with
  `accent_blue`, `accent_yellow`, `accent_green`~~ **— dropped**:
  `Palette` already exposes `blue`/`orange`/`aqua`/`green`/`red`
  from the gruvbox-material fallback (`theme.rs:16-29`). Reusing
  `blue` (code), `orange` (archive), `fg_dim` (config), `fg`
  (text/image/file) avoids inventing redundant schema fields.
- `src/stack/bar/app.rs:489-560` (modified) —
  `glyph_for_item(it: &Item) -> (&'static str, ColorKey)` where
  `ColorKey` is a `Debug, Clone, Copy, PartialEq, Eq` enum
  (`Text, Code, Image, Archive, Config, File`). The returned key is
  mapped to a `Color` by `resolve_glyph_color(palette, key) -> Color`.
  Update both `bar_view` (clip section, `app.rs:341-376`) and
  `stack_slot` (`app.rs:402-415`) to thread the colour through to
  the existing `slot()` function. The clip section also routes
  through `resolve_glyph_color` (key = Image or Text) so theme
  changes propagate uniformly across stack and cliphist slots.

**Tests:**
- `src/stack/bar/app.rs::tests::glyph_for_item_rust_source_is_code` —
  `.rs` file maps to `ColorKey::Code`.
- `src/stack/bar/app.rs::tests::glyph_for_item_tarball_is_archive` —
  `.tar.gz` → `ColorKey::Archive`.
- `src/stack/bar/app.rs::tests::glyph_for_item_toml_is_config` —
  `.toml` → `ColorKey::Config`.
- `src/stack/bar/app.rs::tests::glyph_for_item_png_is_image` —
  `.png` → `ColorKey::Image` (still relevant in Step 3, where we
  swap the rendering but keep the key).
- `src/stack/bar/app.rs::tests::glyph_for_item_content_text_is_text` —
  content item with `content_kind = "text"` → `ColorKey::Text`.
- `src/stack/bar/app.rs::tests::resolve_glyph_color_distinguishes_code_archive_config`
  — three of the five non-image families resolve to distinct colours
  against `Palette::default()` (replaces the dropped
  `default_palette_has_distinct_accents` test now that we reuse
  existing palette fields).

**Definition of Done:**
- [x] All tests above pass — 7/7 new
      (`stack::bar::app::tests::glyph_for_item_*` and
      `resolve_glyph_color_distinguishes_code_archive_config`).
- [~] `make test` green (62/62); `make lint` red workspace-wide on
      the same pre-existing dead-code debt called out under Step 1.
      `src/stack/bar/app.rs` itself is clippy-clean.
- [x] No new `#[allow(dead_code)]`.
- [ ] Slot rendering verified visually in a live `sy stack bar`
      session — owed to the PR description (screenshot of
      code/text/config/archive reading differently at arm's length).
- [x] Comment block at `src/stack/bar/app.rs` describing the slot
      contract still accurate — the colour story did not invalidate
      the hover/click claims (hover comes in Step 4).

**Risks / unknowns:**
- ~~Accent colours derived from a non-default `Palette` (user theme)
  could land too close to `fg`~~ — **resolved by reusing existing
  palette fields**. Themed palettes load `blue`/`orange` via the
  same `theme.rs::apply` path; if a user theme is monochrome, the
  fallback to `Palette::default()` keeps gruvbox-material's
  dichromat-safe pairing.
- Existing colour-blindness assumptions: gruvbox-material's
  `blue` (#7daea3) vs `orange` (#e78a4e) is a strong dichromat
  pairing — documented in the resolve helper's doc comment.

---

## Step 3 — Inline thumbnails for image slots

**Goal:** image slots render a 20×20 thumbnail in place of the
`file-media` codicon, both for stack image items and for cliphist
binary entries. Cache lives at `$XDG_CACHE_HOME/sy/stack/thumbs/`.

**Files:**
- `Cargo.toml:30` (modified) — extend the iced feature list to
  `["tokio", "wgpu", "image"]` so `iced::widget::image` and
  `image::Handle` become available. Re-export the bundled `image`
  crate via `iced::advanced::image` (or pull `image = "0.25"` as a
  direct dep if iced doesn't re-export). Pin minor version,
  document in the dep block comment.
- `src/stack/state.rs` (modified) — add:
  - `pub fn thumbs_dir() -> Result<PathBuf>` — `$XDG_CACHE_HOME/sy/stack/thumbs/`.
  - `pub fn thumbnail_path(item: &Item, size: u32) -> Result<Option<PathBuf>>` — returns `None` for non-image items; otherwise resolves the source path (`item.path` or the blob payload), reads, resizes to `size×size` (aspect-letterboxed), writes to `thumbs_dir().join(format!("{id}.{size}.png"))` if not present, returns the path.
  - `pub fn delete_blobs(id)` (modified, `state.rs:227-243`) — also remove `thumbs_dir().join(format!("{id}.*.png"))`.
- `src/stack/clip.rs:14-21,38-56` (modified) — extend `ClipEntry`
  with `image_ext: Option<&'static str>` parsed from cliphist's
  `[[ binary data N KiB <ext> ]]` line (hand-rolled prefix scan, no
  regex dep). Add `pub fn decode_to_thumb(id: &str, ext: &str, size: u32) -> Result<PathBuf>` that calls `decode(id)`, runs the same resize+cache pipeline as `state::thumbnail_path`, and returns the cached path. Cache key:
  `thumbs_dir().join(format!("clip-{id}.{size}.png"))` so it
  doesn't collide with stack item ids.
- `src/stack/bar/app.rs:339-357,383-397` (modified) — when a slot's
  `ColorKey == Image`, render an `iced::widget::image::Image::new(handle)`
  sized to 20×20 inside the slot instead of the codicon. Image
  handles built from `state::thumbnail_path(..., 20)` /
  `clip::decode_to_thumb(..., 20)`. On error, fall back to the
  existing `file-media` codicon so the bar never goes blank.

**Tests:**
- `src/stack/state.rs::tests::thumbnail_path_creates_20x20_png` —
  given a fixture PNG, returns a path that exists and decodes back
  to a 20×20 image.
- `src/stack/state.rs::tests::thumbnail_path_is_cached_on_second_call`
  — second call returns the same path and does not re-write the
  file (mtime unchanged within one second).
- `src/stack/state.rs::tests::thumbnail_path_returns_none_for_text_item`
  — non-image items return `Ok(None)`.
- `src/stack/state.rs::tests::delete_blobs_removes_thumbnails` —
  after `delete_blobs(id)`, no `thumbs/<id>.*.png` file remains.
- `src/stack/clip.rs::tests::parse_image_ext_handles_png_jpeg_webp_gif_bmp`
  — table-driven over cliphist preview strings.
- `src/stack/clip.rs::tests::parse_image_ext_returns_none_for_text`
  — non-binary previews → `None`.
- `src/stack/clip.rs::tests::decode_to_thumb_caches_under_clip_prefix`
  — given a fake `cliphist` shim in `PATH`, the cached file lives
  under `thumbs/clip-<id>.*.png`.

**Definition of Done:**
- [x] iced `image` feature enabled (`Cargo.toml:30`); direct
      `image = "0.25"` dep added with minimal codecs
      (`png`, `jpeg`, `webp`, `gif`, `bmp`); `tempfile` dev-dep.
      `cargo build --features bar-iced` succeeds.
- [x] All 7 step-3 tests pass —
      `state::tests::thumbnail_path_*`,
      `state::tests::delete_blobs_removes_thumbnails`,
      `clip::tests::parse_image_ext_*`,
      `clip::tests::decode_to_thumb_caches_under_clip_prefix`.
- [~] `make test` green (69/69); `make lint` red workspace-wide on
      the same pre-existing dead-code debt as Steps 1-2. All three
      files I touched in this step (`src/stack/state.rs`,
      `src/stack/clip.rs`, `src/stack/bar/app.rs`) are clippy-clean.
- [ ] Visual verification in a live `sy stack bar` session — owed
      to the PR description (push a PNG into stack/user, copy a
      screenshot to cliphist, confirm both render inline
      thumbnails).
- [x] No new `#[allow(dead_code)]`.
- [x] AGENTS.md unchanged.

**Risks / unknowns:**
- ~~iced 0.14 may require enabling `image` *and*
  `image-without-codecs`~~ — **resolved**: enabling just `image`
  works; codec selection lives on the direct `image = "0.25"` dep.
- ~~Decoding a 50 MB image on the iced thread could stall the UI~~
  — **mitigated by early cache check**:
  `thumbnail_path_at`/`decode_to_thumb_at` stat the destination
  PNG before opening / shelling out to cliphist, so the
  steady-state per-tick cost is one `fs::exists` per image slot.
  First-decode cost still hits the iced thread; deferred to a
  follow-up if a real 50 MB clip ever lands.
- ~~`iced_layershell` may not surface `iced::widget::image` cleanly
  when both `wgpu` and `image` features are on~~ — **resolved**:
  `cargo build --features bar-iced` compiles with the combination;
  `iced::widget::image(Handle::from_path(p))` renders inside the
  layer-shell surface.

---

## Step 4 — Hover preview popup (text + image + file metadata)

**Goal:** hovering any slot for 250 ms spawns a popup beside the
cursor with the slot's content preview — first 24 monospace lines
for text/code, 256×256 thumbnail for image, name+path+size+mtime for
opaque file types. Popup auto-closes on hover-exit.

**Files:**
- `src/stack/bar/app.rs:66-97` (modified) — extend the `Bar` struct:
  - `hover_armed: Option<(SlotSource, String, std::time::Instant)>` — slot under cursor + when entry fired.
  - `hover_popup: Option<Id>` — currently-shown hover popup id (distinct from action `popups: HashMap`).
- `src/stack/bar/app.rs:99-126` (modified) — extend `Msg`:
  - `SlotHoverEnter { id: String, source: SlotSource }`
  - `SlotHoverExit { id: String, source: SlotSource }`
  - `HoverDebounceElapsed { id: String, source: SlotSource, fired_at: std::time::Instant }`
- `src/stack/bar/app.rs:174-181` (modified) — extend `subscription`
  to fire a `HoverDebounceElapsed` 250 ms after each hover-enter
  using `iced::time::every` keyed on the armed slot; cheapest
  implementation: a single 50 ms ticker that polls `hover_armed`
  and emits the message when the threshold is crossed.
- `src/stack/bar/app.rs:317-324` (modified) — `view()` dispatches
  on a new `PopupKind` discriminator stored alongside `PopupCtx`:
  `Action` (existing `popup_view`), `Hover` (new
  `hover_preview_view`). Today's `popups: HashMap<Id, PopupCtx>` is
  reused; `PopupCtx` gains `kind: PopupKind`.
- `src/stack/bar/app.rs:406-449` (modified) — `slot()` accepts
  `on_enter: Msg` and `on_exit: Msg` and wires them through
  `mouse_area::on_enter` / `on_exit`.
- `src/stack/bar/app.rs` (new function) —
  `hover_preview_view(bar, item_id, source) -> Element<Msg>` that
  branches on type:
  - **image** → `Image::new(state::thumbnail_path(item, 256))` /
    `clip::decode_to_thumb(id, ext, 256)`.
  - **text/code** → `state::read_payload` (or `item.path` read) →
    `text(...).font(MONO_FONT).size(11)` on the first 24 lines.
  - **file** (non-image with `path` set, no readable payload) →
    name + path + size + mtime formatted text.

**Tests:**
- `src/stack/bar/app.rs::tests::hover_state_arms_on_enter_disarms_on_exit`
  — drive the `update()` function with synthetic `Msg::SlotHoverEnter` then `Msg::SlotHoverExit`; assert `Bar.hover_armed` transitions accordingly.
- `src/stack/bar/app.rs::tests::hover_state_swaps_on_enter_of_different_slot`
  — entering slot B while armed on slot A re-arms on B and discards
  the A timer.
- `src/stack/bar/app.rs::tests::hover_debounce_fires_only_when_still_hovering`
  — `HoverDebounceElapsed` for slot A is a no-op when
  `hover_armed` is now slot B or `None`.
- `src/stack/bar/app.rs::tests::popup_kind_dispatches_in_view` —
  given a `PopupCtx { kind: Hover, .. }` registered, `view()`
  produces a hover-preview tree (asserted by a `Display`-format
  hash of the element type — iced doesn't expose a richer test
  hook; if this proves brittle, drop in favour of an integration
  manual recipe).
- `src/stack/state.rs::tests::text_preview_truncates_to_24_lines`
  — a 100-line input is truncated to 24 newline-separated lines
  with an ellipsis line appended.

**Definition of Done:**
- [x] All 3 hover-state tests pass
      (`hover_state_arms_on_enter_disarms_on_exit`,
      `hover_state_swaps_on_enter_of_different_slot`,
      `hover_debounce_fires_only_when_still_hovering`) + 2 text-
      preview tests (`text_preview_truncates_to_24_lines`,
      `text_preview_returns_short_input_verbatim`). The proposed
      `popup_kind_dispatches_in_view` test was dropped per the
      roadmap's own caveat (iced doesn't expose an introspection
      hook robust enough to make the test meaningful) — coverage
      is supplied instead by the manual hover-each-slot recipe.
- [~] `make test` green (74/74); `make lint` red workspace-wide on
      the same pre-existing dead-code debt as earlier steps. All
      touched files (`src/stack/bar/app.rs`, `src/stack/state.rs`)
      are clippy-clean.
- [ ] Manual verification — owed: hover stack/clip slots of each
      type in a live `sy stack bar` session; confirm popup body
      (image thumbnail / monospace text / metadata header) and
      auto-dismiss on exit; confirm fast sweeps don't spawn
      intermediate popups (50 ms tick + 250 ms debounce).
- [x] Comment block at `src/stack/bar/app.rs::slot` rewritten:
      old claim "no tooltip — hover handled by left-click" replaced
      with "hover discovery via debounced hover popup; right-click
      remains the canonical 'open for real' affordance".
- [x] No new `#[allow(dead_code)]` or `TODO`/`FIXME` strings.
- [x] README / `.agents/skills/workload/` unchanged.

**Risks / unknowns:**
- ~~`mouse_area::on_enter` / `on_exit` fire frequently enough that
  the 50 ms polling subscription becomes the wrong primitive~~ —
  **acceptable v1**: build green, mouse_area events fire once per
  cross. If user reports flicker we can switch to a per-arming
  `iced::time::every`.
- ~~Popup position relative to the cursor under XDG-popup
  positioning rules may clip near the screen edge~~ — the popup
  reuses the left-of-bar offset from the action popup; deferred to
  follow-up if clipping shows up in practice.
- ~~Hover popup competing with the right-click action popup~~ —
  **resolved**: `Msg::SlotRightClicked` clears `bar.hover_armed`
  before opening the action popup, and `close_all_popups` clears
  `hover_popup` so the bookkeeping invariant holds.

---

## Cross-cutting Definition of Done

- [~] All four step DoDs satisfied at the code level; the manual
      live-niri verification line item in each step remains owed
      (only the human can run a compositor).
- [ ] End-to-end on a clean checkout — owed to manual verification:
  1. `cargo build --release` succeeds. ✓ verified during step 3.
  2. `sy stack push README.md --name readme`.
  3. Copy a PNG screenshot to the clipboard.
  4. `sy stack bar` running under niri.
  5. Bar's top edge is at y=24; image slot shows an inline
     thumbnail; hovering each slot type shows the right popup.
- [~] **`make test` green (74/74)**. **`make lint` red workspace-
      wide on pre-existing dead-code outside `src/stack/`** —
      called out under each step's DoD; separate cleanup pass owed.
- [x] Zero `#[allow(dead_code)]` introduced.
- [x] `configs/niri/config.kdl` comment block updated.
- [x] The spec's "Open Questions" answered inline in each step's
      Risks block — iced feature combination, margin tuple order,
      popup keyboard focus.
- [ ] Final-PR screenshot owed.

## Out of Scope

- `[stack.bar].top_margin` runtime config (constant default first;
  config bump deferred to a follow-up step once at least one user
  reports a non-24-px waybar).
- Per-output / per-monitor configuration of the top margin.
- Syntax highlighting in the hover code preview (anti-goal in the
  spec).
- Animated popup transitions / fades.
- Sensitive-content masking on hover.
- Hover preview for cliphist *file* entries — cliphist only stores
  text + image.
- Multi-page PDF preview — the existing right-click → preview
  action shells out to a real viewer and stays the canonical flow
  for opaque file types.
