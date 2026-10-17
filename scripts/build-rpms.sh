#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
native=false
if [[ ${1:-} == --native ]]; then
  native=true
  shift
fi
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)
if [[ -z "$version" ]]; then
  echo 'could not read workspace version' >&2
  exit 1
fi
spec_version=$(sed -n 's/^Version:[[:space:]]*//p' "$repo_root/packaging/sy.spec" | head -1)
if [[ "$spec_version" != "$version" ]]; then
  echo "RPM spec version $spec_version differs from Cargo version $version" >&2
  exit 1
fi
output_dir=${SY_RPM_OUTPUT_DIR:-"$repo_root/dist/rpm"}
mkdir -p "$output_dir"
output_dir=$(cd -- "$output_dir" && pwd)

if [[ $native == false ]]; then
  if command -v podman >/dev/null 2>&1; then
    container=podman
  elif command -v docker >/dev/null 2>&1; then
    container=docker
  else
    echo 'podman or docker is required' >&2
    exit 1
  fi
fi

releases=("$@")
if (( ${#releases[@]} == 0 )); then
  if [[ $native == true ]]; then releases=("$(rpm -E '%fedora')"); else releases=(43 44 45); fi
fi
for fedora in "${releases[@]}"; do
  case "$fedora" in
    43|44|45) ;;
    *) echo "unsupported Fedora release: $fedora (expected 43, 44 or 45)" >&2; exit 2 ;;
  esac
  if [[ $native == true && $fedora != "$(rpm -E '%fedora')" ]]; then
    echo "native build only supports the host Fedora release ($fedora requested)" >&2
    exit 2
  fi
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  mkdir -p "$work/SOURCES" "$work/RPMS" "$work/SRPMS"
  cp "$repo_root/packaging/sy.spec" "$work/sy.spec"
  tar -C "$repo_root" --transform="s,^,sy-$version/," \
    -czf "$work/SOURCES/sy-$version.tar.gz" \
    Cargo.toml Cargo.lock LICENSE README.md src crates configs themes scripts packaging/sy.1 packaging/default-sy.toml
  if [[ $native == true ]]; then
    rpmbuild -ba "$work/sy.spec" --define "_topdir $work" \
      --define "_rpmdir $work/RPMS" --define "_srcrpmdir $work/SRPMS"
  else
    cache_dir=${SY_RPM_DNF_CACHE_DIR:-"$output_dir/.dnf-cache/f$fedora"}
    cargo_cache_dir=${SY_RPM_CARGO_CACHE_DIR:-"$output_dir/.cargo-cache"}
    mkdir -p "$cache_dir" "$cargo_cache_dir"
    cache_dir=$(cd -- "$cache_dir" && pwd)
    cargo_cache_dir=$(cd -- "$cargo_cache_dir" && pwd)
    "$container" run --rm --network=host -v "$work:/build:Z" \
      -v "$cache_dir:/var/cache/libdnf5:Z" \
      -v "$cargo_cache_dir:/root/.cargo:Z" \
      -e SY_RPM_REPO_URL="${SY_RPM_REPO_URL:-}" \
      -e SY_RPM_FEDORA="$fedora" \
      -e CARGO_NET_OFFLINE="${CARGO_NET_OFFLINE:-false}" \
      "${SY_RPM_CONTAINER_IMAGE:-fedora:$fedora}" bash -euc '
      [[ $(rpm -E "%fedora") == "$SY_RPM_FEDORA" ]] || {
        echo "container Fedora release differs from requested build" >&2
        exit 1
      }
      repo_args=()
      if [[ -n $SY_RPM_REPO_URL ]]; then
        repo_args=(--disablerepo="*" --repofrompath="sy-fedora,$SY_RPM_REPO_URL")
      fi
      dnf --setopt=ip_resolve=4 --setopt=zchunk=False --setopt=install_weak_deps=False \
        --setopt=keepcache=True \
        --setopt=max_parallel_downloads=8 --setopt=max_downloads_per_mirror=8 \
        --setopt=timeout=120 --setopt=minrate=1 --setopt=retries=20 \
        "${repo_args[@]}" install -y rpm-build cargo rust gcc gcc-c++ cmake pkgconf-pkg-config \
        alsa-lib-devel openssl-devel systemd-devel libxkbcommon-devel
      rpmbuild -ba /build/sy.spec --define "_topdir /build" \
        --define "_rpmdir /build/RPMS" --define "_srcrpmdir /build/SRPMS"
    '
  fi
  mkdir -p "$output_dir/f$fedora"
  shopt -s nullglob
  previous=("$output_dir/f$fedora"/*.rpm)
  if (( ${#previous[@]} )); then
    mkdir -p "$output_dir/f$fedora/archive"
    archive=$(mktemp -d "$output_dir/f$fedora/archive/build-XXXXXXXX")
    mv -- "${previous[@]}" "$archive/"
  fi
  find "$work/RPMS" "$work/SRPMS" -type f -name '*.rpm' -exec cp -t "$output_dir/f$fedora" {} +
  rm -rf "$work"
  trap - EXIT
done
