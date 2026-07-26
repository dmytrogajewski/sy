# JOURNEY-20260527-0215: first session in `sy file` — open, browse, preview, knowledge-rank, copy

Source spec: [`specs/research/sy-file-manager/SPEC.md`](../research/sy-file-manager/SPEC.md)
Plugin spec:  [`specs/research/sy-file-manager-plugins/SPEC.md`](../research/sy-file-manager-plugins/SPEC.md)

## Actor & Goal

- **Actor**: rice user on Fedora 43 + niri who already runs sy
  (waybar pills, `sy mon`, `sy stack bar`, `sy knowledge` daemon).
  They previously navigated files via yazi in a foot terminal and
  hit the chrome-headless preview failure ([the `md-rich.yazi`
  episode][md-rich]). Secondary actor: an MCP agent driving the
  same surface over `sy file mcp`.
- **Goal**: open a fresh `sy file` window, browse to
  `~/sources/sy`, preview `README.md` sharply with no terminal-
  image-protocol artefacts and no keyring popup, run a semantic
  query (`":k tuned override"`), multi-select three matched files,
  and copy them to `/tmp/notes/`. The window must collapse from
  3-pane to 2-pane to 1-pane as the niri column is shrunk, and an
  agent must be able to redo every step over JSON IPC.
- **Hardest constraint**: preview first-byte p99 ≤ 150 ms for the
  built-in markdown previewer; pane reflow on `WindowEvent::Resized`
  ≤ 1 frame (≤ 16 ms at 60 Hz) so the layout never visibly hitches.
  No subprocess that touches gnome-keyring / libsecret.

## Happy Path

1. **Bind & launch.** User presses `Mod+E`.
   `configs/niri/config.kdl`'s `binds {}` block spawns
   `sy file ~/sources/sy`. The clap variant (`Cmd::File` in
   `src/main.rs`, sister of `Cmd::Mon` at
   [src/main.rs:32](../../src/main.rs)) dispatches to
   `src/file/cli.rs::run`. If no daemon is running, `cli::run`
   forks an `sy-file.service` background process and IPCs the
   open request to it; otherwise it sends the IPC and exits.
   The actor sees the new xdg-toplevel claim a niri column with
   gruvbox-dark chrome and the JetBrainsMono Nerd Font icons.

2. **Three-pane render.** `src/file/app.rs` runs the iced loop
   (same pattern as `src/mon/app.rs:1`). `state::panes::Panes`
   holds parent / current / preview triplets. `fs::walk` reads
   `~/sources/sy` async with the `statx` fast-path, populates the
   current pane, and emits a `Message::PaneLoaded`. Mode is
   `LayoutMode::Three` because the niri column is ≥ 1100 px wide;
   `view::mod` lays out `row! [parent, current, preview]` with
   `Length::FillPortion(2)`, `FillPortion(3)`, `FillPortion(4)`.

3. **Hover preview.** Arrow keys move the cursor onto `README.md`.
   `Message::Hover` triggers `fs::mime` (extension + `tree_magic_mini`
   sniff) → `view::preview::dispatch` picks the built-in markdown
   previewer (`pulldown-cmark` → `cosmic-text` shaped text →
   `iced::widget::canvas` to a `wgpu` texture). The preview pane
   paints sharply at the column's native pixel size with no
   downscale, no sixel, no chrome process. Match `sy mon`'s
   render path (`src/mon/widgets/`) for memory + perf budgets.

4. **Knowledge query.** User presses `:`. The command bar
   (`view::commandbar`) opens. They type `k tuned override`.
   On `Enter` the host sends an IPC request to
   `sy-knowledge.service` reusing the same RPC backing
   `search_hits` at
   [src/knowledge/cli.rs:1013](../../src/knowledge/cli.rs).
   Returned `HitRow`s carry qdrant scores; `search::knowledge::merge`
   merges them into the current pane's `Entry` list as a
   `ranked_by: Knowledge` sort, surfacing
   `power-tuned-override-on-ac.md` (and two siblings) at the top.
   The statusbar `chip::Knowledge` flips to gruvbox-yellow with a
   match count.

