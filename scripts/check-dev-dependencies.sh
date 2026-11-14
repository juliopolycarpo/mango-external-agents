#!/usr/bin/env bash
# A published crate's dev-dependencies must survive packaging.
#
# `cargo package` drops a dev-dependency that names only a `path`: the packaged manifest has no
# entry for it, so the features that entry turned on, such as a sibling crate's `launcher-tokio`
# or `testing`, are gone when the tarball's tests are built. `workspace = true` (or a `version`
# beside the `path`) keeps the entry.
#
# An entry that repeats the `[dependencies]` entry of the same name is refused as well: tests see
# every normal dependency already, so the copy is only a second version requirement to keep in
# step.
#
# The exception to the first rule is a crate's dev-dependency on itself, with its test features:
# packaging is meant to drop it. The parsing is in check-dev-dependencies.py.
#
# Usage: scripts/check-dev-dependencies.sh [manifest...]
#   with no arguments, every publishable crate's manifest under crates/ is checked.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$#" -eq 0 ]; then
  for manifest in crates/*/Cargo.toml; do
    grep -q '^publish = false' "$manifest" || set -- "$@" "$manifest"
  done
fi

for manifest in "$@"; do
  if [ ! -f "$manifest" ]; then
    echo "expected a manifest at $manifest, received no such file" >&2
    exit 1
  fi
done

python3 scripts/check-dev-dependencies.py "$@"
echo "published crates keep their dev-dependencies when packaged and repeat none"
