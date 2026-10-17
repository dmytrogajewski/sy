# Journey: Rust 2024 workspace migration

## User goal

Build every sy executable and crate with the documented Rust 2024 edition while preserving CLI, IPC, desktop, and Spark behavior.

## Acceptance

- Every workspace package and the third-party plugin fixture declares edition 2024.
- Environment mutation at single-threaded CLI startup and in tests has an explicit safety rationale; tests serialize shared environment overrides where practical.
- Workspace formatting, strict Clippy, all-target tests, and both Spark gates pass.

## Verification

Run `make lint`, `make test`, `make lint-spark`, and `make test-spark` from the repository root. The plugin fixture is exercised by the workspace conformance test.

## Migration decisions

The file watcher returns an owned stream with precise Rust 2024 lifetime capture. The re-exec sentinel test runs each environment state in a child test process. Other test environment overrides hold their existing plane lock or a local test lock and restore prior values. CLI root overrides happen at process startup before workers begin. FFI declarations use `unsafe extern` and unsafe calls carry local safety justifications.
