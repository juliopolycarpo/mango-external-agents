#!/usr/bin/env bash
# Regression coverage for scripts/check-script-tests.sh: every scripts/test-*.{sh,py} must run in
# both scripts/check.sh and .github/workflows/ci.yml, and the ci.yml step must be able to run and
# fail the gate. Each case builds a small scratch repository (named fixtures, no network, no cargo)
# and runs the real check against it. It needs PyYAML, like the check.
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d "${TMPDIR:-/tmp}/check-script-tests-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
checker="$PWD/scripts/check-script-tests.sh"

fail() { echo "$*" >&2; exit 1; }

# FixtureRepo: scripts/ holds the named test files plus a vendor-drift parent that names its two
# Python children and an admission test that names test-packages.sh; check.sh runs the files listed
# in $2 and the workflow fixture below runs the ones in $3. $4 is a workflow variant and $5 the test
# it places (default `test-beta.py`); the variant is added on top of the files in $3.
make_repo() {
  local repo=$1 in_check=$2 in_ci=$3 variant=${4:-clean} placed=${5:-test-beta.py} name
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
  workflow_fixture "$variant" "$placed" "$in_ci" > "$repo/.github/workflows/ci.yml"
}

# WorkflowFixture: a ci.yml whose `policy` job runs the files in $3 and is gated, plus the one test
# $2 placed as variant $1 describes. `clean` places nothing extra. Every variant but `clean` and
# `matrix-os` must make the check refuse $2.
workflow_fixture() {
  local variant=$1 placed=$2 in_ci=$3 name
  local needs='[policy]' triggers='pull_request' gate=true
  [ "$variant" = no-pull-request ] && triggers='push'
  [ "$variant" = gate-less ] && gate=false
  echo 'name: CI'
  echo "on: { $triggers: { branches: [main] } }"
  echo 'jobs:'
  # A YAML comment naming a test is not a run, with or without the file behind it.
  echo '  # test-ghost.sh is only mentioned in a comment'
  echo '  policy:'
  echo '    runs-on: ubuntu-latest'
  echo '    steps:'
  echo '      - uses: actions/checkout@v7'
  for name in $in_ci; do echo "      - run: scripts/$name"; done
  case "$variant" in
    clean | matrix-os | matrix-os-unlisted | orphan-job | job-if | job-continue-on-error) ;;
    never-true-if)
      echo "      - if: github.event_name == 'release'"
      echo "        run: scripts/$placed" ;;
    false-if)
      echo '      - if: false'
      echo "        run: scripts/$placed" ;;
    step-continue-on-error)
      echo '      - continue-on-error: true'
      echo "        run: scripts/$placed" ;;
    exit-before)
      echo '      - run: |'
      echo '          exit 0'
      echo "          scripts/$placed" ;;
    swallowed-status)
      echo "      - run: scripts/$placed || true" ;;
    echoed)
      echo "      - run: echo scripts/$placed" ;;
    commented-in-run)
      echo '      - run: |'
      echo "          # scripts/$placed"
      echo '          echo done' ;;
    interpreter-and-env)
      echo "      - run: RUSTUP_TOOLCHAIN=1.97 python3 scripts/$placed --flag" ;;
    no-pull-request | gate-less)
      echo "      - run: scripts/$placed" ;;
    *) echo "(check-script-tests): unknown workflow variant '$variant'" >&2; exit 1 ;;
  esac
  case "$variant" in
    matrix-os | matrix-os-unlisted)
      echo '  matrixed:'
      echo '    runs-on: ${{ matrix.os }}'
      echo '    strategy:'
      echo '      matrix:'
      echo '        os: [ubuntu-latest, macos-latest]'
      echo '    steps:'
      if [ "$variant" = matrix-os ]; then
        echo "      - if: matrix.os == 'ubuntu-latest'"
      else
        echo "      - if: matrix.os == 'windows-latest'"
      fi
      echo "        run: scripts/$placed"
      needs='[policy, matrixed]' ;;
    orphan-job)
      echo '  orphan:'
      echo '    runs-on: ubuntu-latest'
      echo '    steps:'
      echo "      - run: scripts/$placed" ;;
    job-if | job-continue-on-error)
      echo '  extra:'
      echo '    runs-on: ubuntu-latest'
      if [ "$variant" = job-if ]; then
        echo "    if: github.ref == 'refs/heads/never'"
      else
        echo '    continue-on-error: true'
      fi
      echo '    steps:'
      echo "      - run: scripts/$placed"
      needs='[policy, extra]' ;;
  esac
  if [ "$gate" = true ]; then
    echo '  gate:'
    echo '    if: always()'
    echo "    needs: $needs"
    echo '    runs-on: ubuntu-latest'
    echo '    steps:'
    echo '      - run: echo gate'
  fi
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

