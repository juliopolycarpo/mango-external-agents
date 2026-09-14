#!/usr/bin/env bash
# Named fake for release-asset checksum lookup tests.
set -euo pipefail

if [ "$1 $2" != 'release view' ]; then
  printf 'expected gh release view, received %s %s\n' "$1" "$2" >&2
  exit 2
fi

printf '{"assets":[{"name":"%s","digest":"sha256:%s"}]}\n' \
  "${FAKE_RELEASE_ASSET:?expected FAKE_RELEASE_ASSET}" "${FAKE_RELEASE_DIGEST:-}"
