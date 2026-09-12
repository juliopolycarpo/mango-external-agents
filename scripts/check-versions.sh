#!/usr/bin/env bash
# Lockstep: one version in [workspace.package], inherited by every crate, and repeated
# verbatim in every in-workspace [workspace.dependencies] entry.
# Usage: scripts/check-versions.sh [expected]   # expected = a tag without the leading v
set -euo pipefail
cd "$(dirname "$0")/.."

workspace_version=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p}' Cargo.toml)
if [ -z "$workspace_version" ]; then
  echo "expected [workspace.package] version = \"x.y.z\" in Cargo.toml, received none" >&2
  exit 1
fi

status=0
if [ $# -gt 0 ] && [ "$1" != "$workspace_version" ]; then
  echo "expected workspace version $1, received $workspace_version" >&2
  status=1
fi

for manifest in crates/*/Cargo.toml examples/*/Cargo.toml; do
  if ! grep -q '^version.workspace = true' "$manifest"; then
    echo "$manifest: expected 'version.workspace = true', received its own version" >&2
    status=1
  fi
done

while IFS= read -r line; do
  name=${line%% *}
  dep_version=$(printf '%s' "$line" | sed -n 's/.*version = "\([^"]*\)".*/\1/p')
  if [ "$dep_version" != "$workspace_version" ]; then
    echo "workspace dependency $name: expected version \"$workspace_version\", received \"$dep_version\"" >&2
    status=1
  fi
done < <(sed -n '/^\[workspace\.dependencies\]/,/^\[/p' Cargo.toml | grep -E '^mango-' || true)

[ $status -eq 0 ] && echo "versions in lockstep: $workspace_version"
exit $status
