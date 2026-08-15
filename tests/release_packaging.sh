#!/usr/bin/env bash
set -euo pipefail

repository=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repository"

scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
binary="$scratch/codex-mux"
printf '#!/bin/sh\nprintf "fixture\\n"\n' > "$binary"
chmod 0755 "$binary"

if [[ $(scripts/release-channel.sh 1.2.3) != stable ]]; then
  echo "stable release was misclassified" >&2
  exit 1
fi
for version in 1.2.3-rc.1 1.2.3-beta; do
  if [[ $(scripts/release-channel.sh "$version") != prerelease ]]; then
    echo "prerelease $version was misclassified" >&2
    exit 1
  fi
done
if scripts/release-channel.sh not-a-version >/dev/null 2>&1; then
  echo "invalid release version was accepted" >&2
  exit 1
fi

first="$scratch/first"
second="$scratch/second"
for output in "$first" "$second"; do
  scripts/package-release.sh 0.1.0 x86_64-unknown-linux-gnu "$binary" "$output" >/dev/null
  scripts/package-release.sh 0.1.0 aarch64-unknown-linux-gnu "$binary" "$output" >/dev/null
  scripts/write-checksums.sh "$output" >/dev/null
done

expected=$(cat <<'NAMES'
SHA256SUMS
codex-mux-0.1.0-aarch64-unknown-linux-gnu.tar.gz
codex-mux-0.1.0-x86_64-unknown-linux-gnu.tar.gz
NAMES
)
actual=$(find "$first" -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort)
if [[ $actual != "$expected" ]]; then
  echo "unexpected release assets:" >&2
  printf '%s\n' "$actual" >&2
  exit 1
fi

for asset in SHA256SUMS codex-mux-0.1.0-aarch64-unknown-linux-gnu.tar.gz codex-mux-0.1.0-x86_64-unknown-linux-gnu.tar.gz; do
  cmp "$first/$asset" "$second/$asset"
done
(
  cd "$first"
  sha256sum -c SHA256SUMS
)

printf 'corruption' >> "$first/codex-mux-0.1.0-aarch64-unknown-linux-gnu.tar.gz"
if (cd "$first" && sha256sum -c SHA256SUMS >/dev/null 2>&1); then
  echo "checksum verification accepted a corrupted archive" >&2
  exit 1
fi

echo "release packaging and corruption detection passed"
