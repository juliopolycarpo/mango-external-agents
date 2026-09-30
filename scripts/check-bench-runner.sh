#!/usr/bin/env bash
# The benchmark runner is copied into each crate that has benches, because a published crate
# cannot package a file from another crate. A drifted copy would make two crates' numbers
# incomparable, so the copies must stay byte-identical.
# Usage: scripts/check-bench-runner.sh [core-runner codex-runner]   # paths default to the repo's
set -euo pipefail
cd "$(dirname "$0")/.."

core_runner=${1:-crates/mango-external-agents/benches/support/mod.rs}
codex_runner=${2:-crates/mango-agent-codex/benches/support/mod.rs}

for runner in "$core_runner" "$codex_runner"; do
  if [ ! -f "$runner" ]; then
    echo "expected a bench runner at $runner, received no such file" >&2
    exit 1
  fi
done
if ! cmp -s "$core_runner" "$codex_runner"; then
  echo "expected $core_runner and $codex_runner to be identical, received a difference:" >&2
  diff -u "$core_runner" "$codex_runner" >&2 || true
  exit 1
fi
echo "bench runner copies are identical"
