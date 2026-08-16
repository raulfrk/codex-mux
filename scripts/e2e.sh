#!/usr/bin/env bash
set -euo pipefail

repository=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repository"

for tool in bwrap cargo gzip rustc script sha256sum tar tmux; do
  command -v "$tool" >/dev/null || {
    echo "required packaged E2E tool is unavailable: $tool" >&2
    exit 1
  }
done

toolchain=${CODEX_MUX_E2E_TOOLCHAIN:-1.85}
network_mode=${CODEX_MUX_E2E_NETWORK_MODE:-isolated}
version=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' Cargo.toml)
target=x86_64-unknown-linux-gnu

case "$network_mode" in
  isolated)
    network_args=(--unshare-net)
    ;;
  host)
    network_args=()
    echo "packaged E2E is using the host network namespace" >&2
    ;;
  *)
    echo "invalid CODEX_MUX_E2E_NETWORK_MODE: $network_mode" >&2
    exit 1
    ;;
esac

if [[ ${CODEX_MUX_E2E_SANDBOXED:-0} != 1 ]]; then
  cargo "+$toolchain" fetch --locked
  mkdir -p target
  sandbox=$(mktemp -d)
  trap 'rm -rf -- "$sandbox"' EXIT
  mkdir -p "$sandbox"/{dist,extract,home,tmp,tmux,xdg}
  host_home=$HOME
  cargo_home=${CARGO_HOME:-$host_home/.cargo}
  rustup_home=${RUSTUP_HOME:-$host_home/.rustup}
  bwrap \
    "${network_args[@]}" \
    --unshare-pid \
    --die-with-parent \
    --ro-bind / / \
    --dev-bind /dev /dev \
    --proc /proc \
    --bind "$repository/target" "$repository/target" \
    --bind "$sandbox" "$sandbox" \
    --chdir "$repository" \
    --setenv CODEX_MUX_E2E_SANDBOXED 1 \
    --setenv E2E_SANDBOX "$sandbox" \
    --setenv HOME "$sandbox/home" \
    --setenv XDG_CONFIG_HOME "$sandbox/xdg" \
    --setenv TMPDIR "$sandbox/tmp" \
    --setenv TMUX_TMPDIR "$sandbox/tmux" \
    --setenv PATH "$PATH" \
    --setenv CARGO_HOME "$cargo_home" \
    --setenv RUSTUP_HOME "$rustup_home" \
    --setenv CODEX_MUX_E2E_TOOLCHAIN "$toolchain" \
    --setenv CODEX_MUX_E2E_NETWORK_MODE "$network_mode" \
    /usr/bin/env bash scripts/e2e.sh
  exit
fi

: "${E2E_SANDBOX:?missing E2E_SANDBOX}"
export CARGO_INCREMENTAL=0
export SOURCE_DATE_EPOCH=0

cargo "+$toolchain" build --offline --locked --release --target "$target"
scripts/package-release.sh \
  "$version" \
  "$target" \
  "target/$target/release/codex-mux" \
  "$E2E_SANDBOX/dist"
scripts/write-checksums.sh "$E2E_SANDBOX/dist"
(
  cd "$E2E_SANDBOX/dist"
  sha256sum -c SHA256SUMS
)

archive="$E2E_SANDBOX/dist/codex-mux-${version}-${target}.tar.gz"
tar -xzf "$archive" -C "$E2E_SANDBOX/extract"
packaged="$E2E_SANDBOX/extract/codex-mux-${version}-${target}/codex-mux"
test -x "$packaged"
test "$("$packaged" --version)" = "codex-mux $version"
export CODEX_MUX_E2E_BINARY=$packaged

cargo "+$toolchain" test \
  --offline \
  --locked \
  --test packaged_runtime_e2e \
  --test packaged_theme_e2e \
  --test packaged_installer_e2e \
  -- \
  --test-threads=1

if find "$TMPDIR" -mindepth 1 -print -quit | grep -q .; then
  echo "packaged E2E left temporary state behind" >&2
  find "$TMPDIR" -mindepth 1 -maxdepth 2 -print >&2
  exit 1
fi
while IFS= read -r -d '' socket; do
  if tmux -S "$socket" display-message -p '#{pid}' >/dev/null 2>&1; then
    echo "packaged E2E left a live tmux server behind at $socket" >&2
    exit 1
  fi
  rm -f -- "$socket"
done < <(find "$TMUX_TMPDIR" -type s -print0)
find "$TMUX_TMPDIR" -depth -mindepth 1 -type d -empty -delete
if find "$TMUX_TMPDIR" -mindepth 1 -print -quit | grep -q .; then
  echo "packaged E2E left tmux temporary state behind" >&2
  find "$TMUX_TMPDIR" -maxdepth 3 -print >&2
  exit 1
fi

echo "packaged runtime E2E passed inside the read-only-root sandbox"
