#!/usr/bin/env bash
# The benchmark runner is copied into each crate that has benches, because a published crate
# cannot package a file from another crate. A drifted copy would make two crates' numbers
# incomparable, so the copies must stay byte-identical.
# Usage: scripts/check-bench-runner.sh [reference-runner other-runner...]
#   with no arguments, the core runner is the reference and the Codex and ACP copies are compared
#   to it.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$#" -eq 0 ]; then
  set -- \
    crates/mango-external-agents/benches/support/mod.rs \
    crates/mango-agent-codex/benches/support/mod.rs \
    crates/mango-agent-acp/benches/support/mod.rs
elif [ "$#" -lt 2 ]; then
  echo "expected a reference runner and at least one copy to compare, received $# path(s): $*" >&2
  exit 1
fi

for runner in "$@"; do
  if [ ! -f "$runner" ]; then
    echo "expected a bench runner at $runner, received no such file" >&2
    exit 1
  fi
done

reference=$1
shift
for copy in "$@"; do
  if ! cmp -s "$reference" "$copy"; then
    echo "expected $reference and $copy to be identical, received a difference:" >&2
    diff -u "$reference" "$copy" >&2 || true
    exit 1
  fi
done
echo "bench runner copies are identical"
