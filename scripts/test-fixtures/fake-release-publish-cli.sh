#!/usr/bin/env bash
# Named fake for scripts/test-release.sh. It records `gh release` calls without touching GitHub.
#
# `gh release view` succeeds only for the tag in FAKE_GH_EXISTING_RELEASE, as GitHub answers for a
# release that exists, and fails otherwise. `gh release create` fails for that tag, as GitHub does
# when a release already exists, so a script that creates blindly cannot pass.
set -euo pipefail

printf '%s\n' "$*" >> "${FAKE_GH_LOG:?expected FAKE_GH_LOG}"

if [ "$1" != 'release' ]; then
  printf 'expected gh release, received gh %s\n' "$1" >&2
  exit 2
fi

case "$2" in
  view)
    [ "${3:-}" = "${FAKE_GH_EXISTING_RELEASE:-}" ] || {
      echo "release not found" >&2
      exit 1
    }
    ;;
  create)
    if [ "${3:-}" = "${FAKE_GH_EXISTING_RELEASE:-}" ]; then
      echo "a release with the tag name $3 already exists" >&2
      exit 1
    fi
    ;;
  *)
    printf 'expected gh release view or create, received gh release %s\n' "$2" >&2
    exit 2
    ;;
esac
