#!/usr/bin/env bash
set -euo pipefail

dry_run=false
install_all=false
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=true ;;
    --all) install_all=true ;;
    *) echo 'usage: reinstall-rpms.sh [--dry-run] [--all]' >&2; exit 2 ;;
  esac
done

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
fedora=$(rpm -E '%fedora')
rpm_dir=${SY_RPM_OUTPUT_DIR:-"$repo_root/dist/rpm"}/f$fedora
shopt -s nullglob
packages=("$rpm_dir"/*.rpm)
binary_packages=()
previously_installed=()
have_base=false
need_shell=$install_all
if rpm -q sy-desktop >/dev/null 2>&1; then need_shell=true; fi
selected_desktop=false
for package in "${packages[@]}"; do
  [[ $package == *.src.rpm ]] && continue
  name=$(rpm -qp --qf '%{NAME}' "$package")
  case "$name" in sy|sy-*) ;; *) continue ;; esac
  [[ $name == sy ]] && have_base=true
  installed=false
  if rpm -q "$name" >/dev/null 2>&1; then installed=true; fi
  select=false
  if [[ $name == sy || $installed == true || $install_all == true ]]; then select=true; fi
  if [[ $name == sy-desktop-shell && $need_shell == true ]]; then select=true; fi
  if [[ $select == true ]]; then
    binary_packages+=("$package")
    if [[ $installed == true ]]; then previously_installed+=("$package"); fi
    if [[ $name == sy-desktop ]]; then selected_desktop=true; fi
  fi
done
if [[ $have_base != true ]]; then
  echo "expected sy RPM in $rpm_dir; run make rpm FEDORA=$fedora" >&2
  exit 1
fi
if [[ $dry_run == true ]]; then
  printf 'sudo dnf install -y'
  printf ' %q' "${binary_packages[@]}"
  printf '\n'
  if (( ${#previously_installed[@]} )); then
    printf 'sudo dnf reinstall -y'
    printf ' %q' "${previously_installed[@]}"
    printf '\n'
  fi
  if [[ $selected_desktop == true ]]; then printf '/usr/bin/sy --root /usr/share/sy apply\n'; fi
  if [[ $selected_desktop == false ]]; then printf 'systemctl --user daemon-reload\n'; fi
  exit 0
fi
sudo dnf install -y "${binary_packages[@]}"
if (( ${#previously_installed[@]} )); then
  sudo dnf reinstall -y "${previously_installed[@]}"
fi
if [[ $selected_desktop == true ]]; then /usr/bin/sy --root /usr/share/sy apply; fi
if [[ $selected_desktop == false ]] && systemctl --user show-environment >/dev/null 2>&1; then
  systemctl --user daemon-reload
fi
