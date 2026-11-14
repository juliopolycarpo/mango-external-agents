#!/usr/bin/env bash
# Runs a command in a scratch copy of the workspace without the crates' self dev-dependency.
#
# Each library crate lists itself as a path dev-dependency with its test features, so the bare
# `cargo nextest run -p <crate>` runs the gated targets. Once a test, example or bench target is
# selected, cargo unifies those features into every target of the crate, library included:
# `cargo clippy --all-targets --no-default-features` builds the library with `testing` on. A
# feature lane that selects those targets would pass while checking no reduced-feature library or
# test target. docs/testing.md has the probe.
#
# Cargo has no switch to ignore one dev-dependency. This script removes the entry from the copy's
# manifests and the matching self edges from the copy's Cargo.lock; the committed tree is never
# edited. The copy lives at a fixed path under the target directory and is removed on exit, so
# repeated runs reuse the build artifacts of the workspace crates and of the registry dependencies.
#
# Usage: scripts/run-without-self-dev-dependency.sh command [args...]
set -euo pipefail
cd "$(dirname "$0")/.."
repo=$PWD

if [ "$#" -eq 0 ]; then
  echo "expected a command to run in the copy without the self dev-dependency, received none" >&2
  exit 2
fi

target_dir=${CARGO_TARGET_DIR:-$repo/target}
mkdir -p "$target_dir"
target_dir=$(cd "$target_dir" && pwd)
copy=$target_dir/without-self-dev-dependency
rm -rf -- "$copy"
mkdir -p "$copy"
trap 'rm -rf -- "$copy"' EXIT

# Tracked and untracked-but-not-ignored files, as they are on disk now. A tracked file deleted in
# the working tree is skipped.
while IFS= read -r -d '' file; do
  [ -e "$file" ] || continue
  mkdir -p "$copy/$(dirname "$file")"
  cp -p "$file" "$copy/$file"
done < <(git ls-files -z --cached --others --exclude-standard)

# `name = { path = ".", ... }` in a crate manifest is the self dev-dependency: no other entry
# points at the crate's own directory.
stripped=0
names=()
for manifest in "$copy"/crates/*/Cargo.toml; do
  [ -f "$manifest" ] || continue
  if grep -Eq '^[A-Za-z0-9_-]+ = \{ path = "\.", ' "$manifest"; then
    stripped=$((stripped + 1))
    names+=("$(basename "$(dirname "$manifest")")")
    grep -Ev '^[A-Za-z0-9_-]+ = \{ path = "\.", ' "$manifest" > "$manifest.stripped"
    mv "$manifest.stripped" "$manifest"
  fi
done
# An entry in another shape (keys reordered, a table header) survives the strip above and would
# keep widening the copy.
leftover=$(grep -En 'path = "\."' "$copy"/crates/*/Cargo.toml || true)
if [ -n "$leftover" ]; then
  echo "expected every self dev-dependency to be written as name = { path = \".\", features = [...] }, received: $leftover" >&2
  exit 1
fi
if [ "$stripped" -eq 0 ]; then
  echo "expected at least one crate manifest with a self dev-dependency (name = { path = \".\", ... }), received none" >&2
  exit 1
fi

# Re-resolve the copy's lock without the stripped entries. Offline and with the lock in place,
# cargo only drops the self edges. Any other difference means a version moved, which is refused
# below. A crate whose only dependency was itself also loses its empty `dependencies = [ ]` lines.
host=$(rustc -vV | sed -n 's/^host: //p')
if [ -z "$host" ]; then
  echo "expected rustc -vV to print a host triple, received none" >&2
  exit 1
fi
# `--filter-platform` stops offline cargo from asking for the crates of other platforms, which a
# cargo home that only built for this host has not downloaded.
(cd "$copy" && cargo metadata --format-version 1 --offline --filter-platform "$host" > /dev/null)
unexpected=$(diff "$repo/Cargo.lock" "$copy/Cargo.lock" | grep -E '^[<>]' | grep -Ev "^< +\"($(IFS='|'; echo "${names[*]}"))\",\$|^< dependencies = \[\$|^< \]\$" || true)
if [ -n "$unexpected" ]; then
  echo "expected the copy's Cargo.lock to differ from the committed one only by removed self edges, received: $unexpected" >&2
  exit 1
fi

export CARGO_TARGET_DIR=$target_dir
echo "▶ in a copy without the self dev-dependency of $stripped crate(s): $*"
cd "$copy"
"$@"
