#!/usr/bin/env bash
# post-edit-check.sh — PostToolUse hook fired after every Edit/Write.
#
# Scans the just-edited file for placeholder code and surfaces a
# warning back to Claude so it can self-correct on the next turn. This
# is a soft signal (always exits 0); the hard gate is stop-verify.sh.
#
# Whitelist for `.agents/`, `.claude/`, `specs/`, `AGENTS.md`, this
# script itself — placeholder-shaped strings there are part of the
# instructions, not code to be cleaned up.

set -uo pipefail

INPUT=$(cat)
FILE_PATH=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null)

# No file path or doesn't exist → silently no-op.
[ -z "${FILE_PATH:-}" ] && exit 0
[ ! -f "$FILE_PATH" ] && exit 0

# Skip non-code files. The placeholder vocabulary is a *code* concern;
# specs, skills, and docs are allowed to discuss what TODO/stub means.
case "$FILE_PATH" in
  */.agents/*|*/.claude/*|*/specs/*|*/AGENTS.md|*/CLAUDE.md|*/README.md|*/CHANGELOG.md)
    exit 0
    ;;
  *.md|*.txt|*.json|*.toml|*.yaml|*.yml|*.kdl|*.css|*.jsonc)
    # Hooks themselves and config files are not the target of this scan.
    exit 0
    ;;
esac

# Only scan code files (Rust, Python, shell, JS/TS).
case "$FILE_PATH" in
  *.rs|*.py|*.sh|*.bash|*.js|*.ts|*.tsx|*.jsx)
    ;;
  *)
    exit 0
    ;;
esac

# Placeholder vocabulary. Tuned to flag intent-to-defer language without
# false-positiving common English ("todo list" in a doc string is rare
# enough in code; if it bites, add it here).
PATTERNS='\b(TODO|FIXME|HACK|XXX)\b|todo!\(|unimplemented!\(|panic!\("not (yet )?implemented|\bstub\b|placeholder|for now[,.: ]|in (a |the )?real implementation|TEMPORARY\b|\bWIP\b'

HITS=$(grep -niE "$PATTERNS" "$FILE_PATH" 2>/dev/null || true)
[ -z "$HITS" ] && exit 0

# Surface as additionalContext so Claude sees the warning on its next
# turn. JSON shape mirrors the spec for PostToolUse hooks.
cat <<EOF
{
  "decision": "approve",
  "reason": "placeholder-shaped code detected in $FILE_PATH:\n$(echo "$HITS" | head -10 | sed 's/"/\\"/g')\n\nReplace placeholders with real implementations before stopping. See AGENTS.md non_negotiables."
}
EOF
exit 0
