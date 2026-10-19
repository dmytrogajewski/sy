# ROADMAP: Side-panel drag out

Source: [journey](../../journeys/JOURNEY-20261018-2339.md).

## Step 1 — Materialize clipboard files safely

Reuse the preview cache for full-resolution drag files. Reject failed decodes, use private files, and retain successful exports for the runtime session.

Files: `crates/sy-desktop-shell/src/stack/clip.rs`, `stack/cli.rs`.
Tests: screenshot byte identity, file permissions, invalid identifiers and decode failures.

- [x] Focused tests pass.
- [x] Preview and drag share export handling.

## Step 2 — Start native Wayland drags from panel slots

Share the layer-shell connection, collect pointer-grab serials on a separate event queue, advertise copy-only URI/text offers and serve requested data without blocking the UI. Keep click/menu behavior and suppress hover while dragging.

Files: `crates/sy-desktop-shell/src/stack/bar/app.rs`, new `bar/drag.rs`, crate manifest.
Tests: URI encoding, MIME data, threshold/cancellation, real protocol source/transfer coverage. Manual terminal recipe is in the journey.

- [x] Focused tests pass.
- [x] Native protocol test passes.
- [ ] Manual graphical recipe verified.

## Cross-cutting Definition of Done

- [x] README updated.
- [x] `make lint` and `make test` pass.
- [x] `make lint-spark` and `make test-spark` pass.

## Out of Scope

Inbound panel drops and changes to the file manager.

## Verification record

`make lint` passed. Two consecutive `make test` runs each passed 1,658 tests across 92 test binaries. `make lint-spark` and `make test-spark` passed (17 bridge tests). Native socket tests cover file and text transfer, rejection/cancellation, and release before the UI tick. The PolicyKit fixture fix passed 100 focused parallel repetitions; see [bug record](../../bugs/BUG-20261018-2355.md).

The native Fedora 44 RPM was rebuilt and `sy-desktop-shell-0.1.0-18.fc44` reinstalled on 2026-10-19. `sy-stack-bar.service` was restarted and reported active/running with status `ready`; its process inode matches the installed executable and its ELF build ID matches the release build (`8390fecc03796f36cf561dbe1b28f93df98db62d`). RPM verification passed and startup logged no drag initialization errors. Interactive verification against a live terminal remains pending.
