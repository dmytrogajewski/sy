# sy

`sy` is a modular desktop and agent environment for Fedora. The base RPM installs the command launcher and shared configuration tools; optional RPMs add power controls, AI inference, file management, monitoring, and other features.

You can install only the features you need. Each feature is packaged separately, while the repository keeps the commands, configs, systemd units, and package metadata together.

## Install from RPMs

The build and CI matrix currently targets Fedora 43, 44, and 45. Build packages locally with Podman or Docker:

```sh
make rpm FEDORA=44
```

Packages are written to `dist/rpm/f44/`. To install the base package and every built feature package on a fresh matching Fedora host:

```sh
make reinstall-rpm RPM_ARGS=--all
```

To rebuild and reinstall the package set already installed on the host, use `make reinstall-rpm`. For a desktop session, install `sy-desktop` as well; it supplies the declarative Niri and Waybar configuration and depends on the desktop shell. Applying the packaged configuration uses:

```sh
sudo dnf install ./dist/rpm/f44/sy-desktop-*.rpm
sy --root /usr/share/sy apply
```

The command above assumes the base and other desired packages are already installed. See the [package installation guide](docs/tutorials/getting-started.md) for selecting packages and verifying services. RPM filenames vary by release and build metadata; use the exact files generated in your output directory.

## Choose features

See the [RPM package reference](docs/reference/packages.md) for package
roles, dependencies, and feature requirements. Package metadata is
defined in [`packaging/sy.spec`](packaging/sy.spec).

## Try it

After installing the power feature in a graphical session:

```sh
sy power
```

Preview a power action without performing it:

```sh
sy power reboot --dry-run --json
```

For the AI plane, install `sy-system-aiplane`, then check its CLI and service:

```sh
sy aiplane --help
systemctl --user status sy-aiplane.service
```

NPU inference additionally needs compatible AMD Ryzen AI runtime packages and a prepared model. The AMD runtime and model files are not shipped in sy RPMs. See [NPU setup](docs/how-to/set-up-npu.md).

## Architecture

The `sy` command is the user-facing launcher. Feature crates build dedicated executables that implement their own commands and services. The RPM split lets users install features independently; shared configuration and IPC contracts connect features where needed. See [architecture](docs/explanation/architecture.md) for process and service boundaries.

## Documentation

- [Start here](docs/intro.md)
- [Tutorials](docs/tutorials/)
- [How-to guides](docs/how-to/)
- [CLI and configuration reference](docs/reference/)
- [Architecture explanation](docs/explanation/architecture.md)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)

## Development

Build the workspace with Rust:

```sh
cargo build --workspace
make lint
make test
```

Build RPMs with `make rpm FEDORA=<release>`. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for development setup and required checks.

## Maintainer

`sy` is maintained by [Dmitriy Gajewski](GOVERNANCE.md#roles). See [governance](GOVERNANCE.md) for project decision ownership.

## License

MIT. See [`LICENSE`](LICENSE).
