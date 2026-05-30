//! `sy file` — niri-tiled, iced-shaped file manager plane. Step 13
//! of the [`sy-file-manager` roadmap][roadmap] lands the carrier
//! (clap variant + module skeleton); the actual state model, fs
//! ladder, IPC surface, and iced UI fill in across Steps 14-29.
//!
//! Per-step expansion order:
//!
//! * Step 14 — `state` model (panes, selection, ops enum)
//! * Step 15 — `fs::walk` (`statx` fast-path async dir read)
//! * Step 16-17 — `fs::copy` (`copy_file_range` + `tokio-uring`)
//! * Step 18 — `fs::trash` (freedesktop spec)
//! * Step 19 — `fs::watch` + `fs::mime`
//! * Step 20 — `ipc` (JSON-RPC over `$XDG_RUNTIME_DIR/sy-file.sock`)
//! * Step 22 — systemd `sy-file.service`
//! * Step 23+ — iced UI (lives in `app.rs` / `view/` once it lands)
//!
//! [roadmap]: ../../specs/roadmaps/sy-file-manager/ROADMAP.md
//! [spec]: ../../specs/research/sy-file-manager/SPEC.md
// Step 31 (SPEC §3.3 item 15) — bookmarks (`b<key>` pin/jump) +
// freedesktop `recently-used.xbel` log. Not `gui-iced`-gated because
// the daemon's `file.open` IPC op (Step 20) touches the XBEL log on
// every open, regardless of whether the iced GUI is built in.
pub mod bookmarks;
pub mod cli;
// Step 33 (SPEC §3.3 item 19) — `sy file doctor` health probes. Not
// `gui-iced`-gated because the CLI-only build still needs the doctor
// surface (an operator running `sy file doctor --json` over SSH on a
// freshly-applied host has no Wayland).
pub mod doctor;
pub mod fs;
pub mod ipc;
// Step 34 (SPEC §3.3 item 17 + item 18) — user-overridable keymap
// loader. Not `gui-iced`-gated because the daemon's SIGHUP signal
// handler hot-reloads the file even on CLI-only builds (the future
// dispatch-time integration in the iced reducer takes the same wire
// shape).
pub mod keymap;
pub mod mcp;
// Step 27: bridge between the preview pipeline and the plugin runtime.
// Not `gui-iced`-gated because the bridge is pure data + IPC; the
// `view::preview` dispatcher (which IS gated) reads from it. CLI / MCP
// builds can also reach for it (`sy file --headless plugin-preview` is
// a future MCP affordance — kept compile-clean today).
pub mod plugin_bridge;
// Step 25 (SPEC §3.3 item 7) — `/` fuzzy filter + `:k` knowledge
// affordance. Not `gui-iced`-gated so `--ipc search` (CLI/MCP) can
// reuse the matcher without pulling iced in.
pub mod search;
pub mod state;

// Step 23: iced xdg-toplevel scaffold. Gated on `gui-iced` because
// both `app` and `theme` pull in `iced::*` and re-export from the
// `sy mon` theme (also gui-iced-gated). `--no-default-features`
// builds still get the full CLI + IPC + MCP surface; the bare
// `sy file [PATH]` form short-circuits to the scaffold marker on
// stdout (see `cli::run_scaffold`).
#[cfg(feature = "gui-iced")]
pub mod app;
// Step 29 — wayland `wl_data_device` DnD wire helpers. Gated on
// `gui-iced` because [`dnd::drop_action_from_modifiers`] references
// `iced::keyboard::Modifiers`. The pure-Rust uri-list helpers
// ([`dnd::paths_to_uri_list`] / [`dnd::parse_uri_list`]) could live
// outside the gate, but the only consumers are the `app::update`
// reducer arms and the integration-test harness, both of which already
// require `gui-iced`. Keeping the module behind one gate is simpler
// than splitting half the surface.
#[cfg(feature = "gui-iced")]
pub mod dnd;
#[cfg(feature = "gui-iced")]
pub mod theme;
// Step 24: responsive layout ladder. `view` hosts the `root` + `pane`
// composition; `widgets` hosts the Nerd-Font glyph map + the
// mode/selection chips the SPEC §3.3 row 3 + row 4 contracts pin.
// Both reach for `iced::*` so the `gui-iced` gate is mandatory.
#[cfg(feature = "gui-iced")]
pub mod view;
#[cfg(feature = "gui-iced")]
pub mod widgets;
