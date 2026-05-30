#!/usr/bin/env bash
# Guards `src/main.rs` against the god-binary anti-pattern flagged in
# `specs/research/architecture-refactor/SPEC.md` §6 Risks. Run as
# part of `make lint`. Default ceiling 1000 (901 after Step 1 of
# arch-workspace + ~4 lines for `sy_core::obs::init` in
# arch-observability Step 1 + 1 line for the `with_trace_id` seed
# in arch-observability Step 4 + 18 lines for the `sy doctor` clap
# variant + dispatch arm + module declaration in arch-observability
# Step 5 + 8 lines for the `sy crash` clap variant + dispatch arm +
# module declaration in arch-observability Step 6 + 6 lines for the
# `sy policy` clap variant + dispatch arm in arch-agent-sandbox
# Step 2 + 25 lines for the `sy approve` top-level clap variant
# (docstring + four args + dispatch arm) in arch-agent-sandbox
# Step 6 — logic lives in `src/agt/policy/cli.rs` + 35 lines for the
# arch-supervision Step 2 `sy apply` expansion (three new clap flags
# `--diff`/`--json`/`--yes`, `mod supervision`, and the thin
# `apply_units` dispatch helper — heavy lifting lives in
# `src/supervision/apply.rs::run_cli`) + 18 lines for the
# arch-supervision Step 3 `sy service` top-level clap variant
# (docstring + four-line `#[command(subcommand)]` block) + dispatch
# arm + `ServiceError` exit-code mapping (heavy lifting lives in
# `src/supervision/{service,status,logs}.rs`); the running total is
# 1025 lines after these zones land + 15 lines for the
# syauth-integration Step 3 `Cmd::Syauth` flag expansion (the
# `--service` / `--control` / `--yes` clap fields with their
# docstrings; dispatch is a single-line shim into
# `src/syauth.rs::run_cli`) — running total 1040 + 10 lines for the
# sy-power Step 1 `Cmd::Power` clap variant (docstring + nested
# `#[command(subcommand)]` block) + `mod power;` declaration +
# dispatch arm; heavy lifting lives in `src/power/cli.rs::dispatch`
# + 10 lines for `cargo fmt`-driven re-flow of the pre-existing
# `Cmd::Syauth` match arm into the multi-line struct destructure
# rustfmt requires once the file is touched. Running total: 1060.
# Pre-flight for the sy-mon roadmap extracted `list_themes` into
# `src/themes.rs` (-23 body lines + 1 `mod themes;` declaration +
# the `list_themes` call site rename — net -22). Running total:
# 1038, with 22 lines of slack reserved for the sy-mon roadmap's
# `Cmd::Mon` clap variant (docstring + nested `#[command(
# subcommand)]` block) + `mod mon;` declaration + dispatch arm
# that will land in sy-mon Step 11. Plus 6 lines for the waybar
# auto-reload follow-up: `mod waybar;` declaration + one-line
# `waybar_touched |= rel.starts_with("waybar")` flag inside the
# render loop + a four-line `if waybar_touched && !dry {
# waybar::reload(); }` trailer so `sy apply` is the single deploy
# verb per CLAUDE.md "no snowflakes" (SIGUSR2 plumbing itself
# lives in `src/waybar.rs`). Running total: 1066. Plus 5 lines for
# the yazi bootstrap follow-up landed `mod yazi_install;` + a
# `println!("yazi:")` header + the `yazi_install::ensure_yazi(root,
# dry)?;` call inside `apply`. sy-file-manager Step 36 retired that
# bootstrap (the productivised yazi rice is gone now that `sy file`
# is the canonical path) and the five lines were subtracted again.
# Net for this zone: 0. Running total: 1066. Plus
# 10 lines for the sy-file-manager roadmap Step 1 `mod plugin;`
# declaration (gated `#[cfg(test)]` until the Step 2+ non-test
# consumers in the bin land; the seven-line comment documents the
# gate so Step 2 can drop it without surprise); heavy lifting lives
# in `src/plugin/manifest.rs`. Running total: 1076. Plus 8 lines for
# the sy-file-manager roadmap Step 8 `Cmd::Plugin` clap variant
# (six-line docstring + nested `#[command(subcommand)]` block) +
# dispatch arm; the gate flips off from `#[cfg(test)] mod plugin;`
# to plain `mod plugin;` (net +1 — the `#[cfg(test)]` attribute went
# away but the rewritten comment grew by one line). Heavy lifting
# lives in `src/plugin/cli.rs`. Running total: 1084. Plus 13 lines
# for the sy-file-manager roadmap Step 13 `mod file;` declaration +
# `Cmd::File { path, cmd }` clap variant (six-line docstring + two
# positional/`#[command(subcommand)]` fields) + dispatch arm; heavy
# lifting lives in `src/file/cli.rs::dispatch`. Running total: 1097.
# Pass a lower value as $1 when later zones extract logic out of
# main.rs and the budget should ratchet.
#
# Exit: 0 on under-budget, 1 on over-budget, 2 on usage error.
set -euo pipefail

cd "$(dirname "$0")/.."

MAX="${1:-1000}"

if ! [[ "$MAX" =~ ^[0-9]+$ ]]; then
    echo "usage: $0 [<max-lines>]; got $MAX" >&2
    exit 2
fi

LOC=$(wc -l < src/main.rs)

if [ "$LOC" -gt "$MAX" ]; then
    echo "src/main.rs is $LOC lines (max $MAX); extract to a module under src/" >&2
    exit 1
fi
