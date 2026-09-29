#!/usr/bin/env bash
# Runs the non-gating benchmarks and prints the environment a Base / Variant / Delta receipt needs.
# Not part of scripts/check.sh and not a CI job: numbers from a shared machine are evidence for a
# pull request, not a threshold. See docs/benchmarks.md.
#
# Usage: scripts/bench.sh [case-filter...]
#   BENCH_SAMPLES=25 scripts/bench.sh chunk-4KiB     # 25 samples of the matching cases
set -euo pipefail
cd "$(dirname "$0")/.."

echo "## environment"
echo "date:        $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "commit:      $(git rev-parse HEAD)$(git diff --quiet HEAD -- 2>/dev/null || echo ' (+ uncommitted changes)')"
echo "lockfile:    $(sha256sum Cargo.lock | cut -d' ' -f1)"
echo "kernel:      $(uname -sr)"
if [ -r /proc/cpuinfo ]; then
  echo "cpu:         $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //') x$(nproc)"
fi
if [ -r /proc/loadavg ]; then
  echo "load (1/5/15 min, before the run): $(cut -d' ' -f1-3 /proc/loadavg)"
fi
echo "samples:     ${BENCH_SAMPLES:-15} per case (BENCH_SAMPLES)"
echo "toolchain:"
rustc -vV | sed 's/^/  /'
echo "features:    default features of each crate (no --features flag); dev-dependency features as in Cargo.toml"
echo

run() { echo "## $*"; "$@"; echo; }

run cargo bench --locked -p mango-external-agents --bench framing --bench events -- "$@"
run cargo bench --locked -p mango-agent-codex --bench pipeline -- "$@"
