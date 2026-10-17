# Journey: reinstall sy after a Fedora upgrade

**Actor:** sy desktop user upgrading Fedora 43 to 44.
**Goal:** install matching RPMs, restore the desktop, and verify the AI plane.
**Constraint:** NPU runtime packages may change independently of sy and are not part of Fedora's standard sy build.

1. Build `make rpm` locally or download the matching CI artifacts. The current set contains the base launcher, desktop bundle, and standalone power package.
2. Install or reinstall the matching RPMs with `make reinstall-rpm` after a local build. The launcher is `/usr/bin/sy`; feature executables live under `/usr/libexec/sy`.
3. Run `sy --root /usr/share/sy apply`. It renders the packaged desktop config and syncs user units that invoke `/usr/bin/sy`.
4. Open the power menu from Waybar or `Mod+Shift+Escape`; choose an action and confirm session-ending actions.
5. Run `/usr/share/sy/scripts/check-npu-upgrade.sh` on an NPU host. It checks the device, AMD ORT, XRT, and daemon backend, then verifies a real embed result through the VitisAI daemon.

## Failure paths

- Missing Fedora runtime RPM: the sy RPM still installs; the NPU check reports the missing runtime.
- Changed Python ABI: sy discovers the installed AMD venv package tree by locating ORT.
- Missing NPU device after kernel update: inspect `sy-amdxdna-load.service` and the kernel journal; the script exits nonzero.
- Power menu dismissed: no action runs.
