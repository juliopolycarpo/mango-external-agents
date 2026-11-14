#!/usr/bin/env bash
# Every scripts/test-*.{sh,py} must run in both places a change is judged: scripts/check.sh (local)
# and .github/workflows/ci.yml (CI). A test that only check.sh runs lets a change that skips
# check.sh break what it pins with CI still green; a test that neither runs is dead.
#
# scripts/check.sh is searched for names. ci.yml is parsed as YAML by scripts/ci-script-steps.py,
# which also requires the step to be able to run and fail the gate: a run command of a job listed in
# the gate job's `needs`, with no `if:` other than an OS matrix test and no swallowed exit status.
# It needs PyYAML (apt: python3-yaml).
#
# A few tests are run by a parent test or gate instead of by name. Each is declared in `indirect`
# as `child:parent`; the parent has to be run by both places and has to name the child.
# Usage: scripts/check-script-tests.sh [repository-root]
set -euo pipefail
steps_reader="$(cd "$(dirname "$0")" && pwd)/ci-script-steps.py"
cd "${1:-$(dirname "$0")/..}"

indirect=(
  test-codex-schema-inventory.py:test-vendor-drift.sh
  test-vendor-result.py:test-vendor-drift.sh
  # Run by the release gate (check-publish.sh); test-package-admission.sh exercises it in both places.
  test-packages.sh:test-package-admission.sh
)

# The test scripts a file runs: comment lines are dropped so a note cannot count as a run.
named_in() { grep -v '^ *#' "$1" | grep -oE 'test-[A-Za-z0-9_-]+\.(sh|py)' | sort -u || true; }

on_disk=$(cd scripts && find . -maxdepth 1 -type f \( -name 'test-*.sh' -o -name 'test-*.py' \) -printf '%f\n' | sort)
in_check=$(named_in scripts/check.sh)

# One record per test named in ci.yml: `ok`, `blocked` with a reason, or a `workflow` problem. A
# reader that cannot parse the file exits nonzero, which stops this script with its message.
ci_report=$(python3 "$steps_reader" .github/workflows/ci.yml)
in_ci=$(awk -F'\t' '$1 == "ok" { print $2 }' <<<"$ci_report" | sort -u)
named_in_ci=$(awk -F'\t' '$1 == "ok" || $1 == "blocked" { print $2 }' <<<"$ci_report" | sort -u)
# Why ci.yml cannot run a test it names, as "; reason" fragments; empty when nothing blocks it.
blocked_in_ci() { awk -F'\t' -v name="$1" '$1 == "blocked" && $2 == name { printf "; %s", $3 }' <<<"$ci_report"; }

problems=''
note() { problems="$problems"$'\n'"  $*"; }

# A workflow that cannot gate a pull request, whatever it names.
while IFS=$'\t' read -r kind reason; do
  if [ "$kind" = workflow ]; then note ".github/workflows/ci.yml: $reason"; fi
done <<<"$ci_report"

# A name in check.sh or ci.yml with no file behind it is a stale reference.
for name in $(printf '%s\n%s\n' "$in_check" "$named_in_ci" | sort -u); do
  if ! grep -qxF "$name" <<<"$on_disk"; then
    note "scripts/$name is run by check.sh or ci.yml but does not exist"
  fi
done

for name in $on_disk; do
  parent=''
  for pair in "${indirect[@]}"; do
    if [ "${pair%%:*}" = "$name" ]; then parent=${pair#*:}; fi
  done
  if [ -n "$parent" ]; then
    # Held to the parent: it must be run by both places and must name the child.
    grep -qxF "$parent" <<<"$in_check" || note "scripts/$name runs through scripts/$parent, which scripts/check.sh does not run"
    grep -qxF "$parent" <<<"$in_ci" || note "scripts/$name runs through scripts/$parent, which .github/workflows/ci.yml does not run$(blocked_in_ci "$parent")"
    if ! { [ -f "scripts/$parent" ] && grep -v '^ *#' "scripts/$parent" | grep -qF "$name"; }; then
      note "scripts/$name is declared as run by scripts/$parent, which does not name it"
    fi
    continue
  fi
  here=missing; grep -qxF "$name" <<<"$in_check" && here=present
  there=missing; grep -qxF "$name" <<<"$in_ci" && there=present
  blocked=$(blocked_in_ci "$name")
  if [ "$here" = missing ] && [ "$there" = missing ]; then
    note "scripts/$name is run by neither scripts/check.sh nor .github/workflows/ci.yml$blocked"
  elif [ "$here" = missing ]; then
    note "scripts/$name is run by .github/workflows/ci.yml but not by scripts/check.sh"
  elif [ "$there" = missing ]; then
    note "scripts/$name is run by scripts/check.sh but not by .github/workflows/ci.yml$blocked"
  fi
done

if [ -n "$problems" ]; then
  echo "expected every scripts/test-*.{sh,py} to run in scripts/check.sh and .github/workflows/ci.yml, or be declared in 'indirect' | received:$problems" >&2
  exit 1
fi
echo "script tests: all $(wc -l <<<"$on_disk") run by check.sh and ci.yml"
