#!/usr/bin/env bash
# Lockstep: one version in [workspace.package], inherited by every crate, and repeated
# verbatim in every in-workspace [workspace.dependencies] entry. Each crate README's install
# requirement follows the release rule; see scripts/check-readme-requirements.sh.
# Third-party requirements are written once as well, in [workspace.dependencies]; see
# scripts/check-workspace-dependencies.py.
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

# A member that writes its own version of a dependency is a second requirement that can drift.
python3 scripts/check-workspace-dependencies.py Cargo.toml crates/*/Cargo.toml examples/*/Cargo.toml || status=1

# One README path per crate manifest, not a glob over READMEs: a deleted README would vanish from a
# glob and pass, where a named path is refused as missing.
readmes=()
for manifest in crates/*/Cargo.toml; do
  readmes+=("${manifest%Cargo.toml}README.md")
done
scripts/check-readme-requirements.sh "$workspace_version" "${readmes[@]}" || status=1

# The current registry consumer pins a published release, so it follows the workspace by one release.
scripts/check-consumer-pin.sh || status=1

[ $status -eq 0 ] && echo "versions in lockstep: $workspace_version"
exit $status
