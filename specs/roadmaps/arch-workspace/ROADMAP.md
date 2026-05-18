# ROADMAP: arch-workspace — split `sy` into a cargo workspace

Source: `specs/research/architecture-refactor/SPEC.md` §3.2 K1, §3.3 Zone 1, Appendix A "Z1".

## Overview

Convert the single-crate repo into a virtual cargo workspace whose
first members are a thin `sy` binary, a minimal `crates/sy-core` for
shared vocabulary (`WorkloadKind`, `Priority`, `ErrorCode`, IPC
envelope types), and a `crates/sy-testutils` dev-dep crate that
formalises the daemon-in-thread integration harness already used in
`scripts/prep_npu_workload.py` and the recent
`9bd8ba5 prep_npu_workload.py + daemon-in-thread integration test`.
End state: `cargo build` produces the same binary; `cargo build
-p sy-core` builds the vocabulary crate standalone; no behaviour
change; `src/main.rs` LOC stays ≤ its current size and is bounded by
CI for the future.

This zone deliberately **does not** move `aiplane/`, `knowledge/`,
`stack/`, or `agt/` into their own crates — those are follow-on
roadmaps (`arch-ipc-v1` carves out `sy-ipc`; later splits land per
subsystem once `sy-core` is stable). Reordering exists so a noisy
workspace diff doesn't intermix with behavioural change.

---

## Step 1 — Introduce the workspace shell with `sy` and `sy-core`

**Goal:** root `Cargo.toml` becomes a virtual workspace with two
members (`.` and `crates/sy-core`), lockstep-versioned via
`[workspace.package]`. `sy-core` is empty-ish (one `lib.rs` with a
`pub use` re-export wall). Root crate gains a `sy-core` path dep but
does not use it yet. No behaviour change, no module moves.

**Files:**
- `Cargo.toml:1-82` (modified) — add `[workspace]` table with
  `members = [".", "crates/sy-core"]`, `resolver = "2"`,
  `[workspace.package]` with `version = "0.1.0"`, `edition = "2021"`,
  `[workspace.dependencies]` collecting the current `[dependencies]`
  block so members can pick versions via `<name>.workspace = true`.
  Root `[package]` switches to `version.workspace = true`.
- `crates/sy-core/Cargo.toml` (new) — `name = "sy-core"`,
  `publish = false`, `version.workspace = true`, `edition.workspace =
  true`. No deps yet (intentionally — see Risks §6 in the SPEC re:
  hub-rebuild penalty).
- `crates/sy-core/src/lib.rs` (new) — `//! sy-core: shared
  vocabulary…`. Empty body other than the module doc.

**Tests:**
- `crates/sy-core/src/lib.rs::tests::crate_compiles` — trivial
  `assert!(true)` placeholder that pins the crate as a real cargo
  member (removable once Step 2 lands real types).

**Definition of Done:**
- [x] `cargo build` (workspace-default) succeeds; binary still
      produced at `target/debug/sy` (677 MB debug, unchanged shape).
- [x] `cargo build -p sy-core` succeeds standalone.
- [x] `cargo build -p sy` succeeds (root crate still builds with
      the same 16 pre-existing dead-code warnings as `main`).
- [x] `make test` green; 75 sy tests preserved + 1 new
      `sy-core::tests::crate_compiles` (76 total).
- [~] `make lint` green for `sy-core` in isolation
      (`cargo clippy -p sy-core --all-targets -- -D warnings` →
      finished, zero warnings). **`make lint` red workspace-wide
      on the same 51 pre-existing clippy errors in `src/main.rs` /
      `src/auto_mcp.rs` / `src/aiplane/**` that the stack-bar-ux
      roadmap also called out — separate cleanup pass owed. No
      new clippy violations introduced by this step.** Makefile's
      `lint` / `test` / `build` targets gained `--workspace` so
      the gate covers `sy-core` once the pre-existing debt clears.
- [x] No new `#[allow(dead_code)]`, no `TODO`/`FIXME`.
- [x] AGENTS.md / README unchanged (no user-facing API change).

