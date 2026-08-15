#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 DIST_DIR" >&2
  exit 2
fi

dist_dir=$1
if [[ ! -d $dist_dir ]]; then
  echo "release directory does not exist: $dist_dir" >&2
  exit 2
fi

mapfile -d '' archives < <(
  find "$dist_dir" -maxdepth 1 -type f -name 'codex-mux-*.tar.gz' -print0 | LC_ALL=C sort -z
)
if [[ ${#archives[@]} -eq 0 ]]; then
  echo "no codex-mux release archives found in $dist_dir" >&2
  exit 2
fi

manifest="$dist_dir/SHA256SUMS"
: > "$manifest"
for archive in "${archives[@]}"; do
  (
    cd "$dist_dir"
    sha256sum "$(basename "$archive")"
  ) >> "$manifest"
done
printf '%s\n' "$manifest"
