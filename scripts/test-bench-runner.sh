#!/usr/bin/env bash
# Regression coverage for scripts/check-bench-runner.sh: identical copies pass; a differing copy or
# a missing file is rejected with the expected/received message.
set -euo pipefail
cd "$(dirname "$0")/.."

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

printf 'runner\n' > "$scratch/a.rs"
printf 'runner\n' > "$scratch/same.rs"
printf 'runner, edited\n' > "$scratch/drifted.rs"

scripts/check-bench-runner.sh >/dev/null
scripts/check-bench-runner.sh "$scratch/a.rs" "$scratch/same.rs" >/dev/null

if scripts/check-bench-runner.sh "$scratch/a.rs" "$scratch/drifted.rs" >/dev/null 2>"$scratch/drift.err"; then
  echo "expected rejection of a drifted runner copy, received success" >&2
  exit 1
fi
grep -q 'to be identical, received a difference' "$scratch/drift.err" || {
  echo "expected the drift message to say the copies differ, received: $(cat "$scratch/drift.err")" >&2
  exit 1
}

if scripts/check-bench-runner.sh "$scratch/a.rs" "$scratch/missing.rs" >/dev/null 2>"$scratch/missing.err"; then
  echo "expected rejection of a missing runner copy, received success" >&2
  exit 1
fi
grep -q 'received no such file' "$scratch/missing.err" || {
  echo "expected the missing-file message, received: $(cat "$scratch/missing.err")" >&2
  exit 1
}
echo "bench runner checks passed"
