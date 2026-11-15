#!/usr/bin/env bash
# Named fake for scripts/test-prune-rust-caches.sh. It records GitHub CLI calls without touching
# GitHub.
#
# The cache listing is the exact `gh api --paginate` call the prune script must send; the fake
# prints the file named by FAKE_GH_LISTING verbatim, so a test can hand it several concatenated
# pages (what --paginate prints) or a malformed body. A DELETE of a cache id is logged and succeeds,
# except for the id in FAKE_GH_DELETE_FAILS_FOR, which exits 1 with a message. Any other call exits
# 64 and says what it received.
set -euo pipefail

call="$*"
printf '%s\n' "$call" >> "${FAKE_GH_LOG:?expected FAKE_GH_LOG}"

listing_call='api --paginate repos/owner/repository/actions/caches?per_page=100'
case "$call" in
  "$listing_call")
    cat "${FAKE_GH_LISTING:?expected FAKE_GH_LISTING}"
    ;;
  'api --method DELETE repos/owner/repository/actions/caches/'*)
    cache_id=${call##*/}
    if [ "$cache_id" = "${FAKE_GH_DELETE_FAILS_FOR:-}" ]; then
      echo "fake gh: HTTP 500 deleting cache $cache_id" >&2
      exit 1
    fi
    ;;
  *)
    echo "fake gh: expected '$listing_call' or 'api --method DELETE repos/owner/repository/actions/caches/<id>' | received: $call" >&2
    exit 64
    ;;
esac
