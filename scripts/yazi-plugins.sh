#!/usr/bin/env bash
# yazi-plugins.sh — bootstrap yazi plugins listed by `configs/yazi/`.
#
# `sy apply` writes ~/.config/yazi/{package.toml,yazi.toml,keymap.toml,
# init.lua,theme.toml}, but the plugins themselves are installed by
# yazi's own `ya` package manager (for everything in package.toml) and
# by `git clone` (for the four plugins that aren't reachable through
# `ya pkg`). This script handles both, idempotently, so a fresh machine
# can reach the same state with one invocation.
#
# Safe to re-run: ya pkg install is itself idempotent; git clones are
# guarded by directory existence; the easyjump flatten only runs while
# the upstream nested layout is present.
#
# Usage:
#   scripts/yazi-plugins.sh          # install / refresh
#   YAZI_CONFIG_HOME=... scripts/yazi-plugins.sh
#
# Honours XDG_CONFIG_HOME (falls back to ~/.config).

set -euo pipefail

config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
plugins_dir="$config_home/yazi/plugins"

if ! command -v ya >/dev/null 2>&1; then
  echo "yazi-plugins: 'ya' not found on PATH — install yazi first" >&2
  exit 1
fi
if ! command -v git >/dev/null 2>&1; then
  echo "yazi-plugins: 'git' not found on PATH" >&2
  exit 1
fi

mkdir -p "$plugins_dir"

echo ">> ya pkg install (resolves package.toml)"
ya pkg install

# Plugins not reachable via `ya pkg add`: kept as shallow clones.
# Format: <repo-url>|<plugin-dir-name>
declare -a clones=(
  "https://github.com/dawsers/dual-pane.yazi.git|dual-pane.yazi"
  "https://github.com/mikavilpas/easyjump.yazi.git|easyjump.yazi"
  "https://github.com/DreamMaoMao/searchjump.yazi.git|searchjump.yazi"
  "https://gitlab.com/WhoSowSee/whoosh.yazi.git|whoosh.yazi"
)

for entry in "${clones[@]}"; do
  url="${entry%%|*}"
  name="${entry##*|}"
  dest="$plugins_dir/$name"

  # Plugins flattened out of a nested upstream layout (e.g. easyjump.yazi)
  # lose their .git dir. If a top-level main.lua is already in place we
  # treat the install as complete and skip the upstream pull.
  if [ -f "$dest/main.lua" ] && [ ! -d "$dest/.git" ]; then
    echo ">> skip $name (detached / already flattened)"
    continue
  fi

  if [ -d "$dest/.git" ]; then
    echo ">> git pull $name"
    git -C "$dest" pull --ff-only --quiet
  else
    echo ">> git clone $name"
    rm -rf "$dest"
    git clone --depth=1 --quiet "$url" "$dest"
  fi
done

# easyjump.yazi upstream ships its real plugin nested inside a workspace
# (./easyjump.yazi/main.lua). Flatten so yazi can load it from the top.
easyjump="$plugins_dir/easyjump.yazi"
if [ -d "$easyjump/easyjump.yazi" ] && [ -f "$easyjump/easyjump.yazi/main.lua" ] \
   && [ ! -f "$easyjump/main.lua" ]; then
  echo ">> flatten easyjump.yazi nested layout"
  tmp="$easyjump.tmp"
  rm -rf "$tmp"
  mv "$easyjump" "$tmp"
  mv "$tmp/easyjump.yazi" "$easyjump"
  rm -rf "$tmp"
fi

echo "yazi-plugins: done"
