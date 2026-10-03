#!/usr/bin/env bash
# The current registry consumer must pin a release that exists, so its pin can only be bumped after
# crates.io has the new version, while the workspace version is bumped by the release pull request
# before it. The pin is therefore allowed to trail the workspace version by exactly one release:
#
#   - the workspace version itself (the release is out and the pin was bumped);
#   - the previous patch (the release pull request, or the release that was just published);
#   - when the workspace version starts a new line (x.y.0), any release of the line before it;
#   - a pre-release of the workspace version's own x.y.z (`0.5.0-rc.1` while releasing `0.5.0-rc.2`
#     or `0.5.0`), because a pre-release train is released one tag after another.
#
# Anything older means a release went out without its follow-up bump, and the "current release"
# consumer is no longer testing the current release. The fix is in docs/releasing.md.
# Usage: scripts/check-consumer-pin.sh [workspace-version pin]   # both default to the manifests
set -euo pipefail
cd "$(dirname "$0")/.."

consumer=tests/standalone-current/Cargo.toml
version_pattern='^([0-9]+)\.([0-9]+)\.([0-9]+)(-.*)?$'

if [ $# -ne 0 ] && [ $# -ne 2 ]; then
  echo "expected 0 or 2 arguments (workspace-version pin), received $#" >&2
  exit 2
fi

if [ $# -eq 2 ]; then
  workspace_version=$1
  pin=$2
else
  workspace_version=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p}' Cargo.toml)
  pins=$(sed -n 's/^mango-[a-z-]* = { version = "=\([^"]*\)".*/\1/p' "$consumer" | sort -u)
  if [ "$(printf '%s\n' "$pins" | grep -c .)" -ne 1 ]; then
    echo "$consumer: expected one exact '=x.y.z' pin shared by every mango-* dependency, received: ${pins//$'\n'/ }" >&2
    exit 1
  fi
  pin=$pins
fi

if [[ ! $workspace_version =~ $version_pattern ]]; then
  echo "expected workspace version x.y.z, received '$workspace_version'" >&2
  exit 1
fi
w_major=${BASH_REMATCH[1]} w_minor=${BASH_REMATCH[2]} w_patch=${BASH_REMATCH[3]}
if [ "$pin" = "$workspace_version" ]; then
  echo "registry consumer pin =$pin matches the workspace version"
  exit 0
fi
if [[ ! $pin =~ $version_pattern ]]; then
  echo "expected a released x.y.z pin, received '$pin'" >&2
  exit 1
fi
p_major=${BASH_REMATCH[1]} p_minor=${BASH_REMATCH[2]} p_patch=${BASH_REMATCH[3]} p_suffix=${BASH_REMATCH[4]}

ok=false
if [ -n "$p_suffix" ]; then
  # A pre-release pin is only the step before the same x.y.z (another pre-release, or the final).
  [ "$p_major.$p_minor.$p_patch" = "$w_major.$w_minor.$w_patch" ] && ok=true
elif [ "$p_major" = "$w_major" ] && [ "$p_minor" = "$w_minor" ] && [ "$w_patch" -gt 0 ] \
  && [ "$p_patch" -eq $((w_patch - 1)) ]; then
  ok=true
elif [ "$w_patch" -eq 0 ]; then
  if [ "$w_minor" -gt 0 ] && [ "$p_major" = "$w_major" ] && [ "$p_minor" -eq $((w_minor - 1)) ]; then
    ok=true
  elif [ "$w_minor" -eq 0 ] && [ "$w_major" -gt 0 ] && [ "$p_major" -eq $((w_major - 1)) ]; then
    ok=true
  fi
fi

if [ "$ok" = false ]; then
  {
    echo "$consumer: expected pin =$workspace_version or the release just before it, received =$pin (workspace $workspace_version)"
    echo "  after a release is on crates.io, bump the four '=' pins and regenerate tests/standalone-current/Cargo.lock;"
    echo "  see step 5 of 'Cut a release' in docs/releasing.md"
  } >&2
  exit 1
fi
echo "registry consumer pin =$pin trails workspace $workspace_version by one release (bump it once $workspace_version is published)"
