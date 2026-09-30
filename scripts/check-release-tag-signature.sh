#!/usr/bin/env bash
# Refuses a release whose tag GitHub does not report as signed and verified.
#
# docs/releasing.md requires a signed tag, and nothing else enforced it: the only ruleset covers
# the default branch and the `release` environment admits any `v*` tag. GitHub verifies the tag
# object's signature against the signing keys registered on the tagger's account and reports it as
# `verification.verified`, which a runner can read without holding any key.
#
# What this proves: the tag is an annotated tag whose signature GitHub verified. What it does not:
# that the signer is one particular maintainer. Any account's registered signing key verifies, so
# who may push a `v*` tag stays the job of repository write access and the `release` environment.
#
# Usage: scripts/check-release-tag-signature.sh <owner/repo> <version>
#   GH_TOKEN authorises the read; `contents: read` is enough.
set -euo pipefail

usage() {
  echo "usage: scripts/check-release-tag-signature.sh <owner/repo> <version>" >&2
}

verify_tag() {
  local repository="$1"
  local version="$2"
  local tag="v$version"
  local ref
  local object_type
  local object_sha
  local verdict
  local verified
  local reason

  if ! ref=$(gh api "repos/$repository/git/ref/tags/$tag" \
    --jq '[.object.type, .object.sha] | @tsv'); then
    printf 'expected tag %s to exist in %s, could not read it\n' "$tag" "$repository" >&2
    return 2
  fi
  IFS=$'\t' read -r object_type object_sha <<EOF
$ref
EOF
  if [ "$object_type" != 'tag' ]; then
    printf 'expected %s to be an annotated signed tag, received a %s object %s (create it with git tag -s)\n' \
      "$tag" "$object_type" "$object_sha" >&2
    return 1
  fi
  if ! verdict=$(gh api "repos/$repository/git/tags/$object_sha" \
    --jq '[.verification.verified, .verification.reason] | @tsv'); then
    printf 'expected the tag object %s of %s to be readable, could not read it\n' "$object_sha" "$tag" >&2
    return 2
  fi
  IFS=$'\t' read -r verified reason <<EOF
$verdict
EOF
  if [ "$verified" != 'true' ]; then
    printf 'expected tag %s to be signed and verified by GitHub, received verified=%s reason=%s\n' \
      "$tag" "$verified" "$reason" >&2
    return 1
  fi
  printf 'tag %s is signed and verified by GitHub (reason: %s)\n' "$tag" "$reason"
}

main() {
  if [ $# -ne 2 ]; then
    usage
    return 2
  fi
  verify_tag "$1" "$2"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
