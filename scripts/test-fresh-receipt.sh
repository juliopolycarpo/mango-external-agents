#!/usr/bin/env bash
# Regression coverage for scripts/fresh-receipt.sh: the resolved lock is kept with its hash whenever
# it was produced, also when a later gate failed, and each way of failing gets its own result.
set -euo pipefail
cd "$(dirname "$0")/.."
script=$PWD/scripts/fresh-receipt.sh

work=$(mktemp -d "${TMPDIR:-/tmp}/fresh-receipt-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null

fail() { echo "$*" >&2; exit 1; }
sha256_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi
}

# A scratch repository whose committed lock differs from the "resolved" one.
repo=$work/repo
mkdir "$repo"
git -C "$repo" init -q
printf 'version = 4\n# committed\n' > "$repo/Cargo.lock"
git -C "$repo" add Cargo.lock
git -C "$repo" -c user.name=test -c user.email=test@example.invalid -c commit.gpgsign=false commit -q -m seed
printf 'version = 4\n# resolved\n' > "$repo/Cargo.lock"
resolved_hash=$(sha256_stdin < "$repo/Cargo.lock")
committed_hash=$(git -C "$repo" show HEAD:Cargo.lock | sha256_stdin)
printf '    Locking 2 packages to highest Rust 1.97 compatible versions\n    Updating foo v1.0.0 -> v1.1.0 (available: v1.2.0, requires Rust 1.98)\n' > "$work/update.log"

# run_case <name> <steps-json>: snapshot (when the resolution is claimed to have succeeded) and finish.
run_case() {
  local name=$1 steps=$2 dir=$work/$1
  export RECEIPT_DIR=$dir UPDATE_LOG=$work/update.log FRESH_ROOT=$repo STEPS_JSON=$steps
  export SOURCE_SHA=aaaa HEAD_SHA=bbbb EVENT_NAME=workflow_dispatch REF=refs/heads/x RUN_ID=1 RUN_ATTEMPT=1 REPOSITORY=o/r
  if [ "$(jq -r '.resolve.outcome' <<< "$steps")" = success ]; then
    "$script" snapshot
  fi
  "$script" finish > "$dir.out" 2>&1 || { cat "$dir.out" >&2; fail "case $name: expected finish to succeed, received the output above"; }
}

# expect <name> <jq filter> <expected>
expect() {
  local received
  received=$(jq -r "$2" "$work/$1/receipt.json")
  [ "$received" = "$3" ] || fail "case $1: expected $2 = '$3', received '$received'"
}

passed='{"resolve":{"outcome":"success"},"gate_clippy":{"outcome":"success"},"gate_nextest":{"outcome":"success"}}'
run_case passed "$passed"
expect passed .result passed
expect passed .lock.sha256 "$resolved_hash"
expect passed .lock.committed_sha256 "$committed_hash"
expect passed .lock.moved_from_committed true
expect passed .gates.nextest success
expect passed .source.head_sha bbbb
expect passed .resolver.log_line 'Locking 2 packages to highest Rust 1.97 compatible versions'
expect passed '.resolver.held_back_by_rust_version | length' 1
cmp "$repo/Cargo.lock" "$work/passed/Cargo.lock" || fail "case passed: expected the kept lock to equal the resolved lock"
[ "$(cut -d' ' -f1 "$work/passed/Cargo.lock.sha256")" = "$resolved_hash" ] || fail "case passed: expected Cargo.lock.sha256 to hold $resolved_hash"
[ -s "$work/passed/rustc-Vv.txt" ] || fail "case passed: expected a non-empty rustc-Vv.txt"
[ -f "$work/passed/cargo-update.log" ] || fail "case passed: expected cargo-update.log to be kept"

# A later gate fails: the lock is still kept, and the result says the gate failed, not the resolution.
failed='{"resolve":{"outcome":"success"},"gate_clippy":{"outcome":"failure"},"gate_nextest":{"outcome":"skipped"}}'
run_case gate_failure "$failed"
expect gate_failure .result gates-failed
expect gate_failure .resolution success
expect gate_failure .gates.clippy failure
expect gate_failure .lock.sha256 "$resolved_hash"
[ -f "$work/gate_failure/Cargo.lock" ] || fail "case gate_failure: expected the resolved lock to be kept after a failed gate"

# Resolution fails: no fresh lock exists, so none is kept even if a stale one is lying around.
broken='{"resolve":{"outcome":"failure"},"gate_clippy":{"outcome":"skipped"},"gate_nextest":{"outcome":"skipped"}}'
mkdir "$work/resolution_failure"
printf 'stale\n' > "$work/resolution_failure/Cargo.lock"
run_case resolution_failure "$broken"
expect resolution_failure .result resolution-failed
expect resolution_failure .lock null
[ ! -e "$work/resolution_failure/Cargo.lock" ] || fail "case resolution_failure: expected no Cargo.lock in the receipt, received one"

# A gate rewrites the lock after the snapshot: the kept lock is not what was tested.
export RECEIPT_DIR=$work/mutated UPDATE_LOG=$work/update.log FRESH_ROOT=$repo STEPS_JSON=$passed
"$script" snapshot
printf 'version = 4\n# rewritten by a gate\n' > "$repo/Cargo.lock"
"$script" finish > /dev/null
expect mutated .result lock-mutated
expect mutated .lock.unchanged_by_gates false
printf 'version = 4\n# resolved\n' > "$repo/Cargo.lock"

cancelled='{"resolve":{"outcome":"success"},"gate_clippy":{"outcome":"cancelled"}}'
run_case cancelled "$cancelled"
expect cancelled .result cancelled

# Cancelled after the snapshot but before any gate started: every gate is skipped, none failed.
early='{"resolve":{"outcome":"success"},"gate_clippy":{"outcome":"skipped"},"gate_nextest":{"outcome":"skipped"}}'
run_case cancelled_early "$early"
expect cancelled_early .result cancelled
expect cancelled_early .lock.sha256 "$resolved_hash"

# A real failure is not hidden by a later cancellation.
mixed='{"resolve":{"outcome":"success"},"gate_clippy":{"outcome":"failure"},"gate_nextest":{"outcome":"cancelled"}}'
run_case failed_then_cancelled "$mixed"
expect failed_then_cancelled .result gates-failed

# Cancelled while `cargo update` was still running: cancelled, and no lock is kept.
interrupted='{"resolve":{"outcome":"cancelled"},"gate_clippy":{"outcome":"skipped"}}'
run_case cancelled_resolution "$interrupted"
expect cancelled_resolution .result cancelled
expect cancelled_resolution .lock null

# Refusals name the value they received.
if out=$("$script" bogus 2>&1); then fail "expected rejection of mode 'bogus', received success"; fi
case "$out" in *"received 'bogus'"*) ;; *) fail "expected the diagnostic to name 'bogus', received: $out" ;; esac
echo 'fresh receipt checks passed'
