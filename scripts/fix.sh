#!/usr/bin/env bash
# Apply every auto-fix the checks would complain about.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all
dprint fmt
cargo clippy --workspace --all-targets --all-features --fix --allow-dirty --allow-staged -- -D warnings
