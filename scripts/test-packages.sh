#!/usr/bin/env bash
# Run the tests shipped in each package outside the repository workspace.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=$PWD
read -r -a crates <<< "$(sed -n 's/^  CRATES: //p' .github/workflows/release.yml)"
if [ "${#crates[@]}" -eq 0 ]; then
  echo 'expected a CRATES list in the release workflow, received none' >&2
  exit 1
fi
mkdir -p target
scratch=$(mktemp -d "${TMPDIR:-/tmp}/mango-packages.XXXXXX")
target=$(mktemp -d "$repo/target/package-tests.XXXXXX")
trap 'rm -rf -- "$scratch" "$target"' EXIT
mkdir "$scratch/extracted"
export CARGO_TARGET_DIR="$target"
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-$(rustup show active-toolchain | awk '{print $1}')}"
cargo package --workspace --no-verify --locked --allow-dirty
for crate in "${crates[@]}"; do
  archives=("$target/package/$crate"-*.crate)
  if [ "${#archives[@]}" -ne 1 ] || [ ! -f "${archives[0]}" ]; then
    echo "$crate: expected one package archive, received ${archives[*]}" >&2
    exit 1
  fi
  tar -xzf "${archives[0]}" -C "$scratch/extracted"
done
core=("$scratch/extracted"/mango-external-agents-*)
python3 - "${core[0]}" "$scratch/patch.toml" <<'PY'
import json
import sys
from pathlib import Path
Path(sys.argv[2]).write_text('[patch.crates-io]\nmango-external-agents = { path = ' + json.dumps(sys.argv[1]) + ' }\n')
PY
if [ -e "$scratch/fixtures" ]; then
  echo "expected no fixture fallback at $scratch/fixtures, received a path" >&2
  exit 1
fi
# The unpublished core patch changes each archive's lockfile. Cargo resolves that patch against
# the extracted core, not against a registry release or a workspace source path.
for crate in "${crates[@]}"; do
  directories=("$scratch/extracted/$crate"-*)
  echo "▶ testing extracted package $crate"
  (cd "${directories[0]}" && cargo nextest run --all-features --no-fail-fast --config "$scratch/patch.toml")
  size=$(du -sk "$target" | cut -f1)
  if [ "$size" -gt 20971520 ]; then
    echo "package test target: expected at most 20 GiB, received $size KiB" >&2
    exit 1
  fi
done
echo 'all extracted package tests passed'
