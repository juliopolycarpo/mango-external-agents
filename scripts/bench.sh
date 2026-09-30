#!/usr/bin/env bash
# Runs the non-gating benchmarks and prints the environment a Base / Variant / Delta receipt needs.
# Not part of scripts/check.sh and not a CI job: numbers from a shared machine are evidence for a
# pull request, not a threshold. See docs/benchmarks.md.
#
# Usage: scripts/bench.sh [case-filter...]
#   BENCH_SAMPLES=25 scripts/bench.sh chunk-4KiB     # 25 samples of the matching cases
set -euo pipefail
cd "$(dirname "$0")/.."

# The Codex crate packages its own copy of the runner; a drifted copy would make the two crates'
# numbers incomparable.
core_runner=crates/mango-external-agents/benches/support/mod.rs
codex_runner=crates/mango-agent-codex/benches/support/mod.rs
if ! cmp -s "$core_runner" "$codex_runner"; then
  echo "expected $core_runner and $codex_runner to be identical, received a difference:" >&2
  diff -u "$core_runner" "$codex_runner" >&2 || true
  exit 1
fi

# sha256sum is GNU coreutils; macOS ships shasum instead. Neither present is a hard failure, so a
# receipt never silently omits the lockfile digest.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    echo "expected sha256sum or shasum on PATH to digest $1, received neither" >&2
    return 1
  fi
}
lockfile_digest=$(sha256_of Cargo.lock)

echo "## environment"
echo "date:        $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "commit:      $(git rev-parse HEAD)$(git diff --quiet HEAD -- 2>/dev/null || echo ' (+ uncommitted changes)')"
echo "lockfile:    $lockfile_digest"
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
