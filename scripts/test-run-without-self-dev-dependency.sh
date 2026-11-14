#!/usr/bin/env bash
# Regression coverage for scripts/run-without-self-dev-dependency.sh.
#
# A throwaway workspace stands in for the repository: one crate, `selfish`, lists itself as a
# dev-dependency with its `extra` feature, and its library refuses to compile without that feature.
# That is the shape of the probe in docs/testing.md. The first case shows the problem the script
# exists for: `cargo check --all-targets` in the real tree passes with the feature off, because
# the self dev-dependency widens the library. The next shows the fix: the same command through the
# script fails on the probe. The rest pin the copy, the lock and the exit status.
#
# The workspace has no registry dependency, so the cases need no network and a few seconds of
# cargo.
set -euo pipefail
cd "$(dirname "$0")/.."
helper_source=$PWD/scripts/run-without-self-dev-dependency.sh

work=$(mktemp -d "${TMPDIR:-/tmp}/self-dev-dependency-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

fail() { echo "$*" >&2; exit 1; }

repo=$work/repo
mkdir -p "$repo/scripts" "$repo/crates/selfish/src" "$repo/crates/selfish/tests"
cp "$helper_source" "$repo/scripts/"
cat > "$repo/Cargo.toml" <<'MANIFEST'
[workspace]
resolver = "3"
members = ["crates/*"]
MANIFEST
cat > "$repo/crates/selfish/Cargo.toml" <<'MANIFEST'
[package]
name = "selfish"
version = "0.1.0"
edition = "2024"

[features]
default = []
extra = []

[dev-dependencies]
selfish = { path = ".", features = ["extra"] }
MANIFEST
# The probe: the library only compiles with `extra`, which only the self dev-dependency enables.
cat > "$repo/crates/selfish/src/lib.rs" <<'SOURCE'
#[cfg(not(feature = "extra"))]
compile_error!("probe: expected feature extra on, received it off");
SOURCE
printf '#[test]\nfn builds() {}\n' > "$repo/crates/selfish/tests/builds.rs"
printf '/target\n' > "$repo/.gitignore"

cd "$repo"
git init --quiet
cargo generate-lockfile --offline --quiet
git add .
lock_before=$(cat Cargo.lock)
manifest_before=$(cat crates/selfish/Cargo.toml)
manifest_file=$work/selfish.toml
cp crates/selfish/Cargo.toml "$manifest_file"
if ! grep -q '^ "selfish",$' Cargo.lock; then
  fail "expected the fixture Cargo.lock to record the self edge, received: $lock_before"
fi

# The problem: with a dev target selected, the entry turns the feature on for the library too.
if ! cargo check --offline --all-targets --no-default-features > "$work/widened.out" 2>&1; then
  fail "expected the in-tree --all-targets --no-default-features check to pass, widened by the self dev-dependency | received: $(cat "$work/widened.out")"
fi

# The fix: through the script, the library is built with the feature off and the probe fires.
status=0
scripts/run-without-self-dev-dependency.sh cargo check --offline --locked --all-targets --no-default-features > "$work/copy.out" 2>&1 || status=$?
if [ "$status" -eq 0 ] || ! grep -q 'probe: expected feature extra on, received it off' "$work/copy.out"; then
  fail "expected the check through the script to fail on the probe | received exit $status: $(cat "$work/copy.out")"
fi
# The same command with the feature on passes, so the failure above is the feature and not the copy.
if ! scripts/run-without-self-dev-dependency.sh cargo check --offline --locked --all-targets --features extra > "$work/featured.out" 2>&1; then
  fail "expected the check with --features extra through the script to pass | received: $(cat "$work/featured.out")"
fi

# The command runs in a copy: the manifest has no self entry and the lock has no self edge.
printf 'untracked\n' > crates/selfish/notes.txt
printf 'removed\n' > crates/selfish/gone.txt
git add crates/selfish/gone.txt
rm crates/selfish/gone.txt
scripts/run-without-self-dev-dependency.sh sh -c 'pwd; cat crates/selfish/Cargo.toml; cat Cargo.lock; ls crates/selfish' > "$work/inspect.out" 2> "$work/inspect.err" ||
  fail "expected the inspection command to pass | received: $(cat "$work/inspect.err")"
copy_dir=$(head -n 1 "$work/inspect.out")
if [ "$copy_dir" = "$repo" ] || [ -z "$copy_dir" ]; then
  fail "expected the command to run in a scratch copy, received working directory: $copy_dir"
fi
if grep -q 'path = "\."' "$work/inspect.out"; then
  fail "expected the copy's manifest to lose the self dev-dependency | received: $(cat "$work/inspect.out")"
fi
if grep -q '^ "selfish",$' "$work/inspect.out"; then
  fail "expected the copy's Cargo.lock to lose the self edge | received: $(cat "$work/inspect.out")"
fi
grep -q '^notes.txt$' "$work/inspect.out" || fail "expected an untracked file to be copied | received: $(cat "$work/inspect.out")"
if grep -q '^gone.txt$' "$work/inspect.out"; then
  fail "expected a tracked file deleted on disk to stay out of the copy | received: $(cat "$work/inspect.out")"
fi
if [ -e "$copy_dir" ]; then
  fail "expected the scratch copy to be removed afterwards, received: $copy_dir"
fi

# The real tree is untouched.
[ "$(cat Cargo.lock)" = "$lock_before" ] || fail "expected the committed Cargo.lock unchanged | received: $(cat Cargo.lock)"
[ "$(cat crates/selfish/Cargo.toml)" = "$manifest_before" ] || fail "expected the committed manifest unchanged | received: $(cat crates/selfish/Cargo.toml)"

# The command's exit status is the script's.
status=0
scripts/run-without-self-dev-dependency.sh sh -c 'exit 7' > /dev/null 2>&1 || status=$?
[ "$status" -eq 7 ] || fail "expected the command's exit status 7, received $status"

# No command is a usage error.
status=0
scripts/run-without-self-dev-dependency.sh > /dev/null 2> "$work/usage.err" || status=$?
if [ "$status" -ne 2 ] || ! grep -q 'expected a command .* received none' "$work/usage.err"; then
  fail "expected no command to exit 2 naming the missing command | received exit $status: $(cat "$work/usage.err")"
fi

# A self entry the strip cannot match would keep widening the copy: refuse it by name.
printf '%s\n' 'selfish = { features = ["extra"], path = "." }' >> crates/selfish/Cargo.toml
status=0
scripts/run-without-self-dev-dependency.sh true > /dev/null 2> "$work/reordered.err" || status=$?
if [ "$status" -ne 1 ] || ! grep -Fq 'selfish = { features = ["extra"], path = "." }' "$work/reordered.err"; then
  fail "expected a reordered self dev-dependency to exit 1 naming the entry | received exit $status: $(cat "$work/reordered.err")"
fi
cp "$manifest_file" crates/selfish/Cargo.toml

# A workspace with no self dev-dependency would run unchanged and prove nothing: refuse it.
grep -v '^selfish = ' crates/selfish/Cargo.toml > "$work/plain.toml"
cp "$work/plain.toml" crates/selfish/Cargo.toml
status=0
scripts/run-without-self-dev-dependency.sh true > /dev/null 2> "$work/none.err" || status=$?
if [ "$status" -ne 1 ] || ! grep -q 'expected at least one crate manifest with a self dev-dependency' "$work/none.err"; then
  fail "expected a workspace without the entry to exit 1 naming it | received exit $status: $(cat "$work/none.err")"
fi

echo 'run-without-self-dev-dependency.sh checks passed'
