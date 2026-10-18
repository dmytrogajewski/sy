---
title: Install sy from RPMs
sidebar_label: Install sy
---

<!-- Template source: Good Docs Project tutorial template (CC-BY 4.0) — https://www.thegooddocsproject.dev/template/tutorial. Diátaxis quadrant: tutorial. -->

# Install sy from RPMs

In this tutorial, you build the sy RPMs for your Fedora release, install the base package and feature packages, and apply the packaged desktop configuration.

When you finish, the `sy` command is installed. If you selected `sy-desktop`, the packaged configuration is applied and the user services are available to start.

## Prerequisites

- A Fedora release listed in the [RPM package reference](../reference/packages.md).
- A graphical user session for the desktop configuration steps.
- Podman or Docker, Git, and `make`.
- `sudo` access to install RPM packages.
- A network connection for the container build and package dependencies.

## Step 1: Build the RPMs

Clone the repository and build for the Fedora release you use. This example uses Fedora 44; replace the value with your release.

```sh
mkdir -p ~/sources
git clone https://github.com/dmytrogajewski/sy.git ~/sources/sy
cd ~/sources/sy
make rpm FEDORA=44
```

The packages are written to `dist/rpm/f44/`. The script builds in a Fedora container and retains its download caches under `dist/rpm/`.

## Step 2: Install the base and feature packages

To install every package built for the host's Fedora release:

```sh
make reinstall-rpm RPM_ARGS=--all
```

This command selects the RPM directory based on the host release and invokes `sudo dnf install`. If you built for a different release, install on a matching Fedora host or rebuild for the host release first.

To choose packages manually, install `sy` and the feature RPMs you want from the matching `dist/rpm/f<release>/` directory. Package names and roles are listed in the [RPM package reference](../reference/packages.md).

## Step 3: Apply the desktop configuration

If you installed `sy-desktop`, apply its packaged configuration:

```sh
sy --root /usr/share/sy apply
```

The configuration package includes the Niri and Waybar setup. Optional feature bindings and applets appear when their packages are installed. Applying the configuration synchronizes the user units and refreshes the panel services.

## Step 4: Verify the installation

Check the launcher and installed package list:

```sh
sy --help
rpm -q sy
```

If you installed the desktop packages from a graphical session, check the panel services:

```sh
systemctl --user status sy-waybar.service sy-stack-bar.service
```

If you installed the power feature, open its menu with:

```sh
sy power
```

Use `sy power reboot --dry-run --json` to preview a power action without performing it.

## Next steps

- [Choose feature packages](../reference/packages.md)
- [Set up the optional NPU runtime](../how-to/set-up-npu.md)
- [Browse files](browse-your-files.md)
- [Search your files](search-your-files.md)
- [Read the architecture explanation](../explanation/architecture.md)
