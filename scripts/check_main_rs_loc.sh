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
# 1025 lines after these zones land. Pass a lower value as $1 when
# later zones extract logic out of main.rs and the budget should
# ratchet.
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
