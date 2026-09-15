#!/usr/bin/env bash
# Resolve only a versioned tag, including manually dispatched releases.
# Usage: scripts/release-version.sh refs/tags/v0.1.0 [0.1.0]
set -euo pipefail
ref=${1:-}
requested=${2:-}
if [[ ! "$ref" =~ ^refs/tags/v([0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?)$ ]]; then
  echo "expected refs/tags/v<semver>, received '$ref'" >&2
  exit 1
fi
version=${BASH_REMATCH[1]}
if [[ "$version" == *-canary* ]]; then
  echo "expected a release tag without canary, received '$ref'" >&2
  exit 1
fi
if [[ -n "$requested" && "$requested" != "$version" ]]; then
  echo "expected requested version '$version' to match '$ref', received '$requested'" >&2
  exit 1
fi
printf '%s\n' "$version"
