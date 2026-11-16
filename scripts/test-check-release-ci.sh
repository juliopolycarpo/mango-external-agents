#!/usr/bin/env bash
# Tests for scripts/check-release-ci.sh. Every case runs the real script against a named fake `gh`
# (scripts/test-fixtures/fake-ci-runs-cli.sh) that serves canned run and job listings, so nothing
# reaches GitHub. The script has to pass only for a completed, successful CI run on a push to main
# for the exact commit, and to refuse every other answer.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
check="$repo_root/scripts/check-release-ci.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/mea-release-ci-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

mkdir -p "$work/bin"
cp "$repo_root/scripts/test-fixtures/fake-ci-runs-cli.sh" "$work/bin/gh"
export PATH="$work/bin:$PATH"
export FAKE_CI_LOG="$work/gh.log"
export FAKE_CI_RUNS="$work/runs.json"
export FAKE_CI_JOBS="$work/jobs.json"
export FAKE_CI_COMMIT=56307d67182e53594382e04682800b36eb421689
export FAKE_CI_RUN_ID=9001
other_commit=1111111111111111111111111111111111111111

fail() {
  echo "test failed: $*" >&2
  exit 1
}

# run_entry ID COMMIT EVENT BRANCH STATUS CONCLUSION CREATED_AT -> one workflow run as JSON.
# A CONCLUSION of `null` is the JSON null a run that has not finished reports.
run_entry() {
  jq -n -c --argjson id "$1" --arg commit "$2" --arg event "$3" --arg branch "$4" --arg status "$5" \
    --arg conclusion "$6" --arg created "$7" \
    '{id: $id, head_sha: $commit, event: $event, head_branch: $branch, status: $status,
      conclusion: (if $conclusion == "null" then null else $conclusion end), created_at: $created,
      html_url: "https://github.com/owner/repository/actions/runs/\($id)"}'
}

# write_runs ENTRY... -> the listing body.
write_runs() {
  jq -n -c '$ARGS.positional | map(fromjson) | {total_count: length, workflow_runs: .}' --args "$@" > "$FAKE_CI_RUNS"
}

# job_entry NAME STATUS CONCLUSION -> one job as JSON.
job_entry() {
  jq -n -c --arg name "$1" --arg status "$2" --arg conclusion "$3" \
    '{name: $name, status: $status, conclusion: (if $conclusion == "null" then null else $conclusion end)}'
}

# write_jobs TOTAL JOB... -> the jobs body, claiming TOTAL jobs in all.
write_jobs() {
  local total=$1
  shift
  jq -n -c --argjson total "$total" '$ARGS.positional | map(fromjson) | {total_count: $total, jobs: .}' --args "$@" > "$FAKE_CI_JOBS"
}

passing_jobs() {
  write_jobs 4 \
    "$(job_entry 'Rust (ubuntu-latest)' completed success)" \
    "$(job_entry 'Rust (windows-latest)' completed success)" \
    "$(job_entry 'Policy (deny, versions, docs format)' completed success)" \
    "$(job_entry Gate completed success)"
}

green_run() {
  run_entry "$FAKE_CI_RUN_ID" "$FAKE_CI_COMMIT" push main completed success 2026-11-16T10:00:00Z
}

# run_check [COMMIT] -> runs the script, leaving stdout, stderr and the status for the assertions.
run_check() {
  : > "$FAKE_CI_LOG"
  set +e
  "$check" owner/repository "${1:-$FAKE_CI_COMMIT}" > "$work/out" 2> "$work/err"
  status=$?
  set -e
}

expect_status() {
  local expected=$1
  [ "$status" = "$expected" ] || fail "$2: expected exit $expected, received $status; stdout: $(cat "$work/out"); stderr: $(cat "$work/err")"
}

expect_refusal() {
  local label=$1 status_wanted=$2 wanted=$3
  expect_status "$status_wanted" "$label"
  grep -qF -- "$wanted" "$work/err" || fail "$label: expected stderr to say '$wanted', received: $(cat "$work/err")"
  [ ! -s "$work/out" ] || fail "$label: expected nothing on stdout for a refusal, received: $(cat "$work/out")"
}

# The one passing shape.
write_runs "$(green_run)"
passing_jobs
run_check
expect_status 0 'a successful run of the commit'
grep -qF -- "commit $FAKE_CI_COMMIT passed ci.yml on main: run 9001" "$work/out" ||
  fail "expected the pass to name the commit and run, received: $(cat "$work/out")"
grep -qF -- "head_sha=$FAKE_CI_COMMIT&event=push&branch=main" "$FAKE_CI_LOG" ||
  fail "expected the listing to ask for this commit's push runs on main, received: $(cat "$FAKE_CI_LOG")"

# Nothing that is not this commit's push run on main counts, whatever the listing holds.
write_runs
run_check
expect_refusal 'no run at all' 1 "expected a ci.yml run on main for commit $FAKE_CI_COMMIT, received none"
write_runs "$(run_entry 9001 "$other_commit" push main completed success 2026-11-16T10:00:00Z)"
run_check
expect_refusal 'a green run of another commit' 1 'received none'
write_runs "$(run_entry 9001 "$FAKE_CI_COMMIT" pull_request main completed success 2026-11-16T10:00:00Z)"
run_check
expect_refusal 'a green pull request run' 1 'received none'
write_runs "$(run_entry 9001 "$FAKE_CI_COMMIT" push release/x completed success 2026-11-16T10:00:00Z)"
run_check
expect_refusal 'a green run on another branch' 1 'received none'

