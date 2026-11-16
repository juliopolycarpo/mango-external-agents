#!/usr/bin/env bash
# Refuses a release whose commit has no successful CI run on main.
#
# The release workflow used to run the whole of scripts/check.sh again on a tag. Every line of it is
# already run by the CI workflow on the push that put the commit on main, so this asks GitHub
# whether that run passed instead. The `Gate` job of ci.yml is not a required status check on the
# `main` ruleset (issue #498), so a commit can reach main with CI red and a tag can name it. Hence
# the question is asked of the run itself, for this exact commit, and every answer but a clear yes
# fails closed.
#
# The run counts when all of these hold:
#   - it is a run of .github/workflows/ci.yml, on a push to main, for the commit being released;
#   - it is the newest such run, so a later red or cancelled run of the same commit is not hidden
#     behind an older green one;
#   - it has completed with the conclusion `success`;
#   - every job of its latest attempt completed with `success`, and one of them is `Gate`. A skipped
#     job counts as a failure here: the run's own conclusion treats it as a pass (issue #476).
#
# No run, a run that is still queued or in progress, a cancelled, failed, timed-out or skipped one,
# a job list that is empty, truncated or lacks `Gate`, and an API error are all refusals. Rerun the
# release once CI has finished.
#
# Exit status: 0 the commit passed, 1 CI did not pass for it, 2 usage or an API error.
#
# Usage: scripts/check-release-ci.sh <owner/repo> <commit-sha>
#   GH_TOKEN authorises the reads; `actions: read` is enough.
set -euo pipefail

CI_WORKFLOW_FILE='ci.yml'
GATE_JOB='Gate'
PAGE_SIZE=100

usage() {
  echo "usage: scripts/check-release-ci.sh <owner/repo> <commit-sha>" >&2
}

verify_ci() {
  local repository="$1"
  local commit="$2"
  local runs
  local run
  local run_id status conclusion url
  local jobs
  local total listed unsuccessful

  if ! [[ "$commit" =~ ^[0-9a-f]{40}$ ]]; then
    printf 'expected a full 40-character lowercase commit sha, received %q\n' "$commit" >&2
    return 2
  fi

  if ! runs=$(gh api \
    "repos/$repository/actions/workflows/$CI_WORKFLOW_FILE/runs?head_sha=$commit&event=push&branch=main&per_page=$PAGE_SIZE"); then
    printf 'expected to read the %s runs of %s for commit %s, could not read them\n' \
      "$CI_WORKFLOW_FILE" "$repository" "$commit" >&2
    return 2
  fi

  # The filters of the request are repeated here, so an API that ignored one could not lend its run.
  if ! run=$(jq -r --arg commit "$commit" '
      [.workflow_runs[]
       | select(.head_sha == $commit and .event == "push" and .head_branch == "main")]
      | sort_by([.created_at, .id])
      | last
      | if . == null then empty
        else [.id, .status, (.conclusion // "none"), .html_url] | @tsv end' <<<"$runs"); then
    printf 'expected a JSON list of workflow runs from %s, received: %s\n' "$repository" "${runs:0:200}" >&2
    return 2
  fi
  if [ -z "$run" ]; then
    printf 'expected a %s run on main for commit %s, received none (push the commit to main and let CI finish first)\n' \
      "$CI_WORKFLOW_FILE" "$commit" >&2
    return 1
  fi
  IFS=$'\t' read -r run_id status conclusion url <<EOF
$run
EOF
  if [ "$status" != 'completed' ]; then
    printf 'expected the %s run %s of commit %s to have completed, received status=%s (%s); rerun the release when it has finished\n' \
      "$CI_WORKFLOW_FILE" "$run_id" "$commit" "$status" "$url" >&2
    return 1
  fi
  if [ "$conclusion" != 'success' ]; then
    printf 'expected the %s run %s of commit %s to conclude success, received conclusion=%s (%s)\n' \
      "$CI_WORKFLOW_FILE" "$run_id" "$commit" "$conclusion" "$url" >&2
    return 1
  fi

  if ! jobs=$(gh api "repos/$repository/actions/runs/$run_id/jobs?per_page=$PAGE_SIZE&filter=latest"); then
    printf 'expected to read the jobs of run %s of %s, could not read them\n' "$run_id" "$repository" >&2
    return 2
  fi
  if ! total=$(jq -r '.total_count' <<<"$jobs") || ! listed=$(jq -r '.jobs | length' <<<"$jobs"); then
    printf 'expected a JSON job list for run %s, received: %s\n' "$run_id" "${jobs:0:200}" >&2
    return 2
  fi
  if [ "$total" = 'null' ] || [ "$listed" = 'null' ] || [ "$total" != "$listed" ] || [ "$listed" -eq 0 ]; then
    printf 'expected run %s to list every one of its jobs, received %s of %s (a page holds %s)\n' \
      "$run_id" "$listed" "$total" "$PAGE_SIZE" >&2
    return 1
  fi
  unsuccessful=$(jq -r '.jobs[]
    | select(.status != "completed" or .conclusion != "success")
    | "\(.name) status=\(.status) conclusion=\(.conclusion // "none")"' <<<"$jobs")
  if [ -n "$unsuccessful" ]; then
    printf 'expected every job of %s run %s to succeed, received:\n%s\n' \
      "$CI_WORKFLOW_FILE" "$run_id" "$unsuccessful" >&2
    return 1
  fi
  if ! jq -e --arg gate "$GATE_JOB" 'any(.jobs[]; .name == $gate)' <<<"$jobs" >/dev/null; then
    printf 'expected run %s to include the %s job, received only: %s\n' "$run_id" "$GATE_JOB" \
      "$(jq -r '[.jobs[].name] | join(", ")' <<<"$jobs")" >&2
    return 1
  fi
  printf 'commit %s passed %s on main: run %s (%s), %s jobs, all success\n' \
    "$commit" "$CI_WORKFLOW_FILE" "$run_id" "$url" "$listed"
}

main() {
  if [ $# -ne 2 ]; then
    usage
    return 2
  fi
  verify_ci "$1" "$2"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
