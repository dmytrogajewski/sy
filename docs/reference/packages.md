---
title: RPM packages
sidebar_label: RPM packages
---

# RPM packages

This page lists the Fedora RPMs produced by the repository. Package names, dependencies, and file ownership are defined in [`packaging/sy.spec`](https://github.com/dmytrogajewski/sy/blob/main/packaging/sy.spec). The RPM build script currently supports Fedora 43, 44, and 45. A package build does not guarantee that every hardware-specific runtime is qualified on every release.

## Base and desktop packages

| Package | Purpose |
|---|---|
| `sy` | Base command launcher and shared configuration support. |
| `sy-desktop` | Declarative Niri desktop configuration, themes, and desktop integration. Depends on `sy-desktop-shell`. |
| `sy-desktop-shell` | Waybar integration, layer-shell bar, and desktop applets. |

## Optional feature packages

| Package | Purpose | Additional requirement |
|---|---|---|
| `sy-system-power` | Lock, logout, suspend, reboot, and power-off commands. | `fuzzel`, `swaylock` |
| `sy-system-agent` | Sandboxed agent runner, policy tools, and user unit. | `fuzzel`, `foot` |
| `sy-system-auth` | `syauth` command and SELinux policy sources. | The syauth daemon and PAM module are installed separately. |
| `sy-system-aiplane` | AI workload commands and inference service; CPU execution is available. | AMD runtime is needed for AMD NPU execution. |
| `sy-system-aiplane-ryzenai` | AMD NPU boot integration and post-upgrade verification. | Depends on `sy-system-aiplane`; AMD runtime and model files are external. |
| `sy-system-knowledge` | Semantic search, local Qdrant integration, and user units. | Configure Qdrant and knowledge sources as described in the [knowledge guide](../how-to/add-a-knowledge-source.md). |
| `sy-system-monitor` | System metrics collector, monitor CLI, MCP server, and user unit. | Desktop UI use requires a graphical session. |
| `sy-filemanager` | Native file manager, IPC daemon, and preview plugin host; packages the Markdown plugin executable. | The GUI currently handles text and Markdown itself as syntax-highlighted source. External plugins are used for MIME types without a built-in GUI preview. |

## Build and install

Build the complete RPM set for one Fedora release:

```sh
make rpm FEDORA=44
```

This writes packages under `dist/rpm/f44/`. Replace `44` with the Fedora release you intend to install on. To install every package from the host release's output directory, run:

```sh
make reinstall-rpm RPM_ARGS=--all
```

For a smaller installation, use DNF to install `sy` plus only the feature packages you need. Install `sy-desktop` if you want the packaged desktop configuration, then apply it with:

```sh
sy --root /usr/share/sy apply
```

The installer and package list are documented in the [getting-started tutorial](../tutorials/getting-started.md).