**Risks / unknowns:**
- ~~`iced_layershell` and `ort` pinning under
  `[workspace.dependencies]` may surface a version-resolution
  conflict that `[dependencies]` papered over.~~ — **resolved**:
  `cargo tree -d` reports 52 duplicate root crates pre- and
  post-conversion (verified by cloning the pre-change tree to
  `/tmp/sy-pre`). Zero new duplicates introduced. All four pre-
  existing duplicate chains (`base64`, `calloop`, `tokenizers`'
  `spm_precompiled`-pinned `base64@0.13`, `iced`/`iced_layershell`'s
  divergent `calloop`/`smithay-client-toolkit`/`winit` worlds) are
  framework-side and unchanged.

---

## Step 2 — Extract `WorkloadKind`, `WorkloadInput`, `WorkloadOutput`, `WorkloadHealth` into `sy-core`

**Goal:** the type vocabulary the IPC layer + registry + workloads
all reference moves to `sy-core`. Existing call sites switch to
`use sy_core::…`; no behavioural code moves. This is the spec's "K1
matklad hub-crate" pattern made concrete.

**Files:**
- `crates/sy-core/src/lib.rs` (modified) — declare `pub mod
  workload;` and `pub mod ipc;`, re-export the public names.
- `crates/sy-core/src/workload.rs` (new) — cut-paste
  `WorkloadKind` (`src/aiplane/registry.rs:22-77`),
  `WorkloadInput` / `WorkloadOutput` (around
  `src/aiplane/registry.rs:80-200`), and `WorkloadHealth` (same
  file, search for `pub struct WorkloadHealth`). No logic — these are
  pure ser/de data shapes.
- `crates/sy-core/Cargo.toml` (modified) — add
  `serde.workspace = true`, `serde_json.workspace = true`,
  `anyhow.workspace = true` as needed by the moved types only.
- `src/aiplane/registry.rs:22-77,80-200` (modified) — replace the
  in-line type definitions with `pub use sy_core::workload::{
  WorkloadKind, WorkloadInput, WorkloadOutput, WorkloadHealth};`
  for back-compat of the `super::registry::` re-export path used by
  `src/aiplane/ipc.rs:31`.
- `Cargo.toml` (modified) — root crate gains
  `sy-core = { path = "crates/sy-core" }` under `[dependencies]`.

**Tests:**
- `crates/sy-core/src/workload.rs::tests::workload_kind_round_trip` —
  serialise every `WorkloadKind` variant to JSON and deserialise
  back. Pins the wire schema.
- `crates/sy-core/src/workload.rs::tests::workload_input_tagged_union_round_trip`
  — same for `WorkloadInput`.
- Plus seven schema-pinning siblings landed alongside the two
  spec'd tests:
  `workload_kind_kebab_case_on_wire`,
  `workload_kind_from_str_round_trips_via_as_str`,
  `workload_kind_from_str_rejects_unknown`,
  `workload_output_tagged_union_round_trip`,
  `workload_state_ready_serializes_with_backend`,
  `workload_state_default_is_not_prepared`,
  `workload_health_default_serializes_with_not_prepared`.
  Each pins a distinct wire-shape invariant the spec calls out
  (`kebab-case` discriminator on `WorkloadKind`, PascalCase-free
  `state` tag on `WorkloadState`, `Default` semantics survive
  serde). Two of these replaced near-duplicate tests removed from
  `src/aiplane/registry.rs::tests`
  (`kind_roundtrip_via_str`, `kind_rejects_unknown_string`,
  `workload_state_ready_serializes_with_backend`,
  `workload_state_default_is_not_prepared` — the vocabulary now
  owns its own coverage).

**Definition of Done:**
- [x] Types live in `sy-core::workload`
      (`crates/sy-core/src/workload.rs:15-178`); root crate re-
      exports via `pub use sy_core::workload::{…}` at
      `src/aiplane/registry.rs:23-25`. Zero consumer-side import
      changes — all 16 `use …::registry::{Workload…}` sites still
      compile (`src/aiplane/cli.rs:15`, `src/aiplane/ipc.rs:31`,
      `src/aiplane/status.rs:24-26`,
      `src/aiplane/supervisor/{child,mod}.rs`,
      `src/aiplane/worker{,_ipc}/runner.rs`,
      `src/aiplane/workloads/{embed,fake,ocr,rerank,stt,vad}.rs`,
      `src/knowledge/{daemon,embed}.rs`).
- [x] Tests above pass — 9 tests in `sy-core::workload::tests`,
      run via `cargo test -p sy-core`.
