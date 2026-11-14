#!/usr/bin/env bash
# Regression coverage for scripts/check-script-tests.sh: every scripts/test-*.{sh,py} must run in
# both scripts/check.sh and .github/workflows/ci.yml. Each case builds a small scratch repository
# (named fixtures, no network, no cargo) and runs the real check against it.
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d "${TMPDIR:-/tmp}/check-script-tests-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
checker="$PWD/scripts/check-script-tests.sh"

fail() { echo "$*" >&2; exit 1; }

# FixtureRepo: scripts/ holds the named test files plus a vendor-drift parent that names its two
# Python children and an admission test that names test-packages.sh; check.sh and ci.yml run the
# files listed in $1 and $2.
make_repo() {
  local repo=$1 in_check=$2 in_ci=$3 name
  rm -rf "$repo"
  mkdir -p "$repo/scripts" "$repo/.github/workflows"
  for name in test-alpha.sh test-beta.py test-vendor-drift.sh test-package-admission.sh test-codex-schema-inventory.py test-vendor-result.py test-packages.sh; do
    : > "$repo/scripts/$name"
  done
  printf 'python3 scripts/test-codex-schema-inventory.py\npython3 scripts/test-vendor-result.py\n' > "$repo/scripts/test-vendor-drift.sh"
  printf 'cp scripts/test-packages.sh "$work/"\n' > "$repo/scripts/test-package-admission.sh"
  {
    echo '# test-ghost.sh is only mentioned in a comment'
    for name in $in_check; do echo "run scripts/$name"; done
  } > "$repo/scripts/check.sh"
  {
    echo 'jobs:'
    echo '  # test-ghost.sh is only mentioned in a comment'
    for name in $in_ci; do echo "      - run: scripts/$name"; done
  } > "$repo/.github/workflows/ci.yml"
}

base='test-alpha.sh test-beta.py test-vendor-drift.sh test-package-admission.sh'

# expect_pass <case>: the check exits 0.
expect_pass() {
  local label=$1 status=0
  "$checker" "$work/repo" > "$work/output" 2>&1 || status=$?
  if [ "$status" -ne 0 ]; then
    cat "$work/output" >&2
    fail "(check-script-tests): case '$label' | expected exit 0 | received exit $status"
  fi
}

# expect_fail <case> <message fragment>: the check exits 1 and names the fragment.
expect_fail() {
  local label=$1 fragment=$2 status=0
  "$checker" "$work/repo" > "$work/output" 2>&1 || status=$?
  if [ "$status" -ne 1 ]; then
    cat "$work/output" >&2
    fail "(check-script-tests): case '$label' | expected exit 1 | received exit $status"
  fi
  if ! grep -qF -- "$fragment" "$work/output"; then
    cat "$work/output" >&2
    fail "(check-script-tests): case '$label' | expected message containing: $fragment | received the output above"
  fi
}

make_repo "$work/repo" "$base" "$base"
expect_pass 'every test runs in both places'

make_repo "$work/repo" "test-alpha.sh test-vendor-drift.sh test-package-admission.sh" "test-alpha.sh test-vendor-drift.sh test-package-admission.sh"
expect_fail 'a test run by neither' 'scripts/test-beta.py is run by neither scripts/check.sh nor .github/workflows/ci.yml'

make_repo "$work/repo" "$base" "test-alpha.sh test-vendor-drift.sh test-package-admission.sh"
expect_fail 'a test only check.sh runs' 'scripts/test-beta.py is run by scripts/check.sh but not by .github/workflows/ci.yml'

make_repo "$work/repo" "test-alpha.sh test-vendor-drift.sh test-package-admission.sh" "$base"
expect_fail 'a test only ci.yml runs' 'scripts/test-beta.py is run by .github/workflows/ci.yml but not by scripts/check.sh'

make_repo "$work/repo" "$base test-ghost.sh" "$base"
expect_fail 'a stale reference in check.sh' 'scripts/test-ghost.sh is run by check.sh or ci.yml but does not exist'

make_repo "$work/repo" "$base" "$base test-ghost.sh"
expect_fail 'a stale reference in ci.yml' 'scripts/test-ghost.sh is run by check.sh or ci.yml but does not exist'

# An indirect test is held to its parent: the parent has to run in both places and name the child.
make_repo "$work/repo" "test-alpha.sh test-beta.py test-package-admission.sh" "$base"
expect_fail 'an indirect parent missing from check.sh' 'scripts/test-vendor-result.py runs through scripts/test-vendor-drift.sh, which scripts/check.sh does not run'

make_repo "$work/repo" "$base" "test-alpha.sh test-beta.py test-vendor-drift.sh"
expect_fail 'an indirect parent missing from ci.yml' 'scripts/test-packages.sh runs through scripts/test-package-admission.sh, which .github/workflows/ci.yml does not run'

make_repo "$work/repo" "$base" "$base"
: > "$work/repo/scripts/test-vendor-drift.sh"
expect_fail 'an indirect parent that no longer names the child' 'scripts/test-vendor-result.py is declared as run by scripts/test-vendor-drift.sh, which does not name it'

# The real repository passes.
"$checker" > "$work/output" 2>&1 || { cat "$work/output" >&2; fail "(check-script-tests): case 'this repository' | expected exit 0 | received the output above"; }

echo "check-script-tests.sh checks passed"
