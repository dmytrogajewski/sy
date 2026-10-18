# 0005 — Package features as optional RPMs

- Status: accepted

## Context and problem statement

The previous packaging model treated sy as one installable binary and made users install features they might not use. The project now builds separate feature executables and packages them with related units and configuration. Users need a small base installation with a consistent command entry point, plus optional packages for independent features.

## Decision drivers

- Let users install only the features they need.
- Keep one discoverable `sy` command surface.
- Keep feature code, package declarations, configs, and service units in the same repository.
- Make package dependencies explicit and inspectable by DNF.
- Preserve the single-owner process boundary for the AMD NPU.

## Considered options

1. **One RPM with every feature.** Simple installation, but users cannot omit large or hardware-specific features.
2. **One RPM per command with no common launcher.** Fine-grained installation, but creates inconsistent command entry points and makes the feature relationship harder to discover.
3. **Base launcher plus optional feature RPMs.** A common CLI routes to dedicated executables; package metadata declares dependencies.

## Decision outcome

Choose option 3. The `sy` RPM provides `/usr/bin/sy` and shared support. Feature RPMs install their executables under `/usr/libexec/sy/` and include the units or configuration owned by that feature. The launcher dispatches to installed feature executables. `sy-desktop` supplies the packaged desktop configuration and depends on `sy-desktop-shell`.

Package names and dependencies live in [`packaging/sy.spec`](../../packaging/sy.spec). Fedora RPM build targets live in [`scripts/build-rpms.sh`](../../scripts/build-rpms.sh), with CI coverage in [`.github/workflows/rpm.yml`](../../.github/workflows/rpm.yml).

## Consequences

- Users can omit optional features and their runtime dependencies.
- A feature change may require coordinated updates to its crate, launcher routing, RPM subpackage, configs, and user documentation.
- Package installation and removal must be tested alongside command dispatch and configuration apply behavior.
- The RPM set is built for Fedora 43, 44, and 45; individual hardware integrations can support a narrower runtime matrix.
- The launcher provides a common command interface, but feature services remain separate processes with feature-specific ownership and IPC.

## Related decisions

- [ADR 0002: virtual workspace with a shared vocabulary crate](0002-virtual-workspace-with-sy-core-vocabulary.md)
- [ADR 0003: VitisAI for on-device embedding](0003-vitisai-ep-not-cuda-for-on-device-embedding.md)
