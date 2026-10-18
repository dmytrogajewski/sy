#!/usr/bin/env bash
# Install a built sy authentication dialog for this graphical session.
set -euo pipefail
agent_repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
agent_home=${SY_POLKIT_HOME:-$HOME}
agent_binary=${SY_POLKIT_BINARY:-$agent_repo/target/release/sy-desktop-shell}
agent_dry_run=${SY_POLKIT_DRY_RUN:-0}
while (($#)); do
  case "$1" in
    --binary) agent_binary=${2:?--binary requires a path}; shift 2 ;;
    --dry-run) agent_dry_run=1; shift ;;
    --help) echo 'Usage: install-polkit-agent.sh [--binary PATH] [--dry-run]'; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [[ $agent_dry_run == 1 ]]; then
  echo "Would install $agent_binary and sy-polkit-agent.service under $agent_home"
  exit 0
fi
[[ -x $agent_binary ]] || { echo "Build sy-desktop-shell first: $agent_binary" >&2; exit 1; }
agent_destination=$agent_home/.local/libexec/sy/sy-desktop-shell
mkdir -p "$(dirname -- "$agent_destination")" "$agent_home/.config/systemd/user"
agent_temporary=$(mktemp "$(dirname -- "$agent_destination")/.polkit-XXXXXX")
trap 'rm -f -- "$agent_temporary"' EXIT
install -m 0755 -- "$agent_binary" "$agent_temporary"
mv -fT -- "$agent_temporary" "$agent_destination"
install -m 0644 "$agent_repo/configs/systemd/user/sy-polkit-agent.service" "$agent_home/.config/systemd/user/"
agent_niri=$agent_home/.config/niri/config.kdl
if [[ -f $agent_niri ]] && grep -q 'spawn-at-startup "lxpolkit"' "$agent_niri"; then
  [[ -e $agent_niri.sy-polkit-backup ]] || cp -p "$agent_niri" "$agent_niri.sy-polkit-backup"
  sed -i 's/spawn-at-startup "lxpolkit"/spawn-at-startup "systemctl" "--user" "start" "sy-polkit-agent.service"/' "$agent_niri"
fi
echo 'Installed. Run systemctl --user daemon-reload, then enable --now sy-polkit-agent.service after the previous agent exits.'
