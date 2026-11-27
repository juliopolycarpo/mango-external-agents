#!/usr/bin/env bash
# Tests for the input comparison of scripts/check-standalone.sh. Every case runs the real script in
# a throwaway git repository that holds a copy of both registry consumers, against a named fake
# `cargo` and `rustc` (scripts/test-fixtures/fake-standalone-cargo.sh) that log calls and compile
# nothing. A consumer was built when the log has a `cargo test` call for its scratch copy.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/mea-check-standalone-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

mkdir -p "$work/bin" "$work/tmp"
cp "$repo_root/scripts/test-fixtures/fake-standalone-cargo.sh" "$work/bin/cargo"
cp "$repo_root/scripts/test-fixtures/fake-standalone-cargo.sh" "$work/bin/rustc"
export PATH="$work/bin:$PATH"
export FAKE_CARGO_LOG="$work/cargo.log"
export FAKE_CARGO_PIN=9.9.9
# The script's scratch copies go here, so a log line names the consumer by its directory.
export TMPDIR="$work/tmp"
unset CARGO_TARGET_DIR

fail() {
  echo "test failed: $*" >&2
  exit 1
}

# A repository with what the script reads: itself, the toolchain file, the two compared fixtures
# and both consumers, each pinned to the release the fake cargo reports.
tree="$work/tree"
mkdir -p "$tree/scripts" "$tree/fixtures/claude/help" "$tree/fixtures/claude/transcripts"
cp "$repo_root/scripts/check-standalone.sh" "$tree/scripts/"
cp "$repo_root/rust-toolchain.toml" "$repo_root/Cargo.toml" "$tree/"
cp "$repo_root/fixtures/claude/help/2.1.260.txt" "$tree/fixtures/claude/help/"
cp "$repo_root/fixtures/claude/transcripts/read-turn.jsonl" "$tree/fixtures/claude/transcripts/"
for name in current historical; do
  mkdir -p "$tree/tests/standalone-$name"
  cp -R "$repo_root/tests/standalone-$name/." "$tree/tests/standalone-$name/"
  sed -i.bak 's/^\(mango-[a-z-]* = { version = "=\)[^"]*/\1'"$FAKE_CARGO_PIN"'/' "$tree/tests/standalone-$name/Cargo.toml"
  rm "$tree/tests/standalone-$name/Cargo.toml.bak"
done
git -C "$tree" init --quiet
git -C "$tree" -c user.name=test -c user.email=test@example.invalid -c commit.gpgsign=false \
  commit --quiet --allow-empty -m empty
git -C "$tree" add -A
commit() {
  git -C "$tree" add -A
  git -C "$tree" -c user.name=test -c user.email=test@example.invalid -c commit.gpgsign=false \
    commit --quiet -m "$1"
}
commit base
base=$(git -C "$tree" rev-parse HEAD)

# run_check ARG... -> runs the copied script, leaving stdout, stderr and the status for assertions.
run_check() {
  : > "$FAKE_CARGO_LOG"
  status=0
  "$tree/scripts/check-standalone.sh" "$@" > "$work/out" 2> "$work/err" || status=$?
}

assert_status() {
  [ "$status" -eq "$1" ] || fail "expected exit status $1 | received: $status; stdout: $(cat "$work/out"); stderr: $(cat "$work/err")"
}

# built NAME -> success when the consumer's scratch copy was tested.
built() { grep -Eq "^cargo test --manifest-path .*/$1/Cargo.toml" "$FAKE_CARGO_LOG"; }

assert_built() {
  local name
  for name in "$@"; do
    built "$name" || fail "expected the $name consumer built | received cargo calls: $(tr '\n' ';' < "$FAKE_CARGO_LOG")"
  done
}

assert_not_built() {
  local name
  for name in "$@"; do
    if built "$name"; then
      fail "expected the $name consumer not built | received cargo calls: $(tr '\n' ';' < "$FAKE_CARGO_LOG")"
    fi
  done
}

# reset_tree -> back to the base commit with no untracked file.
reset_tree() {
  git -C "$tree" reset --quiet --hard "$base"
  git -C "$tree" clean --quiet -fd
}

