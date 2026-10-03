#!/usr/bin/env bash
# Check the published feature contract on the declared minimum Rust version, one package at a time.
#
# `cargo check -p <crate>` resolves features for the selected package alone, so no other workspace
# member can switch a feature on behind its back. That is what makes this isolated coverage:
# `cargo build --workspace --no-default-features` is not, because the harness crates and `mea`
# depend on the core with `stdio` (and `launcher-tokio` for `mea`), which Cargo unifies into the
# one shared build. Library targets only, except for the final bare-core line: building a test or
# bench target adds the dev-dependencies' own features (tokio's `net`, `rt-multi-thread`), which
# would hide a missing feature on the library itself.
#
# The compiler is the workspace `rust-version`, not the development toolchain from
# rust-toolchain.toml. Extra arguments go to every cargo call (`--locked` for the committed lock;
# none for a freshly resolved one).
#
# Usage: scripts/check-msrv-features.sh [--print-toolchain] [cargo args...]
set -euo pipefail
cd "$(dirname "$0")/.."

rust_version=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^rust-version = "\([0-9][0-9]*\.[0-9][0-9]*\)"/\1/p}' Cargo.toml)
if [ -z "$rust_version" ]; then
  echo "expected [workspace.package] rust-version = \"1.x\" in Cargo.toml, received none" >&2
  exit 1
fi
toolchain="$rust_version.0"

if [ "${1:-}" = "--print-toolchain" ]; then
  echo "$toolchain"
  exit 0
fi

if ! rustc "+$toolchain" --version >/dev/null 2>&1; then
  echo "expected toolchain $toolchain installed (rustup toolchain install $toolchain --profile minimal), received none" >&2
  exit 1
fi
rustc "+$toolchain" --version --verbose

extra=("$@")
check() {
  echo "▶ cargo +$toolchain check $* ${extra[*]:-}"
  cargo "+$toolchain" check "$@" ${extra[@]+"${extra[@]}"}
}

# Core: nothing, each public feature alone, then everything. `stdio` is the default feature, so the
# bare line is the one a host that supplies its own transport and launcher compiles.
core=(-p mango-external-agents --no-default-features)
check "${core[@]}"
for feature in stdio websocket launcher-tokio testing; do
  check "${core[@]}" --features "$feature"
done
check -p mango-external-agents --all-features
check "${core[@]}" --all-targets

# Harness crates: the only public feature is `testing` (ACP), and each takes the core with `stdio`
# regardless, so one bare line per crate and the feature where it exists.
check -p mango-agent-claude --no-default-features
check -p mango-agent-codex --no-default-features
check -p mango-agent-acp --no-default-features
check -p mango-agent-acp --no-default-features --features testing
echo "✓ isolated feature checks passed on Rust $toolchain"
