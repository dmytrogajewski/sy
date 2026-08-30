#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
binary=${1:?usage: package-spark-release.sh ARM64_BINARY OUTPUT_DIR}
output=${2:?usage: package-spark-release.sh ARM64_BINARY OUTPUT_DIR}
install -d "$output/configs/sy/spark/engines"
install -m 0555 "$binary" "$output/sy-aarch64"
install -m 0644 "$repo/configs/sy/spark/models.toml" "$output/configs/sy/spark/models.toml"
shopt -s nullglob
engines=("$repo/configs/sy/spark/engines/"*.toml)
if ((${#engines[@]} == 0)); then
    echo "engine inventory is empty" >&2
    exit 1
fi
for engine in "${engines[@]}"; do
    install -m 0644 "$engine" "$output/configs/sy/spark/engines/$(basename "$engine")"
done
(
    cd "$output"
    release_engines=(configs/sy/spark/engines/*.toml)
    sha256sum sy-aarch64 configs/sy/spark/models.toml "${release_engines[@]}" > SHA256SUMS
)
