#!/usr/bin/env bash
# stop-verify.sh — Stop hook fired when Claude tries to end the turn.
#
# Three gates, any of which blocks the stop:
#   1. Placeholder scan: TODO/FIXME/unimplemented!()/etc. across src/
#      (excluding tests, .agents/, .claude/, specs/).
#   2. Dead-code suppression: #[allow(dead_code)] / #[allow(unused)]
#      outside #[cfg(test)].
#   3. Build sanity: `cargo build --release --quiet` must succeed.
#      (Full `cargo test` is too slow for a Stop hook — left to the
#      developer / CI. Build catches obvious breakage.)
#
# Uses stop_hook_active to prevent infinite loops if Claude tries to
# retry the same stop without addressing the violations.

set -uo pipefail

INPUT=$(cat)
STOP_HOOK_ACTIVE=$(echo "$INPUT" | jq -r '.stop_hook_active // false' 2>/dev/null)
if [ "$STOP_HOOK_ACTIVE" = "true" ]; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

# Only run the gate if we look like a Rust crate.
[ -f Cargo.toml ] || exit 0
[ -d src ] || exit 0

ERRORS=""

# --- Gate 1: placeholder scan -----------------------------------------------
PATTERNS='\b(TODO|FIXME|HACK|XXX)\b|todo!\(|unimplemented!\(|placeholder|for now[,.: ]'
PLACEHOLDER_HITS=$(grep -rnE "$PATTERNS" src/ 2>/dev/null \
  | grep -v '#\[cfg(test)\]' \
  | grep -v '/tests/' \
  | head -10 || true)
if [ -n "$PLACEHOLDER_HITS" ]; then
  ERRORS="${ERRORS}PLACEHOLDER CODE DETECTED (replace with real implementation before stopping):\n${PLACEHOLDER_HITS}\n\n"
fi

# --- Gate 2: dead-code suppression ------------------------------------------
DEAD_HITS=$(grep -rnE '#\[allow\(dead_code\)\]|#!\[allow\(dead_code\)\]|#\[allow\(unused\)\]|#!\[allow\(unused\)\]' src/ 2>/dev/null \
  | grep -v '#\[cfg(test)\]' \
  | head -10 || true)
if [ -n "$DEAD_HITS" ]; then
  ERRORS="${ERRORS}DEAD CODE SUPPRESSION DETECTED (delete the code or move it under #[cfg(test)] instead of #[allow(dead_code)]):\n${DEAD_HITS}\n\n"
fi

# --- Gate 3: build sanity ---------------------------------------------------
# Run only if we haven't already accumulated errors — saves ~10s when the
# user is already going to need another turn anyway.
if [ -z "$ERRORS" ]; then
  BUILD_OUT=$(cargo build --release --quiet 2>&1) || {
    BUILD_TAIL=$(echo "$BUILD_OUT" | tail -20)
    ERRORS="${ERRORS}BUILD FAILED (\`cargo build --release\`):\n${BUILD_TAIL}\n\n"
  }
fi

# --- Final decision ---------------------------------------------------------
if [ -n "$ERRORS" ]; then
  REASON=$(echo -e "$ERRORS" | tr '\n' ' ' | tr '"' "'" | head -c 4000)
  printf '{"decision": "block", "reason": "%s"}\n' "$REASON"
  exit 0
fi

exit 0
