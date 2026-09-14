#!/usr/bin/env bash
# The local equivalent of the CI lane: format, lint, test, docs, policy.
# Usage: scripts/check.sh [--skip-format]
set -euo pipefail
cd "$(dirname "$0")/.."

skip_format=false
for arg in "$@"; do
  case "$arg" in
    --skip-format) skip_format=true ;;
    *) echo "unknown flag: $arg (expected --skip-format)" >&2; exit 2 ;;
  esac
done

run() { echo "▶ $*"; "$@"; }

if [ "$skip_format" = false ]; then
  run cargo fmt --all -- --check
  run dprint check
fi
run scripts/test-release.sh
run scripts/test-vendor-drift.sh
run cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
run cargo clippy --workspace --all-targets --no-default-features --locked -- -D warnings
run cargo nextest run --workspace --all-features --locked
run cargo test --doc --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" run cargo doc --no-deps --workspace --all-features --locked
run cargo deny check
run scripts/check-versions.sh
run scripts/check-tls.sh
echo "✓ check passed"
