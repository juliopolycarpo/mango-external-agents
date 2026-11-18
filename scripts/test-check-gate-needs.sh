#!/usr/bin/env bash
# Regression coverage for scripts/check-gate-needs.sh, the rule of the `gate` job, and for how
# ci.yml reaches it. Each case feeds a fake `needs` JSON, shaped like `toJSON(needs)`, to the real
# script. The gate used to pass a need that was skipped (issue #476); that case fails first against
# the old `grep -E '"result": "(failure|cancelled)"'`.
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d "${TMPDIR:-/tmp}/check-gate-needs-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
gate="$PWD/scripts/check-gate-needs.sh"

fail() { echo "$*" >&2; exit 1; }

# needs_json <job=result>...: the `toJSON(needs)` of a gate whose needs ended with those results.
needs_json() {
  local pair args=()
  for pair in "$@"; do args+=("${pair%%=*}" "${pair#*=}"); done
  jq -n -c '[$ARGS.positional as $p | range(0; $p | length; 2) | {key: $p[.], value: {result: $p[. + 1], outputs: {}}}] | from_entries' --args "${args[@]}"
}

# expect_pass <case> <RESULTS>: the gate exits 0.
expect_pass() {
  local label=$1 status=0
  RESULTS=$2 "$gate" > "$work/output" 2>&1 || status=$?
  if [ "$status" -ne 0 ]; then
    cat "$work/output" >&2
    fail "(check-gate-needs): case '$label' | expected exit 0 | received exit $status"
  fi
}

# expect_fail <case> <RESULTS> <exact line>...: the gate exits 1 and prints each line, and no other
# line that starts with `expected <job> result`.
expect_fail() {
  local label=$1 results=$2 line status=0 lines
  shift 2
  RESULTS=$results "$gate" > "$work/output" 2>&1 || status=$?
  if [ "$status" -ne 1 ]; then
    cat "$work/output" >&2
    fail "(check-gate-needs): case '$label' | expected exit 1 | received exit $status"
  fi
  for line in "$@"; do
    if ! grep -qxF -- "$line" "$work/output"; then
      cat "$work/output" >&2
      fail "(check-gate-needs): case '$label' | expected line: $line | received the output above"
    fi
  done
  lines=$(grep -c '^expected .* result: success | received: ' "$work/output" || true)
  if [ "$lines" -ne "$#" ]; then
    cat "$work/output" >&2
    fail "(check-gate-needs): case '$label' | expected $# result lines | received $lines in the output above"
  fi
}

expect_pass 'every need succeeded' "$(needs_json rust=success powerset=success policy=success)"

expect_fail 'a skipped need' "$(needs_json rust=failure powerset=success policy=skipped)" \
  'expected rust result: success | received: failure' \
  'expected policy result: success | received: skipped'

expect_fail 'only a skipped need' "$(needs_json rust=success msrv=skipped)" \
  'expected msrv result: success | received: skipped'

expect_fail 'a failed need' "$(needs_json rust=failure policy=success)" \
  'expected rust result: success | received: failure'

expect_fail 'a cancelled need' "$(needs_json rust=success msrv-windows=cancelled)" \
  'expected msrv-windows result: success | received: cancelled'

expect_fail 'every kind of non-success at once' \
  "$(needs_json a=success b=skipped c=failure d=cancelled e=success)" \
  'expected b result: success | received: skipped' \
  'expected c result: success | received: failure' \
  'expected d result: success | received: cancelled'

# A need with no result string is not a success either.
expect_fail 'a need with no result' '{"rust":{"outputs":{}},"policy":"oops"}' \
  'expected rust result: success | received: <missing>' \
  'expected policy result: success | received: <missing>'

# Nothing to judge fails closed with the received value named.
for empty in '' '{}' 'not json' '[]' 'null'; do
  status=0
  RESULTS=$empty "$gate" > "$work/output" 2>&1 || status=$?
  [ "$status" -eq 1 ] || fail "(check-gate-needs): case 'RESULTS=$empty' | expected exit 1 | received exit $status"
  grep -qF 'expected RESULTS to be a non-empty JSON object of job id to {result} | received: ' "$work/output" ||
    { cat "$work/output" >&2; fail "(check-gate-needs): case 'RESULTS=$empty' | expected the shape message | received the output above"; }
done
status=0
env -u RESULTS "$gate" > "$work/output" 2>&1 || status=$?
[ "$status" -eq 1 ] || fail "(check-gate-needs): case 'RESULTS unset' | expected exit 1 | received exit $status"

# The real gate job runs the script on the `needs` JSON, not a copy of the rule. Its job needs a
# checkout for that, and the script test runs in the Policy job.
python3 -I - .github/workflows/ci.yml <<'GATE_WIRING' || fail "(check-gate-needs): the gate job of ci.yml does not run scripts/check-gate-needs.sh as expected"
import sys
import yaml

with open(sys.argv[1], encoding="utf-8") as handle:
    gate = yaml.safe_load(handle)["jobs"]["gate"]
problems = []
if gate.get("if") != "always()":
    problems.append(f"expected gate `if: always()` | received {gate.get('if')!r}")
steps = [step for step in gate["steps"] if step.get("run") == "scripts/check-gate-needs.sh"]
if len(steps) != 1:
    problems.append(f"expected one gate step running scripts/check-gate-needs.sh | received {len(steps)}")
elif steps[0].get("env", {}).get("RESULTS") != "${{ toJSON(needs) }}":
    problems.append(f"expected RESULTS: ${{{{ toJSON(needs) }}}} | received {steps[0].get('env')!r}")
if not any(str(step.get("uses", "")).startswith("actions/checkout@") for step in gate["steps"]):
    problems.append("expected a checkout step so the script exists on the runner | received none")
for problem in problems:
    print(f"(check-gate-needs): {problem}", file=sys.stderr)
sys.exit(1 if problems else 0)
GATE_WIRING

echo "check-gate-needs.sh checks passed"
