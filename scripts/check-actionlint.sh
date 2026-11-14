#!/usr/bin/env bash
# Runs actionlint over every workflow under .github/workflows/. actionlint checks the workflow
# syntax and expressions, and hands each `run:` block to shellcheck, which scripts/check-shellcheck.sh
# cannot reach. A finding that is intentional gets a `# shellcheck disable=SCxxxx # reason`
# directive on the first line of the `run:` block; never an `-ignore` flag here.
#
# actionlint skips the shellcheck pass without a word when shellcheck is not on PATH, so the gate
# requires both tools instead of letting a clean result mean "run blocks unchecked".
# CI's Policy job installs both (scripts/install-actionlint.sh for actionlint). scripts/check.sh
# runs this only when both are on PATH and warns once otherwise.
# Usage: scripts/check-actionlint.sh [repository-root]
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

for tool in actionlint shellcheck; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "expected $tool on PATH, received none (see docs/testing.md for the pinned install)" >&2
    exit 1
  fi
done

# The versions go in the log: they decide which findings a run can report.
actionlint -version | sed -n '1s/^/actionlint version /p'
shellcheck --version | sed -n 's/^version: */shellcheck version /p'

# Sorted so the invocation, and a finding's order in the output, does not depend on directory order.
mapfile -t workflows < <(find .github/workflows -maxdepth 1 -type f \( -name '*.yml' -o -name '*.yaml' \) | LC_ALL=C sort)
if [ "${#workflows[@]}" -eq 0 ]; then
  echo "expected workflow files under .github/workflows/, received none in $PWD" >&2
  exit 1
fi

actionlint "${workflows[@]}"
echo "actionlint: ${#workflows[@]} workflows clean"
