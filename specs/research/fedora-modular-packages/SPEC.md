# SPEC: Fedora packages and desktop modularity

## Context

The repository has `sy-core` and `sy-ipc`, but its original binary owns desktop, AI, agent, and knowledge modules. The first two-RPM split left these features inseparable. A small launcher now retains the `sy` CLI while feature executables move into separate packages. Power is the first independent feature; the existing desktop/AI executable remains bundled until its internal calls are replaced with IPC clients.

Fedora 44 was released in April 2026; Fedora 45 remained in final release review on 16 October 2026. The build matrix targets Fedora 43, 44, and pre-release 45. [Fedora release calendar](https://calendar.fedoraproject.org/list/Fedora%20release/).

AMD's current Ryzen AI Linux instructions describe Ubuntu 24.04 packages and an installed venv, so sy cannot declare a portable Fedora `Requires: ryzenai` without a specific compatible RPM repository. The sy RPM keeps AMD software external and the upgrade preflight checks the host before model execution. [AMD Linux installation instructions](https://ryzenai.docs.amd.com/en/latest/linux.html).

## Design

- The GitHub Actions matrix invokes the same container build script used locally; each run builds one Fedora version and uploads RPM artifacts. GitHub documents container jobs as a supported way to make the build environment explicit. [GitHub Actions container documentation](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/run-jobs-in-a-container).
- `sy` contains the small `/usr/bin/sy` launcher; `sy-desktop` owns `/usr/libexec/sy/sy-all` plus configs, themes, and settings. `sy-system-power` owns `/usr/libexec/sy/sy-system-power`. `sy apply` falls back to the packaged source tree.
- The power menu delegates to systemd, niri, and swaylock, with confirmation before logout, suspend, reboot, or power off.
- The NPU re-exec resolves the installed ORT Python tree at runtime so a Python ABI update does not silently skip the VitisAI path. The upgrade check requires a running VitisAI daemon and a finite embed vector.

## Verification

Shell syntax, RPM spec parse, source archive layout, Rust formatting, and the targeted Rust tests are local gates. CI compiles both Fedora RPMs. NPU execution requires a post-upgrade host run because the CI runners have no AMD NPU or proprietary runtime.

## Risks

Checkout configs still use a user-local binary path for direct development installs. RPM-staged units and rendered desktop configs use `/usr/bin/sy`; `sy apply` skips the binary copy for the packaged root.

## October 2026 architecture and runtime update

The RPM now puts a small launcher in `/usr/bin/sy`, the existing bundled
desktop executable at `/usr/libexec/sy/sy-all`, and the independently built
power menu at `/usr/libexec/sy/sy-system-power`. The full target still requires
separate AI, knowledge, agent, file-manager, monitoring, and auth executables;
those commands currently run from `sy-all`.

On Fedora 43, the installed `ryzenai-1.7.1`, `xrt-base-202610.2.21.75`, and
`xrt-plugin-amdxdna-2.21.260102.53` stack reported `vitisai` for embed; a live
`sy aiplane run --workload embed` returned a finite 768-dimensional vector.
This qualifies the current host, not a Fedora 44/45 upgrade.

AMD's [1.8.0 Linux instructions](https://ryzenai.docs.amd.com/en/latest/linux.html)
now describe `onnxruntime/lib` and newer XRT 2.25 packages. The current sy
re-exec and preflight probe the 1.7 `onnxruntime/capi` layout, so a 1.8 runtime
requires a separately tested adapter before claiming support.
