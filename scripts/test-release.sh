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

# A pull request title becomes the squash subject, and git-cliff reads that subject. The rejected
# case below is verbatim the title that shipped v0.1.0's release notes without the pull request
# that produced the tagged tree.
for title in 'feat(core): add a thing' 'fix: no scope' 'docs(release): record the bootstrap' \
  'feat(acp)!: breaking change'; do
  if ! scripts/check-pr-title.sh "$title" >/dev/null; then
    echo "expected acceptance of '$title', received the rejection above" >&2
    exit 1
  fi
done
while IFS= read -r title; do
  if scripts/check-pr-title.sh "$title" >/dev/null 2>&1; then
    echo "expected rejection of '$title', received success" >&2
    exit 1
  fi
done <<'TITLES'
Release gate: host adoption, Hub-owned retry, and publication readiness (#18)
feat(nope): a scope AGENTS.md does not name
nope(core): a type the changelog cannot group
feat(core):no space after the colon
feat(core):
TITLES
if scripts/check-pr-title.sh >/dev/null 2>&1; then
  echo 'expected rejection of an empty title, received success' >&2
  exit 1
fi
echo 'pull request title checks passed'
