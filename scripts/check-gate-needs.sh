#!/usr/bin/env bash
# The rule of the `gate` job in .github/workflows/ci.yml: every job it needs ended with `success`.
#
# The job passes RESULTS="${{ toJSON(needs) }}", an object of job id to {result, outputs}. A need
# that is skipped (because one of its own needs failed) or cancelled is not a success, and the
# gate used to let skipped through. No job in the gate's `needs` has an `if:`, which
# scripts/ci-script-steps.py enforces, so `skipped` never means "intended". Nothing to read, or
# text that is not a non-empty object, fails as well: a gate with no needs proves nothing.
#
# Usage: RESULTS='{"rust":{"result":"success"}}' scripts/check-gate-needs.sh
set -euo pipefail

results=${RESULTS-}
if ! jq -e 'type == "object" and length > 0' >/dev/null 2>&1 <<<"$results"; then
  echo "(check-gate-needs): expected RESULTS to be a non-empty JSON object of job id to {result} | received: ${results:-<empty>}" >&2
  exit 1
fi

# A need that is no object, or has no `result`, is reported as `<missing>`.
not_succeeded=$(jq -r '
  to_entries[]
  | (.value | if type == "object" then .result else null end) as $result
  | select($result != "success")
  | "expected \(.key) result: success | received: \($result // "<missing>")"
' <<<"$results")

if [ -n "$not_succeeded" ]; then
  printf '%s\n' "$not_succeeded" >&2
  exit 1
fi
echo "gate: all $(jq 'length' <<<"$results") needed jobs succeeded"