# A run that has not finished, or finished badly, is not a pass.
write_runs "$(run_entry 9001 "$FAKE_CI_COMMIT" push main in_progress null 2026-11-16T10:00:00Z)"
run_check
expect_refusal 'a run still in progress' 1 'to have completed, received status=in_progress'
write_runs "$(run_entry 9001 "$FAKE_CI_COMMIT" push main queued null 2026-11-16T10:00:00Z)"
run_check
expect_refusal 'a queued run' 1 'received status=queued'
for conclusion in failure cancelled timed_out skipped action_required neutral; do
  write_runs "$(run_entry 9001 "$FAKE_CI_COMMIT" push main completed "$conclusion" 2026-11-16T10:00:00Z)"
  run_check
  expect_refusal "a $conclusion run" 1 "to conclude success, received conclusion=$conclusion"
done
write_runs "$(run_entry 9001 "$FAKE_CI_COMMIT" push main completed null 2026-11-16T10:00:00Z)"
run_check
expect_refusal 'a completed run with no conclusion' 1 'received conclusion=none'

# The newest run of the commit decides, so a later red run is not hidden behind an older green one.
write_runs \
  "$(run_entry 9001 "$FAKE_CI_COMMIT" push main completed success 2026-11-16T10:00:00Z)" \
  "$(run_entry 9005 "$FAKE_CI_COMMIT" push main completed failure 2026-11-16T11:00:00Z)"
run_check
expect_refusal 'a failed run after a green one' 1 'run 9005 of commit'
write_runs \
  "$(run_entry 9005 "$FAKE_CI_COMMIT" push main completed failure 2026-11-16T09:00:00Z)" \
  "$(green_run)"
run_check
expect_status 0 'a green rerun after a failed run'

# A green run is not enough when its jobs are not all green.
passing_jobs
write_runs "$(green_run)"
write_jobs 4 \
  "$(job_entry 'Rust (ubuntu-latest)' completed success)" \
  "$(job_entry 'Rust (windows-latest)' completed skipped)" \
  "$(job_entry 'Policy (deny, versions, docs format)' completed success)" \
  "$(job_entry Gate completed success)"
run_check
expect_refusal 'a skipped job' 1 'Rust (windows-latest) status=completed conclusion=skipped'
write_jobs 2 \
  "$(job_entry 'Rust (ubuntu-latest)' completed failure)" \
  "$(job_entry Gate completed success)"
run_check
expect_refusal 'a failed job under a green run' 1 'Rust (ubuntu-latest) status=completed conclusion=failure'
write_jobs 2 \
  "$(job_entry 'Rust (ubuntu-latest)' in_progress null)" \
  "$(job_entry Gate completed success)"
run_check
expect_refusal 'a job still running' 1 'Rust (ubuntu-latest) status=in_progress conclusion=none'
write_jobs 2 \
  "$(job_entry 'Rust (ubuntu-latest)' completed success)" \
  "$(job_entry 'Policy (deny, versions, docs format)' completed success)"
run_check
expect_refusal 'a run without the Gate job' 1 'to include the Gate job, received only: Rust (ubuntu-latest), Policy (deny, versions, docs format)'
write_jobs 0
run_check
expect_refusal 'a run with no jobs' 1 'to list every one of its jobs, received 0 of 0'
write_jobs 120 \
  "$(job_entry 'Rust (ubuntu-latest)' completed success)" \
  "$(job_entry Gate completed success)"
run_check
expect_refusal 'a truncated job list' 1 'received 2 of 120'

# An API that fails, or answers with something else, is not a pass either, and is told apart (2).
passing_jobs
write_runs "$(green_run)"
export FAKE_CI_FAIL=runs
run_check
unset FAKE_CI_FAIL
expect_refusal 'a failing runs call' 2 'could not read them'
export FAKE_CI_FAIL=jobs
run_check
unset FAKE_CI_FAIL
expect_refusal 'a failing jobs call' 2 'expected to read the jobs of run 9001'
printf 'not json' > "$FAKE_CI_RUNS"
run_check
expect_refusal 'a malformed runs body' 2 'expected a JSON list of workflow runs'
write_runs "$(green_run)"
printf '{"unexpected": true}' > "$FAKE_CI_JOBS"
run_check
expect_refusal 'a jobs body without a list' 2 'expected a JSON job list with total_count and jobs for run 9001'
printf '{"total_count": 4}' > "$FAKE_CI_JOBS"
run_check
expect_refusal 'a jobs body with a count and no list' 2 'expected a JSON job list with total_count and jobs for run 9001'
printf '{"jobs": []}' > "$FAKE_CI_JOBS"
run_check
expect_refusal 'a jobs body with a list and no count' 2 'expected a JSON job list with total_count and jobs for run 9001'
printf 'not json' > "$FAKE_CI_JOBS"
run_check
expect_refusal 'a malformed jobs body' 2 'expected a JSON job list for run 9001'

# Arguments: a commit has to be spelled in full, so a branch name or a short sha cannot match a run.
passing_jobs
run_check main
expect_refusal 'a branch name for a commit' 2 'expected a full 40-character lowercase commit sha, received main'
run_check 56307d6
expect_refusal 'a short sha' 2 'received 56307d6'
set +e
"$check" owner/repository > "$work/out" 2> "$work/err"
status=$?
set -e
expect_refusal 'a missing commit argument' 2 'usage: scripts/check-release-ci.sh <owner/repo> <commit-sha>'

echo 'release CI check tests passed'
