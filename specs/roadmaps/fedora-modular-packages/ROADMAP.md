# Roadmap: modular sy desktop and Fedora packages

- [x] Give the power menu its own Rust module and wire it into niri and Waybar.
- [x] Split packaging into `sy` binary and `sy-desktop` config RPMs; build Fedora 43/44 artifacts in CI and locally.
- [x] Discover AMD venv Python ABI and add an upgrade preflight.
- [x] Extract the power menu into a standalone Rust crate and `sy-system-power` RPM; delegate `sy power` to it.
- [x] Package a small `sy` launcher so the base RPM has no GUI or inference binary; put the existing bundled executable in `sy-desktop` as a migration step.
- [x] Add Fedora 45 to the RPM matrix and use `/usr/bin/sy` in packaged niri, Waybar, and user-unit commands.
- [x] Extract the agent runner into `sy-agent` with a standalone CLI, policy files, and user unit in `sy-system-agent`.
- [x] Extract AI workload code into `sy-aiplane`; package the CLI independently and split Ryzen AI boot integration into its own RPM.
- [x] Extract file manager and plugin host code into `sy-filemanager`; move shared color definitions into `sy-theme`.
- [x] Extract desktop, knowledge, monitoring, and auth executables from the remaining bundle into their own RPMs.
  - [x] Authentication CLI and SELinux sources are in `sy-system-auth`.
    Knowledge, monitoring, and desktop shell have separate crates and RPMs.
  - [x] Monitor runtime and standalone `sy-system-monitor` executable live in
    `sy-monitor`; its RPM owns the monitor user unit and Waybar module.
- [x] Move aiplane supervision into its own user daemon and socket; route packaged knowledge clients to it while retaining the legacy checkout migration path. The CPU-capable AI RPM and separate Ryzen AI integration RPM have an upgrade check.
- [x] Render only installed feature configs and user units, including removal and upgrade cleanup.
  - [x] RPM file ownership selects installed feature sources; `sy apply`
    removes stale links into sy's unit source directory after removal.
  - [x] Packaged Waybar and niri render optional power, agent, auth, and file
    manager entries only when their executables are installed.
  - [x] Reconcile copied config files and previously rendered unit files on
    package removal without deleting user edits. A first apply establishes
    the content manifest for future removals.
- [x] Move the remaining desktop applets into an optional desktop-shell crate and executable. Remove direct dependencies on aiplane and knowledge internals; keep the standalone power crate independent.
- [x] Extract aiplane workload implementations into an `sy-aiplane` crate; keep IPC envelopes in `sy-ipc` and runtime paths in a small configuration module.
- [x] Extract the knowledge runtime into an independent crate, preserving the current binary dispatch and existing IPC tests.
- [x] Migrate the workspace from Rust 2021 to the documented Rust 2024 edition, resolving new unsafe environment APIs at the affected boundaries.
- [x] Replace fixed `~/.local/bin/sy` paths with a package-aware executable path in rendered configs and units, then make `sy apply` stop copying an RPM-owned binary.
- [ ] Validate the Fedora 44 NPU stack on actual hardware with `sy aiplane run --workload embed` after an upgrade and record the AMD/XRT package versions.

Each crate extraction must pass `make lint`, `make test`, `make lint-spark`, and `make test-spark`. The hardware gate requires `/dev/accel/accel0` and the matching proprietary runtime.
