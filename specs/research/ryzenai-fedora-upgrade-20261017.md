# Ryzen AI compatibility after a Fedora upgrade

AMD's [Ryzen AI 1.8 Linux installation guide](https://ryzenai.docs.amd.com/en/latest/linux.html) describes an `onnxruntime/lib` directory under `RYZEN_AI_INSTALLATION_PATH` and a matching XRT driver stack distributed as Ubuntu 24.04 packages. The guide instructs users to source `/opt/xilinx/xrt/setup.sh` before NPU execution. It does not qualify a Fedora RPM or the previous 1.7 Python `site-packages/onnxruntime/capi` layout.

The current sy VitisAI adapter is qualified for the Fedora 43 Ryzen AI 1.7 runtime layout. It discovers the Python ABI directory rather than pinning a `python3.x` path, and the packaged upgrade check verifies ORT, VitisAI config, XRT, the device node, daemon backend, and a finite 768D embed result. A Fedora upgrade that preserves the 1.7 runtime layout can be checked using `/usr/share/sy/scripts/check-npu-upgrade.sh` after reinstalling matching sy RPMs.

Ryzen AI 1.8 changes the library layout and driver version. The preflight identifies that layout and fails before an unqualified workload uses it. Its adapter needs a host with the 1.8 runtime and model artifacts to test loader paths, EP registration, and real NPU execution. A passing build or CPU run is insufficient to qualify it.

AMD's current Linux guide documents an Ubuntu package flow. The [Fedora XRT
package overview](https://packages.fedoraproject.org/pkgs/xrt/xrt/) currently
lists Rawhide, so the Fedora 44 NPU upgrade needs a separately verified
Ryzen AI/XRT package source and cannot be inferred from the sy RPM build.

The CI matrix builds Fedora 43, 44, and 45 sy RPMs. It checks package construction and the standalone command surfaces; hardware qualification remains a host-only check because the containers have neither AMD's proprietary runtime nor `/dev/accel/accel0`.

The post-upgrade script now prints the Fedora release, kernel, and installed
AMD/XRT RPM versions after a successful VitisAI embed. Keep that output with
the Fedora 44 hardware run. A Fedora 44 container package install validates
the base RPM's dependency resolution and optional command routing after
package extraction. It cannot establish NPU runtime compatibility.

## Fedora 43 host check (2026-10-17)

On the current Fedora 43 host, kernel `7.1.5-101.fc43.x86_64`, the
upgrade-check script completed an embed request through the running VitisAI
daemon and returned 768 finite vector dimensions. Installed RPMs were
`ryzenai 1.7.1-1.fc43`, `xrt-base 202610.2.21.75-1.fc43`,
`xrt-npu 202610.2.21.75-1.fc43`, and
`xrt-plugin-amdxdna 2.21.260102.53-1.fc43`. This validates the current host,
while Fedora 44 remains a separate hardware gate after the operating-system
upgrade.
