#!/usr/bin/env bash
# Leaves rustup with exactly the toolchains a CI job asked for: the one rust-toolchain.toml pins,
# installed here when it is missing, and the ones named as arguments, which must already be
# installed. Every other toolchain is uninstalled and the pinned one becomes the default.
#
# Swatinem/rust-cache hashes `rustc -vV` of every installed toolchain into its key, the runner
# image's own `stable` included. That toolchain is not ours: when an image update moves it, every
# key changes and each job compiles from an empty cache, with no change in this repository. Run this
# before the cache step, in every job that has one; scripts/test-prune-rust-toolchains.sh pins that.
#
# The default is moved because the image's default is the toolchain just removed: a cargo call
# outside the checkout would have rustup install it again.
#
# A name matches an installed toolchain exactly or as its release prefix: `1.98.0` keeps
# `1.98.0-x86_64-unknown-linux-gnu`, and never `1.98.01-…`. Written for the bash 3.2 of the macOS
# runners: no `mapfile`, no expansion of an empty array.
# Usage: scripts/prune-rust-toolchains.sh [toolchain...]
set -euo pipefail
cd "$(dirname "$0")/.."

# With no toolchain named, rustup installs the active one: the channel, components and profile of
# rust-toolchain.toml.
rustup toolchain install >/dev/null
pinned=$(rustup show active-toolchain | awk 'NR == 1 { print $1 }')
if [ -z "$pinned" ]; then
  echo "expected rustup to name the toolchain rust-toolchain.toml pins, received none" >&2
  exit 1
fi

# One toolchain per line; `tr` drops the carriage returns rustup prints on Windows.
installed=$(rustup toolchain list --quiet | tr -d '\r')

# names WANTED NAME -> success when NAME is WANTED or WANTED is its release prefix.
names() { [ "$2" = "$1" ] || [ "${2#"$1"-}" != "$2" ]; }

# A named toolchain nobody installed is a step in the wrong order, and the cache key would be
# computed without it. Refuse before anything is removed.
for wanted in "$pinned" "$@"; do
  found=false
  while IFS= read -r name; do
    if names "$wanted" "$name"; then found=true; fi
  done <<<"$installed"
  if [ "$found" = false ]; then
    echo "expected toolchain $wanted installed before pruning (rustup toolchain install $wanted --profile minimal), received: $(tr '\n' ' ' <<<"$installed")" >&2
    exit 1
  fi
done

surplus=''
while IFS= read -r name; do
  [ -n "$name" ] || continue
  keep=false
  for wanted in "$pinned" "$@"; do
    if names "$wanted" "$name"; then keep=true; fi
  done
  if [ "$keep" = false ]; then surplus="$surplus $name"; fi
done <<<"$installed"

rustup default "$pinned" >/dev/null
if [ -n "$surplus" ]; then
  # Toolchain names carry no whitespace, so the list splits back into one argument each.
  # shellcheck disable=SC2086 # word splitting is the point
  rustup toolchain uninstall $surplus >/dev/null
  echo "uninstalled:$surplus"
fi
echo "toolchains: $(rustup toolchain list --quiet | tr -d '\r' | tr '\n' ' ')"