5. **Multi-select.** User presses `<Space>` on each of the three
   top-ranked files. `state::selection::SelectionSet::toggle` adds
   them; statusbar shows `[3 selected]`. The selection chip is
   rendered by `view::statusbar`.

6. **Copy to /tmp/notes.** User presses `y`, then `:` `cd /tmp/notes`
   (creates the dir if missing via `fs::ops::Operation::Mkdir`),
   then `p`. `state::ops::spawn(Operation::Copy { srcs, dst })`
   launches an async task. The fast-path is
   `fs::copy::copy_same_fs(src, dst)` — for `/home → /tmp`
   (different mount), it falls through to a `tokio-uring`
   batched copy. `OpEvent::{Started, Progress, Completed}` events
   stream over IPC; the statusbar grows a `widgets::progress_row::ProgressRow`
   chip; waybar's `sy file --waybar` pill (driven by the same IPC
   shape `src/disk.rs::waybar_json` uses) reads `{ ops_pending: 1 }`
   until `Completed`.

7. **Tile-shrink reflow.** User invokes niri's
   `set-column-width '1/3'` (their bound key). niri sends a
   smaller `xdg_toplevel.configure`; iced fires
   `WindowEvent::Resized`. `app::update` recomputes
   `LayoutMode` and re-renders as `LayoutMode::Two`
   (current + preview); the parent pane disappears with a 120 ms
   ease. Statusbar `chip::Layout` shows `2-pane`. Shrinking
   further to `1/4` triggers `LayoutMode::One`; preview becomes a
   transient overlay invoked by `p`.

8. **Agent path mirror.** Concurrently, a Claude Code session
   running `sy agt …` calls `file_search { query: "tuned
   override", knowledge: true, root: "/home/dmitriy/sources/sy" }`
   over MCP. The handler in `src/file/mcp.rs` calls the same
   `search::knowledge::query` as Step 4 and returns
   `{ results: [path, path, path] }`. The agent then calls
   `file_copy { sources, dest, conflict: "rename" } → { op_id }`
   and subscribes to `file_op_stream { op_id }` for progress —
   the GUI's progress chip and the agent's stream are powered by
   the same `OpEvent` enum, so they never disagree.

## Edge Cases

- **`sy-knowledge` daemon unreachable.** `:k` query times out at
  `src/file/search/knowledge.rs::query` (250 ms ceiling). The
  `chip::Knowledge` flips dim-grey with tooltip
  `"sy-knowledge: unreachable (retrying in 30 s)"`. Search falls
  back to `search::filename::nucleo_match`. Exit / log surface:
  `tracing::warn!(target="sy_file::knowledge", "unreachable")` on
  stderr; `sy file --log-format json` emits
  `{"kind":"knowledge.unreachable","retry_in_s":30}`. No exit; the
  GUI keeps running.

- **Plugin previewer crashes mid-request.** The user hovers a
  malformed PDF; the third-party `sy-plugin-pdf` (see plugin
  SPEC §5) segfaults during `preview`. The host's `plugin::proc::Supervisor`
  detects EOF on stdout, returns `Result<_, PluginError::Crashed>`
  to the previewer dispatcher; preview pane shows
  `"plugin 'pdf-pretty' exited (code 11) — sy plugin doctor"`.
  Restart ladder fires (backoff 100 ms → 200 ms → 400 ms); after
  3 attempts the plugin is marked `Unhealthy` and `chip::Plugins`
  flips orange. waybar pill class becomes `degraded`.

- **niri column too narrow at launch.** User has niri's default
  column-width set to `1/4` and runs `sy file`. First paint:
  `LayoutMode::One`. The actor sees only the current pane and
  the statusbar; `p` opens preview as a transient. No layout
  hitch, no missing-pane error.

- **Bulk copy crosses fs, disk fills.** Step 6 hits `ENOSPC`.
  `fs::copy::stream` returns `Err(io::Error)` with
  `ErrorKind::StorageFull`; the op transitions to `OpEvent::Failed
  { code: "ENOSPC", msg, partial_dst: [paths] }`. The progress
  chip shows red; on click, a banner offers `Retry`, `Rollback`
  (trash partial outputs), `Open destination`. Agent IPC stream
  emits the same `Failed` event. Exit code from `sy file --ipc
  copy …` is `4` (op-cancelled / refused, per CLI exit-code
  table).

