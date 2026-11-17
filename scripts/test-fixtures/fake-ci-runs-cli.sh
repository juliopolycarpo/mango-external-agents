#!/usr/bin/env bash
# Named fake for scripts/test-check-release-ci.sh. It answers the two `gh api` calls
# scripts/check-release-ci.sh makes, from canned bodies, and logs every call.
#
#   FAKE_CI_RUNS       file whose content answers the workflow-runs listing of commit FAKE_CI_COMMIT
#   FAKE_CI_COMMIT     the commit the listing is asked for
#   FAKE_CI_JOBS       file whose content answers the jobs of run FAKE_CI_RUN_ID
#   FAKE_CI_RUN_ID     the run whose jobs exist; any other run is a 404
#   FAKE_CI_FAIL       `runs` or `jobs`: that call exits 1 with an HTTP error, as `gh` does
#
# The listing is served whole, whatever the query asks for: the script has to keep only what it
# wants, as it would if the API ignored a filter. Any other call exits 64 and says what it received.
set -euo pipefail

call="$*"
printf '%s\n' "$call" >> "${FAKE_CI_LOG:?expected FAKE_CI_LOG}"

commit="${FAKE_CI_COMMIT:?expected FAKE_CI_COMMIT}"
run_id="${FAKE_CI_RUN_ID:?expected FAKE_CI_RUN_ID}"
runs_call="api repos/owner/repository/actions/workflows/ci.yml/runs?head_sha=$commit&event=push&branch=main&per_page=100"
jobs_call="api repos/owner/repository/actions/runs/$run_id/jobs?per_page=100&filter=latest"

case "$call" in
  "$runs_call")
    if [ "${FAKE_CI_FAIL:-}" = runs ]; then
      echo 'gh: HTTP 502 (fake: the runs listing is unavailable)' >&2
      exit 1
    fi
    cat "${FAKE_CI_RUNS:?expected FAKE_CI_RUNS}"
    ;;
  "$jobs_call")
    if [ "${FAKE_CI_FAIL:-}" = jobs ]; then
      echo 'gh: HTTP 502 (fake: the jobs listing is unavailable)' >&2
      exit 1
    fi
    cat "${FAKE_CI_JOBS:?expected FAKE_CI_JOBS}"
    ;;
  api\ repos/owner/repository/actions/runs/*)
    echo 'gh: Not Found (HTTP 404)' >&2
    exit 1
    ;;
  *)
    echo "fake gh: expected '$runs_call' or '$jobs_call' | received: $call" >&2
    exit 64
    ;;
esac