# The ci.yml step has to be able to run and fail the gate, not only name the test.
without_beta='test-alpha.sh test-vendor-drift.sh test-package-admission.sh'
blocked_prefix='scripts/test-beta.py is run by scripts/check.sh but not by .github/workflows/ci.yml; '

make_repo "$work/repo" "$base" "$without_beta" matrix-os
expect_pass 'a step limited to an OS the job matrix lists'

make_repo "$work/repo" "$base" "$without_beta" interpreter-and-env
expect_pass 'a command with an env prefix, an interpreter and arguments'

make_repo "$work/repo" "$base" "$without_beta" never-true-if
expect_fail 'a step under an if that is not an OS test' "${blocked_prefix}a step of job 'policy' has \`if: github.event_name == 'release'\`, expected none or one \`matrix.os == '<os>'\` test"

make_repo "$work/repo" "$base" "$without_beta" false-if
expect_fail 'a step under if: false' "${blocked_prefix}a step of job 'policy' has \`if: False\`"

make_repo "$work/repo" "$base" "$without_beta" matrix-os-unlisted
expect_fail 'a step limited to an OS the matrix does not list' "the matrix os list is ['ubuntu-latest', 'macos-latest'], expected it to contain 'windows-latest'"

make_repo "$work/repo" "$base" "$without_beta" orphan-job
expect_fail 'a job missing from the gate needs' "${blocked_prefix}job 'orphan' is not in the needs of the gate job"

make_repo "$work/repo" "$base" "$without_beta" job-if
expect_fail 'a job under an if' "${blocked_prefix}job 'extra' has \`if: github.ref == 'refs/heads/never'\`"

make_repo "$work/repo" "$base" "$without_beta" job-continue-on-error
expect_fail 'a job that continues on error' "${blocked_prefix}job 'extra' sets \`continue-on-error: True\`"

make_repo "$work/repo" "$base" "$without_beta" step-continue-on-error
expect_fail 'a step that continues on error' "${blocked_prefix}a step of job 'policy' sets \`continue-on-error: True\`"

make_repo "$work/repo" "$base" "$without_beta" exit-before
expect_fail 'a command after exit in the same run block' "${blocked_prefix}it follows \`exit 0\` on an earlier line of the same run block"

make_repo "$work/repo" "$base" "$without_beta" swallowed-status
expect_fail 'a command whose status is swallowed' "${blocked_prefix}its line ends with \`|| true\`, \`|| :\` or \`|| exit 0\`"

make_repo "$work/repo" "$base" "$without_beta" echoed
expect_fail 'a name that is echoed, not run' "${blocked_prefix}\`echo scripts/test-beta.py\` names it but does not start with it as a command"

make_repo "$work/repo" "$base" "$without_beta" commented-in-run
expect_fail 'a name only in a comment of a run block' 'scripts/test-beta.py is run by scripts/check.sh but not by .github/workflows/ci.yml'

make_repo "$work/repo" "$base" "$without_beta" no-pull-request
expect_fail 'a workflow that does not trigger on pull_request' '.github/workflows/ci.yml: the workflow does not trigger on pull_request'

make_repo "$work/repo" "$base" "$without_beta" gate-less
expect_fail 'a workflow without a gate job' '.github/workflows/ci.yml: the workflow has no `gate` job'

# An indirect parent is held to the same rule.
make_repo "$work/repo" "$base" "test-alpha.sh test-beta.py test-package-admission.sh" orphan-job test-vendor-drift.sh
expect_fail 'an indirect parent in a job missing from the gate needs' "scripts/test-vendor-result.py runs through scripts/test-vendor-drift.sh, which .github/workflows/ci.yml does not run; job 'orphan' is not in the needs of the gate job"

# A workflow the check cannot parse stops it with the received value, not a silent pass.
make_repo "$work/repo" "$base" "$base"
printf 'jobs: [unclosed\n' > "$work/repo/.github/workflows/ci.yml"
expect_fail 'a workflow that is not valid YAML' 'expected .github/workflows/ci.yml to be valid YAML'

# The real repository passes.
"$checker" > "$work/output" 2>&1 || { cat "$work/output" >&2; fail "(check-script-tests): case 'this repository' | expected exit 0 | received the output above"; }

echo "check-script-tests.sh checks passed"
