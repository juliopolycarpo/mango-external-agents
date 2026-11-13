#!/usr/bin/env bash
# Exercise core's packaged unit tests under a valid prerelease version without changing the workspace.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=$PWD
mkdir -p target
scratch=$(mktemp -d "${TMPDIR:-/tmp}/mango-prerelease.XXXXXX")
target=$(mktemp -d "$repo/target/prerelease-tests.XXXXXX")
trap 'rm -rf -- "$scratch" "$target"' EXIT
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-$(rustup show active-toolchain | awk '{print $1}')}"
export CARGO_TARGET_DIR="$target"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
cargo package -p mango-external-agents --no-verify --locked --allow-dirty
archives=("$target"/package/mango-external-agents-*.crate)
if [ "${#archives[@]}" -ne 1 ] || [ ! -f "${archives[0]}" ]; then
  echo "core: expected one package archive, received ${archives[*]}" >&2
  exit 1
fi
tar -xzf "${archives[0]}" -C "$scratch"
core=("$scratch"/mango-external-agents-*)
python3 - "${core[0]}/Cargo.toml" <<'PY'
import re
import sys
import tomllib
from pathlib import Path
path = Path(sys.argv[1])
manifest = path.read_text()
# Cargo's normalized manifest uses one unquoted table header per line.
headers = list(re.finditer(r'(?m)^\[.*\]$', manifest))
package_headers = [i for i, header in enumerate(headers) if header.group() == '[package]']
assignments = []
if len(package_headers) == 1:
    index = package_headers[0]
    start = headers[index].end()
    end = headers[index + 1].start() if index + 1 < len(headers) else len(manifest)
    assignments = list(re.finditer(r'(?m)^version\s*=.*$', manifest[start:end]))
if len(assignments) != 1:
    raise SystemExit(f'{path}: expected one package version assignment, received {len(assignments)}')
version = tomllib.loads(manifest)["package"]["version"].split("-", 1)[0].split("+", 1)[0]
assignment = assignments[0]
manifest = manifest[:start + assignment.start()] + f'version = "{version}-rc.1"' + manifest[start + assignment.end():]
path.write_text(manifest)
PY
(cd "${core[0]}" && cargo update --workspace --offline && cargo nextest run --lib --all-features --locked)
size=$(du -sk "$target" | cut -f1)
if [ "$size" -gt 20971520 ]; then
  echo "prerelease package target: expected at most 20 GiB, received $size KiB" >&2
  exit 1
fi
