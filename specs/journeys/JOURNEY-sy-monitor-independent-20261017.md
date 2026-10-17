# Independent system monitor

A Fedora user installs the monitor feature and runs `sy mon` through the base launcher. The launcher invokes `sy-system-monitor`, which owns the collector, snapshot IPC, MCP bridge, doctor checks, Waybar tile, and popup. The desktop bundle can still dispatch `sy mon` during migration, and top-level `sy doctor` continues to include monitor checks.

## Acceptance

- `sy-system-monitor mon --help` lists the existing monitor commands.
- The standalone monitor accepts the launcher's global `--root` and `--target` options.
- `sy-system-monitor mon snapshot --json` returns the snapshot contract and exit code 3 when the collector is unavailable.
- `sy-system-monitor mon doctor --json` reports monitor checks without depending on the desktop binary.
- The collector emits supervisor metrics via `sy-core` and binds its own metrics socket.
- `cargo check -p sy-monitor --all-targets`, monitor tests, `make lint`, and `make test` pass.

## Manual check

In a user session with `XDG_RUNTIME_DIR` set, run `sy-system-monitor mon collect` in one terminal and `sy-system-monitor mon snapshot --json` in another. The second command must print a versioned snapshot. Run `sy-system-monitor mon doctor --json` and inspect the monitor checks.
