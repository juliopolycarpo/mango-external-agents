#!/usr/bin/env bash
# Creates the GitHub release for a version, unless one already exists.
#
# `gh release create` has no idempotent form: it fails when the tag already has a release, so a full
# rerun of the release workflow after the release exists failed at the last step. A release that is
# already there is left alone, notes and all, because a maintainer may have edited it since. When
# the lookup itself fails (no release, or GitHub unreachable) the create is attempted, and if that
# fails too the workflow fails with GitHub's own message.
#
# Usage: scripts/publish-github-release.sh <version> <notes-file>
#   GH_TOKEN and GH_REPO (or a checkout with a remote) select the repository.
set -euo pipefail

usage() {
  echo "usage: scripts/publish-github-release.sh <version> <notes-file>" >&2
}

publish_release() {
  local version="$1"
  local notes="$2"
  local tag="v$version"
  local prerelease=()

  if [ ! -f "$notes" ]; then
    printf 'expected release notes file %s, received none\n' "$notes" >&2
    return 2
  fi
  if gh release view "$tag" >/dev/null 2>&1; then
    echo "GitHub release $tag already exists; leaving it as it is"
    return
  fi
  case "$version" in
    *-*) prerelease=(--prerelease) ;;
  esac
  gh release create "$tag" --title "$tag" --notes-file "$notes" "${prerelease[@]}"
}

main() {
  if [ $# -ne 2 ]; then
    usage
    return 2
  fi
  publish_release "$1" "$2"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
