#!/usr/bin/env bash
# The local counterpart of CI's Linux lanes on the pinned toolchain: format, lint (including the
# depth-2 feature powerset), test, docs and policy. The powerset needs cargo-hack; without it the
# script warns once, skips that gate and returns the status of the others.
#
# Only CI covers: the macOS and Windows builds, the minimum Rust version (run it here with
# scripts/check-features.sh msrv --locked), the scheduled fresh-dependency and vendor-drift lanes,
# the pull request title check (scripts/check-pr-title.sh) and the release workflow's publication
# gate.
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
run scripts/test-prerelease-toolchain.sh
run scripts/test-prerelease-package.sh
run scripts/test-vendor-drift.sh
run scripts/test-package-admission.sh
run python3 scripts/test-package-fixtures.py
run scripts/package-fixtures.py --check
run scripts/check-dev-dependencies.sh
run python3 scripts/test-resolved-acp-schema.py
run python3 scripts/test-resolved-acp-schema-config.py
run python3 scripts/check-resolved-acp-schema.py
run scripts/check-bench-runner.sh
run scripts/test-bench-runner.sh
run scripts/test-consumer-pin.sh
run scripts/test-fresh-receipt.sh
run scripts/test-check-powerset.sh
run scripts/test-check-tls.sh
run scripts/check-standalone.sh
run cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
run cargo clippy --workspace --all-targets --no-default-features --locked -- -D warnings
# The same driver and lane as the `rust` job in ci.yml; scripts/test-check-powerset.sh pins the two together.
if cargo hack --version >/dev/null 2>&1; then
  run scripts/check-features.sh powerset --locked
else
  echo "warning: cargo-hack not found on PATH; skipped the depth-2 feature powerset that CI runs (cargo install cargo-hack --locked)" >&2
fi
run cargo nextest run --workspace --all-features --locked
run cargo test --doc --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" run cargo doc --no-deps --workspace --all-features --locked
run cargo deny check -D unmatched-skip -D unnecessary-skip
run scripts/test-check-deny.sh
run scripts/check-versions.sh
run scripts/check-tls.sh
echo "✓ check passed"
