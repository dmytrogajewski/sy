#!/usr/bin/env bash
# sy: install the system-level NPU pieces that user-scope `sy apply` cannot.
#
# What this does (all idempotent, safe to re-run):
#
#   1. /etc/dracut.conf.d/sy-amdxdna-defer.conf
#      — keeps amdxdna OUT of the initramfs, so request_firmware()
#        cannot ENOENT on /lib/firmware/amdnpu/<pid>/npu.dev.sbin
#        before pivot_root.
#
#   2. /etc/systemd/system/sy-amdxdna-load.service
#      — explicit post-pivot modprobe, waits for /dev/accel/accel0,
#        ordering anchor for downstream NPU services.
#
#   3. /etc/systemd/system/sy-npu-perf.service
#      — existing xrt-smi pmode=turbo primer, refreshed in case the
#        repo version drifted from /etc.
#
#   4. dracut --force for the running kernel (so the new omit_drivers
#      actually takes effect on the next boot).
#
#   5. systemctl daemon-reload + enable --now for both units.
#
#   6. If amdxdna is currently loaded but /dev/accel/accel0 is missing
#      (the failure mode that prompted this fix), rmmod + modprobe so
#      the NPU comes up THIS session without a reboot.
#
# Re-running on an already-installed host is a no-op except for the
# initramfs regen and a daemon-reload.

set -euo pipefail

if [ "$EUID" -ne 0 ]; then
  exec sudo --preserve-env=PATH "$0" "$@"
fi

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)

DRACUT_SRC="$REPO_ROOT/configs/dracut/sy-amdxdna-defer.conf"
LOAD_SRC="$REPO_ROOT/configs/systemd/system/sy-amdxdna-load.service"
PERF_SRC="$REPO_ROOT/configs/systemd/system/sy-npu-perf.service"

DRACUT_DST="/etc/dracut.conf.d/sy-amdxdna-defer.conf"
LOAD_DST="/etc/systemd/system/sy-amdxdna-load.service"
PERF_DST="/etc/systemd/system/sy-npu-perf.service"

# install_if_changed src dst — diff-aware copy; returns 0 always.
# Prints a single "==> updated <dst>" line iff content changed.
install_if_changed() {
  local src="$1" dst="$2"
  if [ -f "$dst" ] && cmp -s "$src" "$dst"; then
    echo "    unchanged $dst"
    return 0
  fi
  install -D -m 644 -o root -g root "$src" "$dst"
  echo "==> updated $dst"
  CHANGED=1
}

CHANGED=0

echo "==> installing system files"
install_if_changed "$DRACUT_SRC" "$DRACUT_DST"
install_if_changed "$LOAD_SRC"   "$LOAD_DST"
install_if_changed "$PERF_SRC"   "$PERF_DST"

KVER=$(uname -r)
INITRAMFS="/boot/initramfs-${KVER}.img"

if [ "$CHANGED" -eq 1 ] || [ "${SY_NPU_FORCE_DRACUT:-0}" = "1" ]; then
  if [ -f "$INITRAMFS" ]; then
    echo "==> regenerating $INITRAMFS"
    dracut --force "$INITRAMFS" "$KVER"
  else
    echo "    WARN: $INITRAMFS not found, skipping dracut regen" >&2
  fi
else
  echo "    initramfs regen skipped (no config drift; set SY_NPU_FORCE_DRACUT=1 to force)"
fi

echo "==> systemctl daemon-reload"
systemctl daemon-reload

echo "==> enable + start sy-amdxdna-load.service, sy-npu-perf.service"
systemctl enable --now sy-amdxdna-load.service sy-npu-perf.service || true

# Live recovery: if amdxdna is sitting around with no device node, kick
# it. This is the difference between "fixed on next reboot" and "fixed
# right now" — the latter matters when /lib/firmware just became
# reachable post-pivot but the driver had already failed its initrd probe.
if [ -d /sys/module/amdxdna ] && [ ! -e /dev/accel/accel0 ]; then
  echo "==> amdxdna loaded but no /dev/accel/accel0 — reloading driver"
  modprobe -r amdxdna || true
  modprobe amdxdna
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -e /dev/accel/accel0 ] && break
    sleep 0.5
  done
fi

echo
echo "==> verification"
if [ -e /dev/accel/accel0 ]; then
  echo "    /dev/accel/accel0 present"
else
  echo "    /dev/accel/accel0 MISSING — check journalctl -k -b | grep amdxdna" >&2
fi
if command -v xrt-smi >/dev/null 2>&1; then
  xrt-smi examine 2>/dev/null | awk '/Device.*Present|devices found/ { print "    " $0 }' || true
fi

echo
echo "done. After the next reboot, sy-amdxdna-load.service replays this load"
echo "deterministically, and the dracut omit_drivers config keeps amdxdna out"
echo "of the initramfs so the request_firmware race cannot recur."
