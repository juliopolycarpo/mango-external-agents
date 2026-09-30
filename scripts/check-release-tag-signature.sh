#!/usr/bin/env bash
# Refuses a release whose tag GitHub does not report as signed and verified.
#
# docs/releasing.md requires a signed tag, and nothing else enforced it: the only ruleset covers
# the default branch and the `release` environment admits any `v*` tag. GitHub verifies the tag
# object's signature against the signing keys registered on the tagger's account and reports it as
# `verification.verified`, which a runner can read without holding any key.
#
# Three things are checked, all from the tag object GitHub returns:
#   - it is an annotated tag whose signature GitHub verified;
#   - the name inside the signed payload is the tag being released, so a ref cannot alias an object
#     that was signed as something else;
#   - it points at the commit this run was started for, so a tag moved after the run began cannot
#     lend its signature to a tree it does not cover.
#
# What this proves: the release ref is a verified annotated tag over the commit being built. What it
# does not: that the signer is one particular maintainer. Any account's registered signing key
# verifies, so who may push a `v*` tag stays the job of repository write access and the `release`
# environment. Nor does it defend against a pusher who also edits the workflow: a tag event runs the
# workflow file at the tagged commit, so only a repository ruleset can bind that.
#
# Usage: scripts/check-release-tag-signature.sh <owner/repo> <version> <commit-sha>
#   GH_TOKEN authorises the read; `contents: read` is enough.
set -euo pipefail

usage() {
  echo "usage: scripts/check-release-tag-signature.sh <owner/repo> <version> <commit-sha>" >&2
}

verify_tag() {
  local repository="$1"
  local version="$2"
  local commit="$3"
  local tag="v$version"
  local ref
  local object_type
  local object_sha
  local verdict
  local verified
  local reason
  local signed_name
  local tagged_commit

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
    --jq '[.verification.verified, .verification.reason, .tag, .object.sha] | @tsv'); then
    printf 'expected the tag object %s of %s to be readable, could not read it\n' "$object_sha" "$tag" >&2
    return 2
  fi
  IFS=$'\t' read -r verified reason signed_name tagged_commit <<EOF
$verdict
EOF
  if [ "$verified" != 'true' ]; then
    printf 'expected tag %s to be signed and verified by GitHub, received verified=%s reason=%s\n' \
      "$tag" "$verified" "$reason" >&2
    return 1
  fi
  if [ "$signed_name" != "$tag" ]; then
    printf 'expected tag %s to be signed under that name, received a signature over the name %s\n' \
      "$tag" "$signed_name" >&2
    return 1
  fi
  if [ "$tagged_commit" != "$commit" ]; then
    printf 'expected tag %s to point at the commit being released %s, received %s\n' \
      "$tag" "$commit" "$tagged_commit" >&2
    return 1
  fi
  printf 'tag %s is signed and verified by GitHub (reason: %s) over commit %s\n' \
    "$tag" "$reason" "$commit"
}

main() {
  if [ $# -ne 3 ]; then
    usage
    return 2
  fi
  verify_tag "$1" "$2" "$3"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
