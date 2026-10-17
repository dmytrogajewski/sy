# Modular packages implementation run

## Roadmap

`specs/roadmaps/fedora-modular-packages/ROADMAP.md`

## Starting condition

Release 13 provides the power menu, independent agent, auth, AI, and file manager commands, Fedora 43 and 44 RPMs, and an AI daemon. Desktop, knowledge, and monitoring remain in the bundled executable. The host is Fedora 43, so Fedora 44 hardware qualification is unavailable here.

## Plan

1. Extract knowledge, monitoring, and desktop shell code and commands.
2. Give each executable an RPM and route the launcher to its optional package.
3. Complete the AI IPC/config boundary and migrate to Rust 2024.
4. Run workspace, Spark, and RPM gates; qualify Fedora 44 hardware when available.

## Timeline

- 2026-10-17 10:12 UTC: Resumed the first unchecked architecture item. Delegated independent knowledge runtime crate extraction to an implementation worker.
- 2026-10-17 10:15 UTC: Added launcher routing tests for independent knowledge, monitor, and shell executables; observed the expected failures, implemented routes, and passed all 13 launcher dispatch tests.
- 2026-10-17 10:19 UTC: Extended the NPU upgrade smoke script to print Fedora, kernel, and installed AMD/XRT RPM versions after a verified VitisAI embed. The hermetic upgrade test passed.
- 2026-10-17 10:25 UTC: Fixed reinstall selection so existing desktop installations do not silently add every optional feature. Base-only and desktop-subset reinstall tests passed.
- 2026-10-17 10:27 UTC: Knowledge worker completed the standalone runtime crate and passed make lint/test and Spark gates. Added a separate knowledge executable, per-user packaged source configuration, CLI tests, and package ownership.
- 2026-10-17 10:35 UTC: Desktop shell worker completed the separate crate and executable with unit and CLI tests. Added launcher routing for its applets and package ownership.
- 2026-10-17 10:42 UTC: Made packaged Waybar knowledge entry conditional on the knowledge RPM; introduced a generic RPM desktop config and retained personal source settings only in the checkout.
- 2026-10-17 10:44 UTC: Monitor worker completed the standalone crate, CLI, doctor boundary, and IPC tests. AI IPC worker moved wire types into sy-ipc and runtime paths into a configuration module; their targeted and Spark gates passed.
- 2026-10-17 10:45 UTC: Replaced README-dependent pixel goldens in two file-manager end-to-end probes with nonblank live-preview checks; the renderer's canonical golden remains in its own test. Both targeted journey tests passed.
- 2026-10-17 10:46 UTC: Began workspace-wide Rust 2024 migration. RPM spec now separates knowledge, monitor, and desktop shell files; package builds remain to be run after migration.
- 2026-10-17 11:12 UTC: Rust 2024 migration passed its complete workspace and Spark gates. The integrated tree then passed `make lint`, `make test`, `make lint-spark`, and `make test-spark`.
- 2026-10-17 11:36 UTC: Built release 14 Fedora 43 RPMs and the source RPM. Verified base installation in a clean Fedora 43 container and routed optional command surfaces from their respective RPMs. The source archive contains only the generic packaged configuration.
- 2026-10-17 12:12 UTC: Fedora 44 build continues against official release repositories. Network mirror interruptions are slowing dependency retrieval; a persistent DNF cache and checksum-verified package downloads preserve progress.
- 2026-10-17 15:08 UTC: Built Fedora 44 release 14 RPMs and source RPM in a Fedora 44 container after checksum-verifying all 131 system dependencies. The RPM `%check` command surfaces passed. A clean Fedora 44 container installed the base RPM with dependency checks; a second clean container installed all optional RPMs with dependency resolution bypassed for a command-routing smoke test. The Fedora 44 source RPM contains only the generic packaged desktop configuration.
- 2026-10-17 15:08 UTC: Closed the remaining software architecture checklist item. Actual Fedora 44 NPU hardware qualification remains open because this machine runs Fedora 43.
- 2026-10-17 15:09 UTC: Ran `scripts/check-npu-upgrade.sh` on the Fedora 43 host. The live VitisAI daemon returned a finite 768-dimensional embed; the script recorded kernel `7.1.5-101.fc43.x86_64`, Ryzen AI `1.7.1-1.fc43`, XRT base/NPU `202610.2.21.75-1.fc43`, and AMD XDNA plugin `2.21.260102.53-1.fc43`.

## Assumptions

- The roadmap uses checklist bullets rather than the march skill's numbered-step format; checklist order is authoritative.
- Fedora 44 NPU qualification requires an upgraded host and cannot be inferred from Fedora 44 container builds.
