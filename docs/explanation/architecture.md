---
title: Architecture
sidebar_label: Architecture
---

<!-- Template source: Good Docs Project explanation template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/explanation. Diátaxis quadrant: explanation. -->

# Architecture

This page explains how sy's separately packaged features fit together. For installation steps, see the [RPM tutorial](../tutorials/getting-started.md). For commands and options, see the [CLI reference](../reference/cli.md).

## Shared entry point, separate features

The RPM set has a small base package and optional feature packages. The base package installs `/usr/bin/sy`, the command launcher. Feature RPMs install dedicated executables under `/usr/libexec/sy/`; the launcher routes commands to the executable for an installed feature. See the [RPM package reference](../reference/packages.md) for package names and roles.

Feature packages include power controls, agent tools, authentication integration, AI inference, semantic knowledge, monitoring, a desktop shell, and a file manager. `sy-desktop` supplies the declarative desktop configuration and depends on `sy-desktop-shell`. Users can leave out unrelated features.

Separate executables make package installation modular. They do not mean every feature is an independent systemd service: each package owns the processes and user units required by that feature. The launcher gives users a consistent command surface across packages.

## Configuration and supervision

The repository keeps desktop templates and systemd unit definitions under `configs/`. The `sy-desktop` RPM installs the packaged configuration at `/usr/share/sy`. Run `sy --root /usr/share/sy apply` to apply it. When working from a checkout, `sy apply` uses that checkout instead.

The user-level `sy.target` groups sy's user services. Feature packages provide their own service units; installing a package does not imply that every service is always active. `sy apply` synchronizes packaged user units and desktop configuration. It also accounts for optional features installed on the host.

## Communication between features

The launcher dispatches a command to a feature executable. Long-running features communicate through the IPC interfaces documented for those services, commonly JSON messages over Unix sockets in the user runtime directory. The launcher itself does not turn every capability into one long-lived process.

The AI plane owns the AMD NPU device when NPU inference is enabled. CLI and knowledge clients use the AI service interface instead of opening a second NPU context. CPU execution does not require the AMD runtime. The `sy-system-aiplane-ryzenai` package provides host integration; AMD runtime packages and model files are external dependencies.

## Fedora and package support

The RPM build workflow currently targets Fedora 43, 44, and 45. That matrix describes package builds; a hardware-specific feature can have narrower runtime qualification. For example, Ryzen AI support depends on the separately installed AMD runtime and the host kernel/device setup. Read the [NPU setup guide](../how-to/set-up-npu.md) before enabling it.

## Why this structure

The shared repository keeps package metadata, feature code, configuration, and service definitions reviewable together. Separate RPMs let an installation include only the features it needs, while the common launcher and configuration workflow avoid requiring users to learn a different top-level command for each package.

The trade-off is that package boundaries and command routing must stay synchronized. RPM build checks and CI verify package construction; the feature reference pages describe user-visible behavior. When adding a feature, update its crate, package declaration, launcher routing, config or units as needed, and documentation together.

## Related pages

- [What sy is](what-sy-is.md)
- [Install sy from RPMs](../tutorials/getting-started.md)
- [Feature package list](../reference/packages.md)
- [ADR 0005: feature RPM boundaries](../adr/0005-feature-rpm-boundaries.md)
- [Configuration reference](../reference/config.md)
- [Glossary](../reference/glossary.md)
