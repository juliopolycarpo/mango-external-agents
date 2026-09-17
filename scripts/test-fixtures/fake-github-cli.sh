#!/usr/bin/env bash
# Named fake for scripts/test-vendor-drift.sh. It records GitHub CLI calls without touching GitHub.
set -euo pipefail

printf '%s\n' "$*" >> "${FAKE_GH_LOG:?expected FAKE_GH_LOG}"

if [ "${FAKE_GH_FAILURE:-}" = "$1-$2" ]; then
  exit 1
fi

case "$1 $2" in
  'issue list')
    if [ -n "${FAKE_GH_ISSUE_NUMBER:-}" ]; then
      printf '%s\tdrift(%s): public contract changed\n' \
        "$FAKE_GH_ISSUE_NUMBER" "${FAKE_GH_VENDOR:?expected FAKE_GH_VENDOR}"
    fi
    ;;
  'issue view')
    printf '%s\n' "${FAKE_GH_ISSUE_BODY:-}"
    ;;
esac
