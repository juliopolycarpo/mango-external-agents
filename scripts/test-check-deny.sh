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

# Build each graph from local path packages so the test exercises the production selector without
# depending on registry state. The third package is a new patch in the exempted release line.
write_local_package() {
  local fixture=$1 package=$2 version=$3 directory=$4
  mkdir -p "$fixture/$directory/src"
  cat > "$fixture/$directory/Cargo.toml" <<PACKAGE
[package]
name = "$package"
version = "$version"
edition = "2021"
PACKAGE
  : > "$fixture/$directory/src/lib.rs"
}

check_skip_boundary() {
  local package=$1 approved=$2 other=$3 extra=$4
  local fixture="$work/$package" selector status
  mkdir -p "$fixture/src"
  selector=$(awk -v package="$package" '
    /^skip = \[$/ { in_skip = 1; next }
    in_skip && /^\]/ { in_skip = 0 }
    in_skip && $0 ~ "crate = \"" package "@" {
      line = $0
      sub(/^.*crate = "/, "", line)
      sub(/".*/, "", line)
      print line
      count++
    }
    END { if (count != 1) exit 1 }
  ' deny.toml) || {
    echo "deny.toml: expected one skip selector for $package, received: ${selector:-none or multiple}" >&2
    return 1
  }
  write_local_package "$fixture" "$package" "$approved" approved
  write_local_package "$fixture" "$package" "$other" other
  write_local_package "$fixture" "$package" "$extra" extra
  : > "$fixture/src/lib.rs"
  cat > "$fixture/deny.toml" <<POLICY
[bans]
multiple-versions = "deny"
skip = [{ crate = "$selector", reason = "production selector boundary test" }]
POLICY
  cat > "$fixture/Cargo.toml" <<MANIFEST
[package]
name = "skip-boundary-fixture"
version = "0.0.0"
edition = "2021"
[workspace]
exclude = ["approved", "other", "extra"]
[dependencies]
approved = { package = "$package", path = "approved" }
other = { package = "$package", path = "other" }
MANIFEST
  if ! (cd "$fixture" && cargo generate-lockfile --offline) > "$fixture/output" 2>&1; then
    echo "$package: expected local fixture lockfile, received:" >&2
    cat "$fixture/output" >&2
    return 1
  fi
  status=0
  (cd "$fixture" && cargo deny --color never --offline --locked check \
    -D unmatched-skip -D unnecessary-skip bans) > "$fixture/output" 2>&1 || status=$?
  if [ "$status" -ne 0 ] || ! grep -Fq 'bans ok' "$fixture/output"; then
    echo "$package: expected approved versions $approved and $other to pass, received exit $status:" >&2
    cat "$fixture/output" >&2
    return 1
  fi
  printf 'extra = { package = "%s", path = "extra" }\n' "$package" >> "$fixture/Cargo.toml"
  if ! (cd "$fixture" && cargo generate-lockfile --offline) > "$fixture/output" 2>&1; then
    echo "$package: expected local fixture lockfile including $extra, received:" >&2
    cat "$fixture/output" >&2
    return 1
  fi
  status=0
  (cd "$fixture" && cargo deny --color never --offline --locked check \
    -D unmatched-skip -D unnecessary-skip bans) > "$fixture/output" 2>&1 || status=$?
  if [ "$status" -ne 2 ] || ! grep -Fq 'error[duplicate]' "$fixture/output" || \
    ! grep -Fq "$package v$extra" "$fixture/output" || ! grep -Fq 'bans FAILED' "$fixture/output"; then
    echo "$package: expected exit 2 and error[duplicate] naming $package v$extra with bans FAILED, received exit $status:" >&2
    cat "$fixture/output" >&2
    return 1
  fi
}

boundary_failures=0
for versions in \
  'getrandom 0.2.17 0.4.3 0.2.18' \
  'windows-sys 0.52.0 0.61.2 0.52.1' \
  'thiserror 1.0.69 2.0.21 1.0.70' \
  'thiserror-impl 1.0.69 2.0.21 1.0.70' \
  'syn 2.0.119 3.0.10 2.0.120'; do
  # Each row is a fixed, whitespace-separated package and three versions.
  read -r package approved other extra <<< "$versions"
  check_skip_boundary "$package" "$approved" "$other" "$extra" || boundary_failures=$((boundary_failures + 1))
done
if [ "$boundary_failures" -ne 0 ]; then
  echo "skip boundaries: expected 0 failures, received $boundary_failures" >&2
  exit 1
fi
echo 'cargo-deny skips allow only the approved versions'
