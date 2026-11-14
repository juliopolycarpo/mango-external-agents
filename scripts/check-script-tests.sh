#!/usr/bin/env bash
# Every scripts/test-*.{sh,py} must run in both places a change is judged: scripts/check.sh (local)
# and .github/workflows/ci.yml (CI). A test that only check.sh runs lets a change that skips
# check.sh break what it pins with CI still green; a test that neither runs is dead.
#
# A few tests are run by a parent test or gate instead of by name. Each is declared in `indirect`
# as `child:parent`; the parent has to be run by both places and has to name the child.
# Usage: scripts/check-script-tests.sh [repository-root]
set -euo pipefail
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
in_ci=$(named_in .github/workflows/ci.yml)

problems=''
note() { problems="$problems"$'\n'"  $*"; }

# A name in check.sh or ci.yml with no file behind it is a stale reference.
for name in $(printf '%s\n%s\n' "$in_check" "$in_ci" | sort -u); do
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
    grep -qxF "$parent" <<<"$in_ci" || note "scripts/$name runs through scripts/$parent, which .github/workflows/ci.yml does not run"
    if ! { [ -f "scripts/$parent" ] && grep -v '^ *#' "scripts/$parent" | grep -qF "$name"; }; then
      note "scripts/$name is declared as run by scripts/$parent, which does not name it"
    fi
    continue
  fi
  here=missing; grep -qxF "$name" <<<"$in_check" && here=present
  there=missing; grep -qxF "$name" <<<"$in_ci" && there=present
  if [ "$here" = missing ] && [ "$there" = missing ]; then
    note "scripts/$name is run by neither scripts/check.sh nor .github/workflows/ci.yml"
  elif [ "$here" = missing ]; then
    note "scripts/$name is run by .github/workflows/ci.yml but not by scripts/check.sh"
  elif [ "$there" = missing ]; then
    note "scripts/$name is run by scripts/check.sh but not by .github/workflows/ci.yml"
  fi
done

if [ -n "$problems" ]; then
  echo "expected every scripts/test-*.{sh,py} to run in scripts/check.sh and .github/workflows/ci.yml, or be declared in 'indirect' | received:$problems" >&2
  exit 1
fi
echo "script tests: all $(wc -l <<<"$on_disk") run by check.sh and ci.yml"
