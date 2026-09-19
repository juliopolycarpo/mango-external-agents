#!/usr/bin/env bash
# The publication gate, runnable before a tag exists.
#
# Release CI runs `cargo publish --workspace --dry-run` on the tag. That is the last moment a
# packaging fault can be found, and the first moment it costs a reverted release — a crate is
# immutable on crates.io once it lands, and the four here publish in lockstep, so one bad manifest
# strands the other three at a version nobody can depend on.
#
# This runs the same gate on any head, and adds the check the workflow's dry-run cannot make: that
# every file each crate actually ships is one the repository meant to ship. A dry-run proves the
# package builds; it does not tell you a fixture directory, a scratch file or a vendored schema
# silently joined the tarball.
#
# Usage: scripts/check-publish.sh [--list]   # --list also prints each crate's packaged files
set -euo pipefail
cd "$(dirname "$0")/.."

list=false
for arg in "$@"; do
  case "$arg" in
    --list) list=true ;;
    *) echo "unknown flag: $arg (expected --list)" >&2; exit 2 ;;
  esac
done

# Publication order, not alphabetical order: core first, then the three harnesses that depend on
# it. `cargo publish --workspace` resolves this itself, but naming it here is what makes the
# per-crate packaging loop below meaningful when one of them fails.
#
# Read from the release workflow rather than repeated, so the list a tag publishes and the list
# this gate checks cannot drift apart — which is the failure the gate exists to catch, and one it
# would be unable to see with a copy of its own.
read -r -a crates <<< "$(
  sed -n 's/^  CRATES: //p' .github/workflows/release.yml
)"
if [ "${#crates[@]}" -eq 0 ]; then
  echo "expected a 'CRATES:' list in .github/workflows/release.yml, received none" >&2
  exit 1
fi

run() { echo "▶ $*"; "$@"; }

echo "== every publishable package builds from its own tarball =="
run cargo publish --workspace --dry-run --locked

echo
echo "== every crate that claims to be published is one of the four =="
declared=$(
  for manifest in crates/*/Cargo.toml examples/*/Cargo.toml; do
    grep -q '^publish = false' "$manifest" && continue
    sed -n 's/^name = "\(.*\)"/\1/p' "$manifest" | head -1
  done | sort
)
expected=$(printf '%s\n' "${crates[@]}" | sort)
if [ "$declared" != "$expected" ]; then
  # `sed` rather than `printf '  %s\n'`: both values are newline-separated lists in one variable,
  # so printf indents the first entry and leaves the rest flush, which reads as a one-item list.
  echo "expected exactly these publishable crates:" >&2
  printf '%s\n' "$expected" | sed 's/^/  /' >&2
  echo "received:" >&2
  printf '%s\n' "$declared" | sed 's/^/  /' >&2
  echo "a new member either takes 'publish = false' or joins the lockstep set deliberately" >&2
  exit 1
fi
echo "publishable crates: $(printf '%s ' "${crates[@]}")"

echo
echo "== no crate ships a file the repository did not mean to ship =="
status=0
for crate in "${crates[@]}"; do
  # Not silenced: `cargo package --list` refuses a dirty tree, and swallowing that would leave
  # every check below reading an empty file list and reporting a missing README instead of the
  # uncommitted change that actually caused it.
  if ! files=$(cargo package -p "$crate" --locked --list); then
    echo "$crate: expected cargo to list the package contents, received the failure above" >&2
    exit 1
  fi
  if [ "$list" = true ]; then
    echo "--- $crate"
    printf '%s\n' "$files" | sed 's/^/    /'
  fi
  # A packaged tarball is the public artifact. These are the things that have no business in one:
  # a capture workspace, an editor or agent scratch directory, or an environment file.
  stowaways=$(printf '%s\n' "$files" | grep -E '(^|/)(\.env|\.envrc|target/|node_modules/|\.claude/|\.codex/|scratch/)' || true)
  if [ -n "$stowaways" ]; then
    echo "$crate: expected no build, scratch or environment paths in the package, received:" >&2
    printf '  %s\n' "$stowaways" >&2
    status=1
  fi
  # Every published crate owes a README and a licence, because crates.io renders one and the
  # other is the file a downstream legal review looks for first.
  for owed in README.md LICENSE; do
    printf '%s\n' "$files" | grep -qx "$owed" || {
      echo "$crate: expected $owed in the package, received a tarball without it" >&2
      status=1
    }
  done
done
[ $status -eq 0 ] || exit $status

echo
echo "✓ publication dry-run passed for ${#crates[@]} crates"