- **Two `sy file` instances run the same IPC op.** The second
  invocation finds the daemon socket already in use and
  IPC-forwards instead of spawning. `Cmd::File` falls through
  `cli::run` to `cli::ipc_send`. The two windows share state
  through the daemon; selections / ops are global. (Same pattern
  as `sy stack bar` reusing its layer-shell surface.)

- **^C during copy from a `sy file --ipc copy …` invocation.**
  CLI sends `SIGINT` → `Cmd::File` handler issues
  `file_op_cancel { op_id }` → op task observes
  cancellation token, deletes any partial output in `dst`, emits
  `OpEvent::Cancelled`. CLI prints
  `cancelled (rollback ok)` and exits `4`. The GUI's progress
  chip clears.

- **SELinux denial on plugin spawn.** Plugin SPEC §4.3 labels
  plugins as `sy_plugin_t`; the policy module isn't installed on
  this host. `runcon` returns ENOENT or `setexeccon` returns
  EPERM; `proc::Supervisor` falls back to spawning without a
  context **and** logs
  `tracing::warn!(target="sy_file::plugin", "selinux module sy_plugin missing")`.
  `sy file doctor` and `sy plugin doctor` both surface the
  remediation: `make install-system-sy-plugin-selinux`.

- **Preview-pane content > 16 MB.** A user hovers an enormous
  Markdown. The rendered cosmic-text canvas would consume excess
  GPU memory. `view::preview::dispatch` caps the rendered area
  at `rt.preview.max_height` (default 900 px in the iced canvas
  coords); excess content scrolls per `Message::PreviewSeek`
  (mirrors yazi's `seek` semantics) rather than expanding the
  canvas. No OOM; scrolling is the user's recourse.

- **JetBrainsMono Nerd Font missing.** `view::widgets::icon` calls
  `fontconfig` to resolve the glyph; absence falls back to the
  Unicode replacement char. `sy file doctor` flags it with the
  install hint already present in
  [`scripts/yazi-plugins.sh`-style font check][readme-fonts].

- **niri keybind collides with existing `Mod+E`.** `sy file
  doctor` parses `configs/niri/config.kdl`, detects the
  duplicate, prints
  `"warn: Mod+E bound twice (sy file, <other>) — last-wins"`
  and exits 0. CI's `niri validate` step (added to
  `make docs-lint`-equivalent for niri) catches this at PR time.

## Acceptance Criteria

- [ ] Step 1 happy path: integration test in
  `tests/sy_file_open.rs` spawns the daemon on a tmpfs root,
  sends `cli::ipc::open(path)`, asserts the daemon-in-thread
  shows a window of mode `Three` (no real wgpu surface — test
  asserts state, not pixels). Mirrors the
  `src/mon/app.rs`-targeted tests under `tests/mon_*`.
- [ ] Step 2 layout: unit test `view::layout::mode_for_width(px)`
  returns `Three` for 1280, `Two` for 800, `One` for 400.
- [ ] Step 3 preview: integration test renders the in-tree
  `tests/fixtures/preview-sample.md` and asserts the resulting
  `Texture` checksum matches a golden; tolerated drift < 0.5 %.
- [ ] Step 4 knowledge: daemon-in-thread test stubs `sy-knowledge`
  with three known hits; assert pane re-orders the entries and
  `chip::Knowledge` reports `count=3`.
- [ ] Step 5 multi-select: unit test on
  `SelectionSet::{toggle, range, invert, clear}`.
- [ ] Step 6 copy: integration test on tmpfs;
  `copy_file_range` path for same-fs, `tokio-uring` path forced
  via env override; asserts `OpEvent` sequence
  `Started → Progress(>0) → Completed` and final byte-identity of
  src/dst.
- [ ] Step 7 reflow: integration test sends synthetic
  `WindowEvent::Resized` with `(1280, 720) → (640, 720) → (320, 720)`
  and asserts `LayoutMode` transitions `Three → Two → One`
  with no panic and no `Message::Error`.
- [ ] Step 8 agent path: integration test on the MCP surface
  drives `file_search` + `file_copy` + `file_op_stream` and
  asserts the GUI state and the MCP responses agree
  (single source of `OpEvent`).
