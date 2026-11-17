#!/usr/bin/env bash
# Downloads the pinned actionlint release into a private bin directory.
#
# taiki-e/install-action has no manifest for actionlint, so the Policy job cannot install it the way
# it installs shellcheck. This helper takes the exact release asset from rhysd/actionlint's GitHub
# release and refuses to extract it unless its SHA-256 matches scripts/actionlint-pinned-sha256sums.txt.
# Bumping the version is a change to ACTIONLINT_VERSION and to that registry together.
#
# When GITHUB_PATH is set (a GitHub Actions step), the bin directory is appended to it so later
# steps find the binary, as install-action does. It never touches the user's PATH otherwise.
# Usage: scripts/install-actionlint.sh <bin-dir>
set -euo pipefail

ACTIONLINT_VERSION=1.7.12
release_base=https://github.com/rhysd/actionlint/releases/download

usage() { echo "usage: scripts/install-actionlint.sh <bin-dir>" >&2; }

# The release asset for this machine: actionlint_<version>_<os>_<arch>.tar.gz.
asset_name() {
  local system architecture os arch
  system=$(uname -s)
  architecture=$(uname -m)
  case "$system" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    *) os='' ;;
  esac
  case "$architecture" in
    x86_64 | amd64) arch=amd64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *) arch='' ;;
  esac
  if [ -z "$os" ] || [ -z "$arch" ]; then
    printf 'expected Linux or macOS on x86_64 or arm64, received system %s and architecture %s\n' \
      "$system" "$architecture" >&2
    return 2
  fi
  printf 'actionlint_%s_%s_%s.tar.gz\n' "$ACTIONLINT_VERSION" "$os" "$arch"
}

# The pinned digest for an asset, read from the registry next to this script.
pinned_digest() {
  local asset=$1 registry digest
  registry="$(dirname "${BASH_SOURCE[0]}")/actionlint-pinned-sha256sums.txt"
  if [ ! -f "$registry" ]; then
    printf 'expected the pinned checksum registry %s, received none\n' "$registry" >&2
    return 2
  fi
  digest=$(awk -v version="$ACTIONLINT_VERSION" -v asset="$asset" \
    '$1 == version && $2 == asset { print $3; exit }' "$registry")
  if [ -z "$digest" ]; then
    printf 'expected a pinned SHA-256 for actionlint %s asset %s in %s, received none\n' \
      "$ACTIONLINT_VERSION" "$asset" "$registry" >&2
    return 2
  fi
  printf '%s\n' "$digest"
}

archive_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    printf 'expected sha256sum or shasum to verify %s, received neither on PATH\n' "$1" >&2
    return 2
  fi
}

verify_sha256() {
  local expected=$1 archive=$2 actual
  actual=$(archive_sha256 "$archive")
  if [ "$actual" != "$expected" ]; then
    printf 'expected SHA-256 %s for %s, received %s\n' "$expected" "$archive" "$actual" >&2
    return 1
  fi
}

install_actionlint() {
  local destination=$1 asset expected
  asset=$(asset_name)
  expected=$(pinned_digest "$asset")
  # Global on purpose: the EXIT trap runs after this function's locals are gone.
  temporary=$(mktemp -d "${TMPDIR:-/tmp}/actionlint-install.XXXXXX")
  trap 'rm -rf -- "$temporary"' EXIT

  curl --fail --silent --show-error --location --retry 3 \
    --output "$temporary/$asset" "$release_base/v$ACTIONLINT_VERSION/$asset"
  verify_sha256 "$expected" "$temporary/$asset"
  tar -xzf "$temporary/$asset" -C "$temporary" actionlint
  if [ ! -f "$temporary/actionlint" ]; then
    printf 'expected an actionlint binary in %s, received none\n' "$asset" >&2
    return 1
  fi

  mkdir -p "$destination"
  cp "$temporary/actionlint" "$destination/actionlint"
  chmod +x "$destination/actionlint"
  echo "installed actionlint $ACTIONLINT_VERSION at $destination/actionlint"
  if [ -n "${GITHUB_PATH:-}" ]; then
    echo "$destination" >> "$GITHUB_PATH"
  fi
}

main() {
  if [ $# -ne 1 ]; then
    usage
    return 2
  fi
  install_actionlint "$1"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
