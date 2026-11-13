#!/usr/bin/env bash
# Resolve only a versioned tag, including manually dispatched releases.
# Usage: scripts/release-version.sh refs/tags/v0.1.0 [0.1.0]
set -euo pipefail
ref=${1:-}
requested=${2:-}
if [[ ! "$ref" =~ ^refs/tags/v((0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?)$ ]]; then
  echo "expected refs/tags/v<stable-or-prerelease-semver>, received '$ref'" >&2
  exit 1
fi
version=${BASH_REMATCH[1]}
core=${version%%-*}
IFS=. read -r -a components <<< "$core"
for component in "${components[@]}"; do
  if [ "${#component}" -gt 20 ] || { [ "${#component}" -eq 20 ] && [[ "$component" > 18446744073709551615 ]]; }; then
    echo "expected a SemVer core component within u64, received '$component' in '$ref'" >&2
    exit 1
  fi
done
if [[ "$version" == *-* ]]; then
  IFS=. read -r -a identifiers <<< "${version#*-}"
  for identifier in "${identifiers[@]}"; do
    if [[ "$identifier" =~ ^0[0-9]+$ ]]; then
      echo "expected a prerelease numeric identifier without leading zeroes, received '$identifier' in '$ref'" >&2
      exit 1
    fi
  done
fi
if [[ "$version" == *-canary* ]]; then
  echo "expected a release tag without canary, received '$ref'" >&2
  exit 1
fi
if [[ -n "$requested" && "$requested" != "$version" ]]; then
  echo "expected requested version '$version' to match '$ref', received '$requested'" >&2
  exit 1
fi
printf '%s\n' "$version"
