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

Verification: `cargo test -p sy-desktop-shell --all-targets` and
`cargo clippy -p sy-desktop-shell --all-targets -- -D warnings`.
