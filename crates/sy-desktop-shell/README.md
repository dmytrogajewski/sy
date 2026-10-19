# sy-desktop-shell

Optional desktop applets and the layer-shell stack bar. Build the standalone
executable with `cargo build -p sy-desktop-shell --bin sy-desktop-shell`.
It accepts the existing `sy` desktop subcommands, including `stack bar`,
`stack push`, `vol`, `bright`, `bat`, `wifi`, `net`, `notif`, `profile`,
`wallpaper`, and `wwan`. Run `sy-desktop-shell --help` for the full list.

The library exports the applet modules for the legacy `sy` command during
migration. It uses `sy-core` sensors and `sy-ipc` for stack state and metrics.
The shell does not link the AI or knowledge runtimes. The stack's agent action
uses the separate `sy-agent` registry and invokes `sy agt run` through the
public command surface.

Drag an entry from the right-side stack panel into another Wayland application.
Clipboard screenshots and saved files offer file URIs, so dropping a screenshot
into a terminal inserts its path. Text entries offer their original text.
Hold the left button and move at least 6 logical pixels to start a drag; a click
still copies, and right-click opens the action menu. Escape cancels a drag.

Screenshot exports preserve the original bytes under
`$XDG_RUNTIME_DIR/sy/clip/` with private file permissions. They remain available
for the runtime session, including after a drop or clipboard-history refresh.
The receiving application handles insertion, quoting, or attaching the file.
Dragging uses copy semantics and retains the source item.

Verification: `cargo test -p sy-desktop-shell --all-targets` and
`cargo clippy -p sy-desktop-shell --all-targets -- -D warnings`.
