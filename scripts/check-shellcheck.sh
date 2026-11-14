#!/usr/bin/env bash
# Runs shellcheck over every shell script under scripts/, the test fixtures' fakes included.
# `.shellcheckrc` at the repository root makes it follow the `# shellcheck source=` directives.
# A finding that is intentional gets a `# shellcheck disable=SCxxxx # reason` directive in the
# script, never a blanket exclusion here.
#
# CI's Policy job runs this and a missing shellcheck fails it there. scripts/check.sh runs it only
# when shellcheck is on PATH and warns once otherwise.
# Usage: scripts/check-shellcheck.sh [repository-root]
set -euo pipefail
cd "${1:-$(dirname "$0")/..}"

if ! command -v shellcheck >/dev/null 2>&1; then
  echo "expected shellcheck on PATH, received none (see docs/testing.md for the pinned install)" >&2
  exit 1
fi

# The version goes in the log: a hosted runner image ships its own shellcheck, and this line shows
# which binary judged the scripts.
shellcheck --version | sed -n 's/^version: */shellcheck version /p'

# Sorted so the invocation, and a finding's order in the output, does not depend on directory order.
mapfile -t scripts < <(find scripts -type f -name '*.sh' | LC_ALL=C sort)
if [ "${#scripts[@]}" -eq 0 ]; then
  echo "expected shell scripts under scripts/, received none in $PWD" >&2
  exit 1
fi

shellcheck "${scripts[@]}"
echo "shellcheck: ${#scripts[@]} scripts clean"