- [x] `make test` green — 71 sy + 9 sy-core = 80 (Step-1 baseline
      was 75 + 1 = 76; net +4 = 9 new in sy-core minus 4
      duplicates pruned from `src/aiplane/registry.rs::tests`).
      Behaviour-preserving Registry tests
      (`registry_dispatches_to_registered_workload_via_trait_object`,
      `all_health_enumerates_every_registered_kind`,
      `registry_rejects_unregistered_kind`,
      `cache_root_respects_override`) untouched.
- [~] `make lint` green for `sy-core` in isolation
      (`cargo clippy -p sy-core --all-targets -- -D warnings`).
      **`make lint` red workspace-wide on the same 51 pre-existing
      clippy errors as Step 1 — no new clippy violations
      introduced by this step.** Initial post-edit `pub use` of
      `SpeechSpan` triggered an `unused_imports` warning; resolved
      by dropping `SpeechSpan` from the re-export list (no
      consumer imports the type by name; `WorkloadOutput::Spans`
      destructuring continues to work transparently).
- [x] `cargo tree -d -p sy-core` reports `"nothing to print"` —
      `sy-core` pulls only `anyhow` + `serde` (+ `serde_derive`
      proc-macro) at runtime, `serde_json` as a dev-dep, zero
      duplicate version chains. Hub-crate boundary preserved.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- ~~`WorkloadInput` and `WorkloadOutput` currently `derive(Debug,
  Serialize, Deserialize)`; moving them across crates may expose a
  hidden trait-object dep on `super::session::SessionPool`.~~ —
  **resolved**: types compile in `sy-core` with only
  `serde::{Serialize, Deserialize}` derives. No latent
  `SessionPool` coupling; the `Workload` trait (only thing that
  references `SessionPool`) stays in `src/aiplane/registry.rs`.

---

## Step 3 — Add `Priority` and `ErrorCode` to `sy-core`

**Goal:** introduce the two forward-looking vocab types that Zones 2
and 3 will consume. They land in `sy-core` ahead of their callers so
the schema is stable when later zones plug in.

**Files:**
- `crates/sy-core/src/priority.rs` (new) —
  `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize,
  Deserialize)] #[serde(rename_all = "PascalCase")] pub enum
  Priority { Realtime, Interactive, Background, Batch }`. Add
  `impl Priority { pub const ALL: [Priority; 4] = …; pub fn
  as_str(self) -> &'static str; }` and `FromStr` for CLI parsing.
- `crates/sy-core/src/error.rs` (new) — `ErrorCode` enum:
  `Overloaded`, `Cancelled`, `Timeout`, `PolicyDenied`,
  `ConsentRequired`, `IncompatibleSchema`, `NotReady`,
  `NpuUnavailable`, `Internal`, `BadRequest`. `#[serde(rename_all =
  "PascalCase")]` to match the wire shape in SPEC §4.2.
- `crates/sy-core/src/lib.rs` (modified) — declare `pub mod
  priority; pub mod error;`, re-export `Priority` and `ErrorCode` at
  the crate root.

**Tests:**
- `crates/sy-core/src/priority.rs::tests::priority_round_trip` —
  every variant survives JSON ser/de with `PascalCase` casing
  (`"Realtime"`, `"Interactive"`, `"Background"`, `"Batch"`).
- `crates/sy-core/src/priority.rs::tests::priority_from_str_is_case_sensitive_pascal`
  — `"Interactive".parse::<Priority>()` works; `"interactive"`,
  `"inter-active"`, `"INTERACTIVE"` all reject (SPEC §4.7's CLI
  default is PascalCase `Interactive`).
- `crates/sy-core/src/error.rs::tests::error_code_pascal_case_wire`
  — `Overloaded` serialises as `"Overloaded"`, not `"overloaded"`.
- Plus nine schema-pinning siblings landed alongside the three
  spec'd tests:
  `priority_pascal_case_on_wire` (per-variant wire form check),
  `priority_from_str_round_trips_via_as_str`,
  `priority_all_has_four_entries` (guards SPEC §3.2 K3's
  exactly-four-classes commitment),
  `priority_display_matches_as_str`,
  `error_code_round_trip_all_variants`,
  `error_code_all_listed_count_matches_spec` (guards the
  ten-variant surface from SPEC §3.3 / §4.2 / §4.7),
  `error_code_from_str_round_trips_via_as_str`,
  `error_code_from_str_rejects_unknown`,
  `error_code_exit_codes_match_spec_table` (binds the
  `ErrorCode → exit code` mapping to SPEC §4.7's stable
  exit-code table),
  `error_code_no_variant_maps_to_zero_exit` (asserts no
  `ErrorCode` ever smuggles a failure through `$?` as success).

