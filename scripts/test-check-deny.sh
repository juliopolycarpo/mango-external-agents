#!/usr/bin/env bash
# Exercise stale skip lints and keep the local, CI and fresh-dependency gates aligned.
set -euo pipefail
cd "$(dirname "$0")/.."

for entry in \
  'scripts/check.sh|run cargo deny check -D unmatched-skip -D unnecessary-skip' \
  '.github/workflows/ci.yml|      - run: cargo deny check -D unmatched-skip -D unnecessary-skip' \
  '.github/workflows/fresh-dependencies.yml|        run: cargo deny --locked check -D unmatched-skip -D unnecessary-skip bans licenses sources'; do
  file=${entry%%|*}
  expected=${entry#*|}
  if ! grep -Fxq -- "$expected" "$file"; then
    echo "$file: expected cargo-deny gate '$expected', received:" >&2
    grep -F 'cargo deny' "$file" >&2 || true
    exit 1
  fi
done

work=$(mktemp -d "${TMPDIR:-/tmp}/deny-policy-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
for kind in unmatched-skip unnecessary-skip; do
  case "$kind" in
    unmatched-skip) dependency='definitely-nonexistent-crate@1' ;;
    unnecessary-skip) dependency='serde_json@1' ;;
  esac
  awk -v dependency="$dependency" '
    /^skip = \[$/ { print; print "  { crate = \"" dependency "\", reason = \"stale skip test\" },"; next }
    { print }
  ' deny.toml > "$work/deny.toml"
  status=0
  cargo deny --color never --offline --locked --config "$work/deny.toml" check \
    -D unmatched-skip -D unnecessary-skip bans > "$work/output" 2>&1 || status=$?
  if [ "$status" -ne 2 ] || ! grep -Fq "error[$kind]" "$work/output" || \
    ! grep -Fq "$dependency" "$work/output" || ! grep -Fq 'bans FAILED' "$work/output"; then
    echo "expected exit 2 and error[$kind] naming $dependency, received exit $status:" >&2
    cat "$work/output" >&2
    exit 1
  fi
done
echo 'cargo-deny gates reject unmatched and unnecessary skips'
