#!/usr/bin/env bash
# A published crate's dev-dependencies must survive packaging.
#
# `cargo package` drops a dev-dependency that names only a `path`: the packaged manifest has no
# entry for it, so the features that entry turned on, such as a sibling crate's `launcher-tokio`
# or `testing`, are gone when the tarball's tests are built. `workspace = true` (or a `version`
# beside the `path`) keeps the entry.
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

status=0
for manifest in "$@"; do
  if [ ! -f "$manifest" ]; then
    echo "expected a manifest at $manifest, received no such file" >&2
    exit 1
  fi
  offenders=$(awk '
    /^\[/ { section = $0; next }
    section ~ /^\[(target\..*\.)?dev-dependencies(\..+)?\]$/ && /(^|[ ,{])path[ ]*=/ && !/version[ ]*=/ && !/workspace[ ]*=[ ]*true/ {
      printf "%s:%d: %s\n", FILENAME, FNR, $0
    }
  ' "$manifest")
  if [ -n "$offenders" ]; then
    echo "expected each dev-dependency to use workspace = true or carry a version beside its path, because cargo drops a path-only entry from the published manifest, received:" >&2
    printf '  %s\n' "$offenders" >&2
    status=1
  fi
done
[ "$status" -eq 0 ] || exit "$status"
echo "published crates keep their dev-dependencies when packaged"