**Definition of Done:**
- [x] Both new files exist
      (`crates/sy-core/src/priority.rs`,
      `crates/sy-core/src/error.rs`), declared and re-exported
      from `sy-core` at `crates/sy-core/src/lib.rs:13-21` via
      `pub use error::ErrorCode;` + `pub use priority::Priority;`.
- [x] All four `Priority` variants present —
      `Realtime / Interactive / Background / Batch` —
      enforced by `priority_all_has_four_entries`. `Priority::ALL`
      is ordered highest→lowest so the future scheduler can
      iterate strict-priority directly.
- [x] All ten `ErrorCode` variants from SPEC §4.2 / §4.7 present:
      `Overloaded`, `Cancelled`, `Timeout`, `PolicyDenied`,
      `ConsentRequired`, `IncompatibleSchema`, `NotReady`,
      `NpuUnavailable`, `Internal`, `BadRequest` — enforced by
      `error_code_all_listed_count_matches_spec`. Each variant
      carries an `.exit_code() -> i32` mapping the SPEC §4.7
      table (1 generic / 2 usage / 4 not-ready / 5 overloaded /
      6 consent / 7 policy denied; exit 0 reserved for success,
      guarded by `error_code_no_variant_maps_to_zero_exit`).
- [x] Tests above pass — `cargo test -p sy-core`: 22 tests
      (9 workload + 6 priority + 7 error). Workspace test count
      run twice for flake-check: 71 sy + 22 sy-core = 93 stable.
- [~] `make lint` green for `sy-core` in isolation
      (`cargo clippy -p sy-core --all-targets -- -D warnings`)
      and `cargo fmt -p sy-core --check` clean. **`make lint`
      red workspace-wide on the same 51 pre-existing clippy
      errors as Steps 1 & 2 — no new violations introduced.**
      `cargo tree -d -p sy-core` still `"nothing to print"` —
      `Priority` + `ErrorCode` pulled zero new deps (`serde` +
      `anyhow` already in place from Step 2).
- [x] No `#[allow(dead_code)]` despite no in-tree consumer yet.
      `pub use error::ErrorCode; pub use priority::Priority;` at
      the lib crate root counts as use; rustc's dead-code lint
      doesn't fire on public re-exports there.

**Risks / unknowns:**
- ~~Naming bikeshed (`Realtime` vs `RealTime`, `Background` vs
  `Bulk`).~~ — **resolved** by picking SPEC §3.2 K3 names verbatim
  and pinning the wire form in `priority_pascal_case_on_wire`.

---

## Step 4 — Scaffold `crates/sy-testutils` (dev-dep daemon-in-thread harness)

**Goal:** formalise the harness already exercised by the recent
`9bd8ba5 prep_npu_workload.py + daemon-in-thread integration test`
so future zones (especially Zone 2 IPC v1 + Zone 3 scheduler) can
import it as a dev-dep rather than re-implementing the boilerplate.

**Files:**
- `crates/sy-testutils/Cargo.toml` (new) — `publish = false`,
  `version.workspace = true`, deps: `tokio.workspace = true`,
  `tempfile.workspace = true`, `anyhow.workspace = true`, plus a
  path dep on `sy-core`.
- `crates/sy-testutils/src/lib.rs` (new) — public API:
  - `pub struct DaemonHandle { … }` with `pub async fn shutdown
    (self) -> Result<()>`.
  - `pub fn spawn_in_thread<F, Fut>(f: F) -> DaemonHandle where F:
    FnOnce(IsolatedRuntimeDir) -> Fut + Send + 'static, Fut: Future
    + Send`.
  - `pub struct IsolatedRuntimeDir { … }` that allocates a
    `tempfile::TempDir`, sets `XDG_RUNTIME_DIR` for the thread, and
    cleans up on drop.
- `Cargo.toml` root (modified) — workspace `members` gains
  `"crates/sy-testutils"`.

**Tests:**
- `crates/sy-testutils/src/lib.rs::tests::isolated_runtime_dir_round_trip`
  — allocate, write a file, drop, assert the dir is gone.
