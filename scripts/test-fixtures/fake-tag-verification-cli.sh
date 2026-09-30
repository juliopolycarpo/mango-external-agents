#!/usr/bin/env bash
# Named fake for scripts/test-release.sh. It answers `gh api` like GitHub's tag endpoints, for one
# tag, and records every call.
#
#   FAKE_TAG_NAME         the only tag ref that exists (`v0.4.0`); any other ref is a 404
#   FAKE_TAG_OBJECT       `tag` for an annotated tag, `commit` for a lightweight one
#   FAKE_TAG_VERIFIED     `true` or `false`, as the tag object's verification reports
#   FAKE_TAG_REASON       the verification reason (`valid`, `unsigned`, `unknown_key`, ...)
#   FAKE_TAG_SIGNED_NAME  the name inside the tag object; defaults to FAKE_TAG_NAME
#   FAKE_TAG_COMMIT       the commit the tag object points at
set -euo pipefail

printf '%s\n' "$*" >> "${FAKE_GH_LOG:?expected FAKE_GH_LOG}"

if [ "$1" != 'api' ]; then
  printf 'expected gh api, received gh %s\n' "$1" >&2
  exit 2
fi

endpoint="$2"
jq_filter="$4"
name="${FAKE_TAG_NAME:?expected FAKE_TAG_NAME}"
object_sha='5697f3cb791cb239f4f1b5e7110c34c0b14e326a'
commit="${FAKE_TAG_COMMIT:-56307d67182e53594382e04682800b36eb421689}"

if [ "$3" != '--jq' ]; then
  printf 'expected gh api <endpoint> --jq <filter>, received %s\n' "$*" >&2
  exit 2
fi

case "$endpoint" in
  "repos/owner/repository/git/ref/tags/$name")
    jq -n -c --arg type "${FAKE_TAG_OBJECT:-tag}" --arg sha "$object_sha" \
      '{object: {type: $type, sha: $sha}}' | jq -r "$jq_filter"
    ;;
  "repos/owner/repository/git/tags/$object_sha")
    jq -n -c --argjson verified "${FAKE_TAG_VERIFIED:-true}" --arg reason "${FAKE_TAG_REASON:-valid}" \
      --arg tag "${FAKE_TAG_SIGNED_NAME:-$name}" --arg commit "$commit" \
      '{tag: $tag, verification: {verified: $verified, reason: $reason}, object: {type: "commit", sha: $commit}}' |
      jq -r "$jq_filter"
    ;;
  *)
    echo 'gh: Not Found (HTTP 404)' >&2
    exit 1
    ;;
esac