# Without the flag both consumers are built: the weekly CI run and a bare local call.
run_check
assert_status 0
assert_built current historical

# Unchanged since the commit: neither is built, and each line says what was compared.
run_check --changed-since "$base"
assert_status 0
assert_not_built current historical
grep -q "current registry consumer (=$FAKE_CARGO_PIN): inputs unchanged since $base (tests/standalone-current rust-toolchain.toml scripts/check-standalone.sh); not rebuilt" "$work/out" \
  || fail "expected the skip to name the pin, the commit and the inputs | received: $(cat "$work/out")"

# The comparison of the checkout's fixtures with the consumer's copies still runs when the build
# is skipped: the root fixture is not a build input, and a drifted copy has to fail.
printf 'drift\n' >> "$tree/fixtures/claude/help/2.1.260.txt"
run_check --changed-since "$base"
assert_status 1
assert_not_built current historical
reset_tree

# So does the pin shape.
sed -i.bak 's/^mango-external-agents = { version = "=[^"]*"/mango-external-agents = { version = "9"/' "$tree/tests/standalone-current/Cargo.toml"
rm "$tree/tests/standalone-current/Cargo.toml.bak"
run_check --changed-since "$base"
assert_status 1
grep -q "expected one exact '=x.y.z' mango-external-agents pin, received: mango-external-agents = { version = \"9\"" "$work/err" || fail "expected the pin shape refused | received: $(cat "$work/err")"
reset_tree

# A committed change to one consumer builds that one only.
printf '\n// touched\n' >> "$tree/tests/standalone-current/src/lib.rs"
commit 'touch the current consumer'
run_check --changed-since "$base"
assert_status 0
assert_built current
assert_not_built historical
reset_tree

# So does an uncommitted change, and an untracked file.
printf '\n// touched\n' >> "$tree/tests/standalone-historical/src/lib.rs"
run_check --changed-since "$base"
assert_status 0
assert_built historical
assert_not_built current
reset_tree
: > "$tree/tests/standalone-historical/tests/untracked.rs"
run_check --changed-since "$base"
assert_status 0
assert_built historical
assert_not_built current
reset_tree

# The toolchain file and the script are inputs of both consumers.
for shared in rust-toolchain.toml scripts/check-standalone.sh; do
  printf '\n# touched\n' >> "$tree/$shared"
  run_check --changed-since "$base"
  assert_status 0
  assert_built current historical
  reset_tree
done

# A caller's own input counts only when it is named.
printf '\n# touched\n' >> "$tree/Cargo.toml"
run_check --changed-since "$base"
assert_status 0
assert_not_built current historical
run_check --changed-since "$base" --also-input Cargo.toml --test-only current
assert_status 0
assert_built current
grep -q '^cargo clippy' "$FAKE_CARGO_LOG" && fail "expected --test-only to skip clippy | received: $(tr '\n' ';' < "$FAKE_CARGO_LOG")"
reset_tree

# A commit this clone does not have cannot be compared, so the consumers are built and the output
# says why. The all-zero id is what a first push reports as its previous commit.
for unknown in 0000000000000000000000000000000000000000 not-a-revision; do
  run_check --changed-since "$unknown"
  assert_status 0
  assert_built current historical
  grep -q "current registry consumer: $unknown is not a commit in this clone, so its inputs cannot be compared; building" "$work/out" \
    || fail "expected the unknown commit $unknown reported | received: $(cat "$work/out")"
done

# A flag without its value is a usage error that names the flag.
for flag in --changed-since --also-input; do
  run_check "$flag"
  assert_status 2
  grep -q "expected a value after $flag, received none" "$work/err" || fail "expected the missing value of $flag reported | received: $(cat "$work/err")"
  run_check "$flag" ''
  assert_status 2
done
run_check --unknown
assert_status 2
grep -q 'unknown argument: --unknown' "$work/err" || fail "expected the unknown argument named | received: $(cat "$work/err")"

echo "✓ check-standalone tests passed"