- `crates/sy-testutils/src/lib.rs::tests::spawn_in_thread_runs_and_shuts_down`
  — start a no-op daemon closure; assert `shutdown()` returns
  within a second.

**Definition of Done:**
- [x] `cargo build -p sy-testutils --tests` succeeds. Workspace
      `members` updated at `Cargo.toml:8` to include
      `"crates/sy-testutils"`.
- [x] Tests above pass — `cargo test -p sy-testutils` reports
      `2 passed` (`isolated_runtime_dir_round_trip` +
      `spawn_in_thread_runs_and_shuts_down`). The
      `isolated_runtime_dir_round_trip` test asserts the tempdir's
      `XDG_RUNTIME_DIR` is observable while the dir is alive,
      writes a marker file, drops the dir, and confirms the
      tempdir was removed — the full lifecycle the daemon-in-
      thread pattern needs.
- [~] `make lint` green for `sy-testutils` in isolation
      (`cargo clippy -p sy-testutils --all-targets -- -D warnings`)
      and `cargo fmt -p sy-testutils --check` clean.
      **`make lint` red workspace-wide on the same 51 pre-
      existing clippy errors in `src/main.rs` / `src/auto_mcp.rs`
      / `src/aiplane/**` as Steps 1-3 — unchanged.** No new
      violations introduced.
- [x] `make test` shows the two new tests in the count — workspace
      total 71 sy + 22 sy-core + 2 sy-testutils = 95, run twice
      for flake-check, stable.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`. `_temp` /
      `_guard` field names are intentional RAII-only fields
      (rustc's underscore-prefix convention; not lint-suppressed).
- [x] `cargo tree -d -p sy-testutils` reports `"nothing to print"`
      — zero new duplicate version chains. Dep tree:
      `anyhow + sy-core + tempfile + tokio` only.

**Risks / unknowns:**
- ~~`XDG_RUNTIME_DIR` is process-wide, not thread-local.~~ —
  **mitigated**: the harness owns a private `static Mutex<()>
  TEST_ENV_LOCK` that `IsolatedRuntimeDir::new()` locks before
  touching the env, holding the guard until the dir drops. Two
  concurrent test threads inside the same process serialise on
  this lock rather than corrupting each other's view of
  `$XDG_RUNTIME_DIR`. Field-drop ordering (`_temp` → restore env
  → `_guard`) is documented inline. The existing
  `crate::aiplane::TEST_ENV_LOCK` (in `src/aiplane/mod.rs`) stays
  — sy-testutils intentionally doesn't reach into `sy::*` so it
  can remain a dev-dep without circular coupling.

---

## Step 5 — Add a CI lint that bounds `src/main.rs` LOC

**Goal:** SPEC §6 Risks calls out "Workspace split lands but
`main.rs` still grows business logic — God-binary risk re-emerges".
Land a cheap check that fails CI if `src/main.rs` blows past a
budget. Today it's 901 lines; the budget is set at the SPEC's
recommended 400 once the obvious extractions land (later zones),
but this step ships the *check* with the current size as the
ceiling so we don't regress while the workspace shell is fresh.

**Files:**
- `scripts/check_main_rs_loc.sh` (new) — small shell script:
  `LOC=$(wc -l < src/main.rs); MAX=${1:-901}; if [ "$LOC" -gt
  "$MAX" ]; then echo "src/main.rs is $LOC lines (max $MAX)";
  exit 1; fi`. Executable bit set.
- `Makefile` (modified) — `lint:` target gains a
  `./scripts/check_main_rs_loc.sh 901` line. **The 901 ceiling is
  documented as a moving target**: per the SPEC §6 mitigation, when
  Zone 2 lands and the IPC scaffolding extracts cleanly, this drops
  to 700; final target 400 once `sy-aiplane`/`sy-knowledge` carve
  out (follow-on roadmap).

**Tests:**
- Manual: run `./scripts/check_main_rs_loc.sh 50` against the
  current tree, expect non-zero exit. Run with `901` (or whatever
  the current `wc -l` reports), expect zero. Documented in the
  PR description.
- `scripts/check_main_rs_loc.sh` self-test via a comment line: not
  a unit test (it's shell), but the Makefile target itself
  exercises the script on every `make lint`.

**Definition of Done:**
- [x] Script exists at `scripts/check_main_rs_loc.sh`, executable
      (`-rwxr-xr-x`), passes against current `main.rs` (901 lines
      under default ceiling 901). Behaviour matrix verified
      manually: `50` → exit 1 with actionable error; `901` → exit
      0; `2000` → exit 0; `foo` → exit 2 (usage error per CLIG).
      Script `cd`s to repo root via `$(dirname "$0")/..` so it
      works regardless of cwd.
- [~] `make lint` runs the script first (placed before the
      `cargo clippy` line so the LOC gate is exercised on every
      invocation regardless of the pre-existing clippy debt). LOC
      check passes silently. **The full `make lint` target stays
      red workspace-wide on the same 51 pre-existing clippy errors
      as Steps 1-4 — unchanged.** No new violations introduced.
      Once the cleanup pass lands, `make lint` will be green and
      the LOC ratchet will fail loudly on any main.rs regression.
- [x] Budget-drop schedule documented inline in the script's head
      comment and in this ROADMAP's Step 5 head paragraph: 901 now
      (Step 1 baseline) → 700 after Zone 2 IPC scaffolding lifts
      out → 400 after `sy-aiplane`/`sy-knowledge` carve out. Future
      zones bump the literal in the Makefile (`./scripts/check_main_rs_loc.sh
      <N>`) as part of their roadmap DoD.
- [x] No `#[allow(dead_code)]`, no `TODO`/`FIXME`.

