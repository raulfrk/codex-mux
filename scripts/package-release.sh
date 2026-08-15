#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 VERSION TARGET BINARY OUTPUT_DIR" >&2
  exit 2
fi

version=$1
target=$2
binary=$3
output_dir=$4

if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]]; then
  echo "invalid release version: $version" >&2
  exit 2
fi
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) ;;
  *)
    echo "unsupported release target: $target" >&2
    exit 2
    ;;
esac
if [[ ! -x $binary ]]; then
  echo "release binary is missing or not executable: $binary" >&2
  exit 2
fi
for file in README.md LICENSE; do
  if [[ ! -f $file ]]; then
    echo "required release file is missing: $file" >&2
    exit 2
  fi
done

package="codex-mux-${version}-${target}"
archive="${package}.tar.gz"
mkdir -p "$output_dir"
staging=$(mktemp -d)
trap 'rm -rf -- "$staging"' EXIT
mkdir "$staging/$package"
install -m 0755 "$binary" "$staging/$package/codex-mux"
install -m 0644 README.md LICENSE "$staging/$package/"

LC_ALL=C tar \
  --sort=name \
  --mtime='@0' \
  --owner=0 \
  --group=0 \
  --numeric-owner \
  --format=ustar \
  -C "$staging" \
  -cf - "$package" \
  | gzip -n -9 > "$output_dir/$archive"

printf '%s\n' "$output_dir/$archive"
