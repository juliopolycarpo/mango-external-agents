#!/usr/bin/env bash
# Tests for scripts/prune-rust-toolchains.sh. Every case runs the real script against a named fake
# `rustup` (scripts/test-fixtures/fake-rustup-toolchains.sh) that keeps a toolchain list in a file
# and logs calls, so no toolchain on this machine is touched. The last case reads the workflows:
# every job that restores a rust-cache has to prune first, or its key follows the runner image.
# The workflow case needs PyYAML (apt: python3-yaml), like scripts/check-script-tests.sh.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
prune="$repo_root/scripts/prune-rust-toolchains.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/mea-prune-rust-toolchains-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

mkdir -p "$work/bin"
cp "$repo_root/scripts/test-fixtures/fake-rustup-toolchains.sh" "$work/bin/rustup"
export PATH="$work/bin:$PATH"
export FAKE_RUSTUP_LOG="$work/rustup.log"
export FAKE_RUSTUP_STATE="$work/toolchains"
export FAKE_RUSTUP_PINNED=1.100.0-x86_64-unknown-linux-gnu

fail() {
  echo "test failed: $*" >&2
  exit 1
}

# run_prune_with INSTALLED -- ARG... -> runs the script on a machine with the toolchains listed
# before `--`, leaving stdout, stderr and the status for the assertions.
run_prune_with() {
  : > "$FAKE_RUSTUP_LOG"
  : > "$FAKE_RUSTUP_STATE"
  while [ "$1" != -- ]; do
    printf '%s\n' "$1" >> "$FAKE_RUSTUP_STATE"
    shift
  done
  shift
  status=0
  "$prune" "$@" > "$work/out" 2> "$work/err" || status=$?
}

assert_status() {
  [ "$status" -eq "$1" ] || fail "expected exit status $1 | received: $status; stderr: $(cat "$work/err")"
}

# assert_toolchains NAME... -> the machine is left with exactly these, in order.
assert_toolchains() {
  local expected received
  expected=$(printf '%s\n' "$@")
  received=$(cat "$FAKE_RUSTUP_STATE")
  [ "$received" = "$expected" ] || fail "expected toolchains: $(tr '\n' ' ' <<<"$expected")| received: $(tr '\n' ' ' <<<"$received")"
}

assert_called() {
  grep -qxF -- "$1" "$FAKE_RUSTUP_LOG" || fail "expected the rustup call '$1' | received: $(tr '\n' ';' < "$FAKE_RUSTUP_LOG")"
}

assert_no_call_starting() {
  if grep -q -- "^$1" "$FAKE_RUSTUP_LOG"; then
    fail "expected no rustup call starting '$1' | received: $(tr '\n' ';' < "$FAKE_RUSTUP_LOG")"
  fi
}

pinned=$FAKE_RUSTUP_PINNED

# The image ships its own stable and not the pinned release: the pin is installed, stable goes,
# and the default follows the pin.
run_prune_with stable-x86_64-unknown-linux-gnu --
assert_status 0
assert_toolchains "$pinned"
assert_called 'toolchain install'
assert_called "default $pinned"
assert_called 'toolchain uninstall stable-x86_64-unknown-linux-gnu'

# Named toolchains stay, matched by release prefix; everything else goes in one call.
run_prune_with stable-x86_64-unknown-linux-gnu "$pinned" 1.98.0-x86_64-unknown-linux-gnu \
  1.96.0-x86_64-unknown-linux-gnu nightly-x86_64-unknown-linux-gnu -- 1.98.0 1.96.0
assert_status 0
assert_toolchains "$pinned" 1.98.0-x86_64-unknown-linux-gnu 1.96.0-x86_64-unknown-linux-gnu
assert_called 'toolchain uninstall stable-x86_64-unknown-linux-gnu nightly-x86_64-unknown-linux-gnu'

# A prefix has to end at the release: `1.9` names neither 1.98.0 nor 1.96.0, so it is reported as
# missing and nothing is removed.
run_prune_with stable-x86_64-unknown-linux-gnu "$pinned" 1.98.0-x86_64-unknown-linux-gnu -- 1.9
assert_status 1
grep -q 'expected toolchain 1.9 installed before pruning' "$work/err" || fail "expected the missing toolchain named | received: $(cat "$work/err")"
grep -q 'received: stable-x86_64-unknown-linux-gnu' "$work/err" || fail "expected the installed toolchains listed | received: $(cat "$work/err")"
assert_toolchains stable-x86_64-unknown-linux-gnu "$pinned" 1.98.0-x86_64-unknown-linux-gnu
assert_no_call_starting 'toolchain uninstall'
assert_no_call_starting 'default'

# Nothing surplus: no uninstall call at all, since rustup refuses one without a name.
run_prune_with "$pinned" --
assert_status 0
assert_toolchains "$pinned"
assert_no_call_starting 'toolchain uninstall'
grep -q "^toolchains: $pinned " "$work/out" || fail "expected the remaining toolchains reported | received: $(cat "$work/out")"

# A full toolchain name is kept as itself.
run_prune_with stable-x86_64-unknown-linux-gnu "$pinned" -- stable-x86_64-unknown-linux-gnu
assert_status 0
assert_toolchains stable-x86_64-unknown-linux-gnu "$pinned"

# rustup that cannot name the active toolchain stops the script before anything is removed.
FAKE_RUSTUP_NO_ACTIVE=1 run_prune_with stable-x86_64-unknown-linux-gnu --
assert_status 1
grep -q 'no active toolchain' "$work/err" || fail "expected rustup's own error on stderr | received: $(cat "$work/err")"
assert_no_call_starting 'toolchain uninstall'
assert_no_call_starting 'default'

# Every workflow job that runs rust-cache prunes first, and jobs that share a cache key prune to the
# same toolchains: a key is only shared when the toolchain sets are equal.
python3 - "$repo_root/.github/workflows" <<'PY' || fail "the workflows do not prune toolchains before every rust-cache step (see above)"
import pathlib
import re
import sys

import yaml

PRUNE = re.compile(r"scripts/prune-rust-toolchains\.sh([^\n;&|]*)")
problems = []
shared = {}
cache_steps = 0
for workflow in sorted(pathlib.Path(sys.argv[1]).glob("*.yml")):
    for job_id, job in yaml.safe_load(workflow.read_text(encoding="utf-8"))["jobs"].items():
        where = f"{workflow.name}:{job_id}"
        pruned = None
        for step in job.get("steps", []):
            match = PRUNE.search(str(step.get("run", "")))
            if match:
                pruned = match.group(1).split()
            if not str(step.get("uses", "")).startswith("Swatinem/rust-cache@"):
                continue
            cache_steps += 1
            if pruned is None:
                problems.append(
                    f"{where}: expected a scripts/prune-rust-toolchains.sh step before "
                    "Swatinem/rust-cache | received none"
                )
                continue
            key = (step.get("with") or {}).get("shared-key")
            if key:
                shared.setdefault(key, {})[where] = pruned
for key, jobs in shared.items():
    if len({tuple(arguments) for arguments in jobs.values()}) > 1:
        problems.append(f"shared-key {key}: expected one toolchain set in every job | received: {jobs}")
if cache_steps == 0:
    problems.append(f"expected at least one Swatinem/rust-cache step under {sys.argv[1]} | received none")
print("\n".join(problems), file=sys.stderr, end="\n" if problems else "")
sys.exit(1 if problems else 0)
PY

echo "✓ prune-rust-toolchains tests passed"