**Risks / unknowns:**
- ~~Hard LOC budget can be gamed by line-wrapping.~~ — **accepted
  v1**: the goal is a behavioural signal, not perfect enforcement.
  The script intentionally counts `wc -l` so a contributor who
  splits one statement across three lines pays one line of budget
  per line on disk — they can do it, but the cost surfaces in
  review. Follow-up swap to `tokei` for SLOC is gated on a real
  case where this matters.

---

## Cross-cutting Definition of Done

- [x] All step DoDs satisfied (Steps 1-5 ticked; lint workspace-
      wide noted as `[~]` pending the separate cleanup pass on
      the 51 pre-existing clippy errors).
- [x] `cargo build --release` produces a working `sy` binary —
      `target/release/sy` 32 MB, same shape as pre-conversion.
      No behaviour change: workspace-conversion + types-extract is
      ABI-equivalent because the re-exports preserve every public
      path the consumers (`src/aiplane/**`, `src/knowledge/**`)
      depended on.
- [x] `cargo build -p sy-core` and `cargo build -p sy-testutils`
      succeed standalone (verified at the end of Steps 1 + 4).
- [x] `cargo tree -d` shows no new duplicate version chains —
      pre/post snapshot both report 52 duplicate root crates,
      all framework-side (calloop / smithay / iced layers,
      tokenizers' `spm_precompiled`-pinned `base64@0.13`).
- [~] `make test` green workspace-wide (71 sy + 22 sy-core + 2
      sy-testutils = 95, stable across flake-check runs).
      `make lint` runs the new LOC gate first (passes), then hits
      the pre-existing 51 clippy errors — same red state as before
      Step 1, zero net regression. Cleanup pass tracked outside
      this roadmap.
- [x] `src/main.rs` LOC ≤ 901 (currently 901, gated by Step 5's
      `scripts/check_main_rs_loc.sh 901` in the Makefile;
      ratcheted by later zones).
- [x] SPEC §3.2 K1 "thin `sy` binary + `sy-core` + `sy-testutils`"
      delivered at the workspace level. `sy-core` re-exports
      `WorkloadKind / WorkloadInput / WorkloadOutput / WorkloadHealth
      / WorkloadState / Priority / ErrorCode / SpeechSpan` for
      Zones 2-6 to consume. Other library crates (`sy-ipc`,
      `sy-aiplane`, etc.) are explicitly deferred per spec.

## Out of Scope

- Moving `src/aiplane/`, `src/knowledge/`, `src/stack/`, `src/agt/`
  into their own crates — covered by follow-on roadmaps under each
  subsystem name once `sy-core` is stable.
- Adding `sy-ipc` — handled by `arch-ipc-v1/ROADMAP.md`.
- Renaming or relocating any module currently under `src/`.
- Publishing any internal crate to crates.io — all stay
  `publish = false` per SPEC §3.4 anti-goal.
- Ripgrep-style public semver per crate — explicitly rejected in
  SPEC §3.2 K1 alternatives.