- [ ] All edge cases above: explicit test or doctor-probe entry.
  - knowledge-unreachable: timeout test.
  - plugin-crash: drives the fake-previewer at
    `tests/fixtures/sy-plugin-fake` and crashes it; asserts
    restart ladder + Unhealthy state.
  - narrow-launch: layout test at startup width 360.
  - ENOSPC: tmpfs with `size=4M`, copy a 5 M file.
  - concurrent CLI: spawn two `sy file --ipc` invocations,
    second IPC-forwards.
  - ^C cancel: spawn copy, send SIGINT, assert rollback.
  - SELinux missing: stub `runcon` returning ENOENT.
  - large preview: 64 MB markdown fixture.
  - missing font: env-clear `XDG_DATA_HOME`, run doctor.
  - niri keybind collision: synthetic `config.kdl` with two
    `Mod+E` binds, run doctor.
- [ ] `make lint` (clippy + LOC ceiling) and `make test` green.
- [ ] README's "Stack" table row for File manager points at
  `sy file`; the yazi row is replaced; `scripts/yazi-plugins.sh`
  removed; `configs/yazi/` removed.
- [ ] `docs/how-to/run-sy-file.md` written (open, browse,
  multi-select, copy, plugin install).
- [ ] `docs/how-to/write-a-sy-plugin.md` written (Rust PDK +
  manifest example, install + doctor flow).
- [ ] `sy file doctor` and `sy plugin doctor` both pass on the
  reference host after `sy apply`.
- [ ] waybar tile via `sy file --waybar` shows running-ops count
  in real time during Step 6's copy.

## Out of Scope

- **Tabs inside the window.** Anti-goal in the SPEC (§3.4) —
  niri's tiling is the tab UI.
- **Remote-fs (SSH/SMB/SFTP) browsing.** Anti-goal in the SPEC —
  single-host charter; credential storage is a snowflake hazard.
- **Embedded archive extraction beyond preview.** Anti-goal —
  we list contents via `lsar`; extraction goes through system
  tools.
- **Per-window theme variants.** Anti-goal — theme is sy-wide.
- **Bulk-rename via in-window editor.** Open question (SPEC §7);
  this journey uses `$EDITOR` only, consistent with yazi muscle
  memory.
- **`xdg-desktop-portal` Global Shortcuts integration.** niri
  does not implement it (May 2026); SPEC §3.2 explicitly chose
  niri keybinds + IPC instead. Revisit when niri gains the
  portal.
- **Third-party plugin signature key management UX.** The plugin
  SPEC §4.5 + §7 leave it as a sketch (minisign keys under
  `configs/sy/plugin-publishers/`); the first first-party
  plugin (`sy-plugin-md`) ships unsigned in this journey and
  the signature flow is exercised in a separate plugin-author
  journey.

## Open Questions

- **Bookmark format.** SPEC §7 — write both XBEL (interop) and
  TOML (sy-tagged); journey assumes the dual write; confirm
  before roadmap step lands.
- **`:k` query language.** SPEC §7 — free-text only in this
  journey; qdrant filter syntax is deferred.
- **Drag-out target compatibility on niri.** Wayland DnD via
  `wl_data_device` works in Telegram (Qt) and Firefox (GTK);
  this journey does not test cross-toolkit DnD because it isn't
  a happy-path step — flagged for the roadmap as a separate
  spike.
- **Per-file knowledge re-rank vs. dir-scoped.** Step 4 scopes
  the query to `cwd`; should `:k` be tree-recursive by default?
  Current proposal: recursive within `cwd` ≤ 5 levels deep,
  capped at qdrant's `limit = 50`; revisit after first user
  trial.
- **Cold-start window for the daemon.** Step 1 spawns the
  daemon if absent; perceived latency target ≤ 250 ms. If first
  pane render exceeds the budget, do we pre-warm `sy-file.service`
  via `sy.target` (matching `sy-knowledge`)? Decide at roadmap
  time once a real measurement exists.

[md-rich]: ../research/sy-file-manager/SPEC.md#section-2-2
[readme-fonts]: ../../README.md
