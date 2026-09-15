#!/usr/bin/env bash
# Regression coverage for tag-only dispatch and mismatched release versions.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(scripts/release-version.sh refs/tags/v0.1.0)" == '0.1.0' ]]
[[ "$(scripts/release-version.sh refs/tags/v0.2.0-rc.1 0.2.0-rc.1)" == '0.2.0-rc.1' ]]
for ref in refs/heads/main refs/heads/v0.1.0 refs/tags/v0.1 refs/tags/v0.1.0-canary.1; do
  if scripts/release-version.sh "$ref" 0.1.0; then
    echo "expected rejection of non-release ref, received success for $ref" >&2
    exit 1
  fi
done
if scripts/release-version.sh refs/tags/v0.1.0 0.2.0; then
  echo 'expected rejection of mismatched version, received success' >&2
  exit 1
fi
echo 'release tag checks passed'
