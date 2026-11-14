#!/usr/bin/env bash
# The one driver for the published feature contract of the four library crates. A lane fixes the
# toolchain and the target set; the crate list lives here and nowhere else, so a new crate is one
# edit and a new feature needs none (cargo-hack reads the features from each manifest).
#
#   powerset  the development toolchain from rust-toolchain.toml. Clippy over every feature pair
#             (depth 2) of each crate, all targets, warnings denied. Run by CI's `rust` job and
#             scripts/check.sh.
#   msrv      the workspace `rust-version`, not the development toolchain. A compile check of
#             library targets, one package at a time: the bare crate and each feature alone, plus the
#             core with all features and the bare core with all targets. Run by CI's `msrv` job, the
#             fresh-dependencies lane and the release verify job.
#
# `cargo hack` resolves each run for one package, so no other workspace member can switch a feature
# on behind its back. That is what makes the msrv lane isolated coverage:
# `cargo build --workspace --no-default-features` is not, because the harness crates and `mea`
# depend on the core with `stdio` (and `launcher-tokio` for `mea`), which Cargo unifies into the
# one shared build. The msrv lane checks library targets only, except for the final bare-core line:
# building a test or bench target adds the dev-dependencies' own features (tokio's `net`,
# `rt-multi-thread`), which would hide a missing feature on the library itself.
#
# Extra arguments go to every cargo call (`--locked` for the committed lock; none for a freshly
# resolved one). A missing cargo-hack or toolchain is an error here; only scripts/check.sh chooses
# to skip the powerset when cargo-hack is absent.
#
# Usage: scripts/check-features.sh (powerset|msrv) [cargo args...]
#        scripts/check-features.sh --print-toolchain
set -euo pipefail
cd "$(dirname "$0")/.."

# Every published library crate. scripts/test-check-powerset.sh pins this list independently.
library_crates=(mango-external-agents mango-agent-claude mango-agent-codex mango-agent-acp)
core_crate=mango-external-agents

usage="usage: scripts/check-features.sh (powerset|msrv) [cargo args...] | --print-toolchain"

# The declared minimum Rust, as a full toolchain name (`1.97` in Cargo.toml is `1.97.0`).
msrv_toolchain() {
  local rust_version
  rust_version=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^rust-version = "\([0-9][0-9]*\.[0-9][0-9]*\)"/\1/p;}' Cargo.toml)
  if [ -z "$rust_version" ]; then
    echo "expected [workspace.package] rust-version = \"1.x\" in Cargo.toml, received none" >&2
    exit 1
  fi
  echo "$rust_version.0"
}

lane=${1:-}
if [ -z "$lane" ]; then
  echo "expected a lane (powerset|msrv) or --print-toolchain, received none; $usage" >&2
  exit 2
fi
shift

extra=("$@")

if [ "$lane" = "--print-toolchain" ]; then
  msrv_toolchain
  exit 0
fi

case "$lane" in
  powerset | msrv) ;;
  *)
    echo "expected lane powerset|msrv or --print-toolchain, received: $lane; $usage" >&2
    exit 2
    ;;
esac

if ! cargo hack --version >/dev/null 2>&1; then
  echo "expected cargo-hack on PATH for the $lane lane, received none (cargo install cargo-hack --locked)" >&2
  exit 1
fi

packages=()
for crate in "${library_crates[@]}"; do
  packages+=(-p "$crate")
done

run() { echo "▶ $*"; "$@"; }

if [ "$lane" = powerset ]; then
  run cargo hack clippy --feature-powerset --depth 2 --all-targets ${extra[@]+"${extra[@]}"} "${packages[@]}" -- -D warnings
  exit 0
fi

toolchain=$(msrv_toolchain)
if ! rustc "+$toolchain" --version >/dev/null 2>&1; then
  echo "expected toolchain $toolchain installed (rustup toolchain install $toolchain --profile minimal), received none" >&2
  exit 1
fi
rustc "+$toolchain" --version --verbose

# Every package: bare (`--no-default-features`) and each feature alone. `default` is skipped: it
# is empty for the harness crates and `stdio` for the core, which the `stdio` run already covers.
# The all-features run is skipped here and made only for the core on the next line. The bare core
# is the build a host that supplies its own transport and launcher compiles.
run cargo "+$toolchain" hack check --each-feature --skip default --exclude-all-features ${extra[@]+"${extra[@]}"} "${packages[@]}"
run cargo "+$toolchain" check -p "$core_crate" --all-features ${extra[@]+"${extra[@]}"}
run cargo "+$toolchain" check -p "$core_crate" --no-default-features --all-targets ${extra[@]+"${extra[@]}"}
echo "✓ isolated feature checks passed on Rust $toolchain"
