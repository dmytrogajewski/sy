#!/usr/bin/env bash
# Install sy's PolicyKit PAM configuration; sudo remains the auth source.
set -euo pipefail
auth_root=${SY_AUTH_ROOT:-/}
auth_dry_run=${SY_AUTH_DRY_RUN:-0}
while (($#)); do
  case "$1" in
    --root) auth_root=${2:?--root requires a path}; shift 2 ;;
    --dry-run) auth_dry_run=1; shift ;;
    --help) echo 'Usage: install-system-auth.sh [--root PATH] [--dry-run]'; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [[ $auth_dry_run == 1 ]]; then echo "Would install $auth_root/etc/pam.d/polkit-1 (auth include sudo)"; exit 0; fi
if [[ $auth_root == / && $EUID != 0 ]]; then
  exec sudo bash "$0" --root "$auth_root"
fi
auth_pam_dir=$auth_root/etc/pam.d
if [[ ! -f $auth_pam_dir/sudo ]] || ! grep -Eq '^[[:space:]]*auth[[:space:]]' "$auth_pam_dir/sudo"; then
  echo 'Cannot install: sudo PAM authentication stack is missing.' >&2
  exit 1
fi
auth_repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
auth_destination=$auth_pam_dir/polkit-1
auth_source=$auth_repo/configs/pam/polkit-1
if cmp -s "$auth_source" "$auth_destination"; then echo 'PolicyKit already uses sudo authentication.'; exit 0; fi
if [[ -e $auth_destination && ! -e $auth_destination.sy-backup ]]; then
  cp -p -- "$auth_destination" "$auth_destination.sy-backup"
fi
auth_temporary=$(mktemp "$auth_pam_dir/.sy-polkit-XXXXXX")
trap 'rm -f -- "$auth_temporary"' EXIT
install -m 0644 -- "$auth_source" "$auth_temporary"
mv -fT -- "$auth_temporary" "$auth_destination"
if [[ $auth_root == / ]] && command -v restorecon >/dev/null; then restorecon "$auth_destination"; fi
echo "Installed $auth_destination (auth include sudo)."
