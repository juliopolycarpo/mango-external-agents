#!/usr/bin/env bash
# Downloads one immutable, vendor-published CLI release into a private bin directory.
#
# The pin lane cannot use each vendor's normal installer: those installers intentionally follow the
# latest release. This helper instead downloads an exact release asset from the vendor's official
# GitHub release. It never signs in to the CLI and it does not put the binary on a user's PATH.
#
# Usage: scripts/install-vendor-cli.sh <claude|codex|opencode> <version|latest> <bin-dir>
set -euo pipefail

usage() {
  echo "usage: scripts/install-vendor-cli.sh <claude|codex|opencode> <version|latest> <bin-dir>" >&2
}

vendor_repo() {
  case "$1" in
    claude) printf '%s\n' 'anthropics/claude-code' ;;
    codex) printf '%s\n' 'openai/codex' ;;
    opencode) printf '%s\n' 'anomalyco/opencode' ;;
    *)
      printf 'expected vendor claude, codex or opencode, received %s\n' "$1" >&2
      return 2
      ;;
  esac
}

release_tag() {
  local vendor="$1"
  local version="$2"
  local repo
  repo=$(vendor_repo "$vendor")

  if [ "$version" = 'latest' ]; then
    gh release view --repo "$repo" --json tagName --jq '.tagName'
    return
  fi

  case "$vendor" in
    codex) printf 'rust-v%s\n' "$version" ;;
    claude | opencode) printf 'v%s\n' "$version" ;;
  esac
}

platform() {
  local system
  local architecture
  system=$(uname -s)
  architecture=$(uname -m)
  case "$system/$architecture" in
    Linux/x86_64) printf '%s\n' 'linux-x64' ;;
    Linux/aarch64 | Linux/arm64) printf '%s\n' 'linux-arm64' ;;
    Darwin/x86_64) printf '%s\n' 'darwin-x64' ;;
    Darwin/arm64 | Darwin/aarch64) printf '%s\n' 'darwin-arm64' ;;
    MINGW*/x86_64 | MSYS*/x86_64 | CYGWIN*/x86_64) printf '%s\n' 'windows-x64' ;;
    MINGW*/aarch64 | MINGW*/arm64 | MSYS*/aarch64 | MSYS*/arm64 | CYGWIN*/aarch64 | CYGWIN*/arm64)
      printf '%s\n' 'windows-arm64'
      ;;
    *)
      printf 'expected a supported runner platform, received system %s and architecture %s\n' \
        "$system" "$architecture" >&2
      return 2
      ;;
  esac
}

asset_name() {
  local vendor="$1"
  local target="$2"
  case "$vendor/$target" in
    claude/linux-x64) printf '%s\n' 'claude-linux-x64.tar.gz' ;;
    claude/linux-arm64) printf '%s\n' 'claude-linux-arm64.tar.gz' ;;
    claude/darwin-x64) printf '%s\n' 'claude-darwin-x64.tar.gz' ;;
    claude/darwin-arm64) printf '%s\n' 'claude-darwin-arm64.tar.gz' ;;
    claude/windows-x64) printf '%s\n' 'claude-win32-x64.zip' ;;
    claude/windows-arm64) printf '%s\n' 'claude-win32-arm64.zip' ;;
    codex/linux-x64) printf '%s\n' 'codex-x86_64-unknown-linux-musl.tar.gz' ;;
    codex/linux-arm64) printf '%s\n' 'codex-aarch64-unknown-linux-musl.tar.gz' ;;
    codex/darwin-x64) printf '%s\n' 'codex-x86_64-apple-darwin.tar.gz' ;;
    codex/darwin-arm64) printf '%s\n' 'codex-aarch64-apple-darwin.tar.gz' ;;
    codex/windows-x64) printf '%s\n' 'codex-x86_64-pc-windows-msvc.exe.zip' ;;
    codex/windows-arm64) printf '%s\n' 'codex-aarch64-pc-windows-msvc.exe.zip' ;;
    opencode/linux-x64) printf '%s\n' 'opencode-linux-x64.tar.gz' ;;
    opencode/linux-arm64) printf '%s\n' 'opencode-linux-arm64.tar.gz' ;;
    opencode/darwin-x64) printf '%s\n' 'opencode-darwin-x64.zip' ;;
    opencode/darwin-arm64) printf '%s\n' 'opencode-darwin-arm64.zip' ;;
    opencode/windows-x64) printf '%s\n' 'opencode-windows-x64.zip' ;;
    opencode/windows-arm64) printf '%s\n' 'opencode-windows-arm64.zip' ;;
    *)
      printf 'expected a release asset for %s on %s, received none\n' "$vendor" "$target" >&2
      return 2
      ;;
  esac
}

binary_name() {
  case "$1/$2" in
    codex/windows-* | claude/windows-* | opencode/windows-*) printf '%s.exe\n' "$1" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

released_binary_name() {
  local vendor="$1"
  local target="$2"
  local asset="$3"
  local name
  case "$vendor" in
    codex)
      name="${asset%.tar.gz}"
      printf '%s\n' "${name%.zip}"
      ;;
    *) binary_name "$vendor" "$target" ;;
  esac
}

unpack_asset() {
  local archive="$1"
  local directory="$2"
  case "$archive" in
    *.tar.gz) tar -xzf "$archive" -C "$directory" ;;
    *.zip) unzip -q "$archive" -d "$directory" ;;
    *)
      printf 'expected a .tar.gz or .zip release asset, received %s\n' "$archive" >&2
      return 2
      ;;
  esac
}

download() {
  local vendor="$1"
  local version="$2"
  local destination="$3"
  local repo
  local tag
  local target
  local asset
  local binary
  local released_binary
  local temporary
  local extracted

  repo=$(vendor_repo "$vendor")
  tag=$(release_tag "$vendor" "$version")
  target=$(platform)
  asset=$(asset_name "$vendor" "$target")
  binary=$(binary_name "$vendor" "$target")
  released_binary=$(released_binary_name "$vendor" "$target" "$asset")
  temporary=$(mktemp -d "${TMPDIR:-/tmp}/mea-vendor.XXXXXX")
  trap "rm -rf -- $(printf '%q' "$temporary")" RETURN

  gh release download "$tag" --repo "$repo" --pattern "$asset" --dir "$temporary" --clobber
  unpack_asset "$temporary/$asset" "$temporary"
  extracted="$temporary/$released_binary"
  if [ ! -f "$extracted" ]; then
    printf 'expected %s in vendor release asset %s, received none\n' "$binary" "$asset" >&2
    return 1
  fi

  mkdir -p "$destination"
  cp "$extracted" "$destination/$binary"
  chmod +x "$destination/$binary"
  trap - RETURN
  rm -rf -- "$temporary"
  echo "installed $vendor $tag at $destination/$binary"
}

main() {
  if [ $# -ne 3 ]; then
    usage
    return 2
  fi
  download "$1" "$2" "$3"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
