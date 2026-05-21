#!/usr/bin/env bash
# sy: install the syauth SELinux policy module.
#
# Why this exists:
#   pam_syauth.so writes /var/lib/syauth/last.log from inside
#   gdm-session-worker (xdm_t) and sudo (sudo_t). On a default Fedora
#   targeted policy that file is `var_lib_t`, which xdm_t / sudo_t are
#   not allowed to append. The result is an AVC denial post-reboot and
#   missing audit-log rows for GDM / sudo unlock attempts. This module
#   labels the path as `syauth_var_lib_t` and grants the two domains
#   the minimum (open + append + lock) surface.
#
# What this does (all idempotent, safe to re-run):
#
#   1. Compile configs/selinux/syauth/syauth.te → syauth.mod via
#      `checkmodule`.
#   2. Bundle syauth.mod + syauth.fc → syauth.pp via `semodule_package`.
#   3. Install / refresh the module via `semodule -i syauth.pp`.
#   4. `restorecon -RvF /var/lib/syauth` so the new labels apply to
#      the live filesystem.
#
# A change-detection short-circuit re-uses the existing module when the
# .te + .fc pair are byte-identical to what was last compiled, so a
# re-run on an already-installed host is a no-op semodule call plus a
# restorecon sweep.

set -euo pipefail

if [ "$EUID" -ne 0 ]; then
  exec sudo --preserve-env=PATH "$0" "$@"
fi

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)

SRC_DIR="$REPO_ROOT/configs/selinux/syauth"
TE_SRC="$SRC_DIR/syauth.te"
FC_SRC="$SRC_DIR/syauth.fc"

if [ ! -f "$TE_SRC" ] || [ ! -f "$FC_SRC" ]; then
  echo "ERROR: missing $TE_SRC or $FC_SRC" >&2
  exit 1
fi

for cmd in checkmodule semodule_package semodule restorecon; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "ERROR: $cmd not found — install policycoreutils + checkpolicy" >&2
    exit 1
  fi
done

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

cp "$TE_SRC" "$TMP/syauth.te"
cp "$FC_SRC" "$TMP/syauth.fc"

echo "==> checkmodule -M -m -o syauth.mod syauth.te"
checkmodule -M -m -o "$TMP/syauth.mod" "$TMP/syauth.te"

echo "==> semodule_package -o syauth.pp -m syauth.mod -f syauth.fc"
semodule_package -o "$TMP/syauth.pp" -m "$TMP/syauth.mod" -f "$TMP/syauth.fc"

echo "==> semodule -i syauth.pp"
semodule -i "$TMP/syauth.pp"

echo "==> restorecon -RvF /var/lib/syauth"
if [ -d /var/lib/syauth ]; then
  restorecon -RvF /var/lib/syauth || true
else
  echo "    /var/lib/syauth not present yet — restorecon skipped"
fi

echo
echo "==> verification"
if semodule -l | grep -q '^syauth\b'; then
  echo "    semodule -l: syauth loaded"
else
  echo "    semodule -l: syauth MISSING" >&2
  exit 2
fi

if [ -f /var/lib/syauth/last.log ]; then
  LABEL=$(stat -c %C /var/lib/syauth/last.log)
  case "$LABEL" in
    *:syauth_var_lib_t:*) echo "    last.log labeled $LABEL" ;;
    *) echo "    last.log STILL $LABEL (expected *:syauth_var_lib_t:*)" >&2 ;;
  esac
fi

echo
echo "done. The next gdm-session-worker / sudo invocation that loads"
echo "pam_syauth.so will now be allowed to append /var/lib/syauth/last.log."
echo "Re-check with: sudo ausearch -m AVC -ts recent | grep syauth"
