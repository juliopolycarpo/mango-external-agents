#!/usr/bin/env bash
# Regression coverage for scripts/check-script-tests.sh: every scripts/test-*.{sh,py} must run in
# both scripts/check.sh and .github/workflows/ci.yml, and the ci.yml step must be able to run and
# fail the gate. Each case builds a small scratch repository (named fixtures, no network, no cargo)
# and runs the real check against it. It needs PyYAML, like the check.
# The cases hold shell and workflow text, `$(...)` and `${{ ... }}` included, in single quotes so it
# reaches the check unexpanded.
# shellcheck disable=SC2016 # literal shell text under test, never meant to expand here
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
  # shellcheck disable=SC2016 # the fixture script must contain the literal $work
  printf 'cp scripts/test-packages.sh "$work/"\n' > "$repo/scripts/test-package-admission.sh"
  {
    echo '# test-ghost.sh is only mentioned in a comment'
    for name in $in_check; do echo "run scripts/$name"; done
  } > "$repo/scripts/check.sh"
  workflow_fixture "$variant" "$placed" "$in_ci" > "$repo/.github/workflows/ci.yml"
}

# WorkflowFixture: a ci.yml whose `policy` job runs the files in $3 and is gated, plus the one test
# $2 placed as variant $1 describes. `clean` places nothing extra. `block` places the multi-line
# run block in $run_block; `matrix-custom` places a matrixed job from $matrix_lines and the step
# guard $matrix_guard. Whether the check refuses $2 is each case's own expectation.
workflow_fixture() {
  local variant=$1 placed=$2 in_ci=$3 name
  local needs='[policy]' triggers='pull_request' gate=true
  [ "$variant" = no-pull-request ] && triggers='push'
  [ "$variant" = gate-less ] && gate=false
  echo 'name: CI'
  echo "on: { $triggers: { branches: [main] } }"
  [ "$variant" = shell-workflow ] && echo "defaults: { run: { shell: '$shell_name' } }"
  echo 'jobs:'
  # A YAML comment naming a test is not a run, with or without the file behind it.
  echo '  # test-ghost.sh is only mentioned in a comment'
  echo '  policy:'
  echo '    runs-on: ubuntu-latest'
  echo '    steps:'
  echo '      - uses: actions/checkout@v7'
  for name in $in_ci; do echo "      - run: scripts/$name"; done
  case "$variant" in
    clean | shell-job | matrix-os | matrix-os-unlisted | matrix-custom | orphan-job | job-if | job-continue-on-error) ;;
    shell-step)
      echo "      - shell: '$shell_name'"
      echo "        run: scripts/$placed" ;;
    block)
      echo '      - run: |'
      while IFS= read -r line; do printf '          %s\n' "$line"; done <<<"$run_block" ;;
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
    no-pull-request | gate-less | shell-workflow)
      echo "      - run: scripts/$placed" ;;
    *) echo "(check-script-tests): unknown workflow variant '$variant'" >&2; exit 1 ;;
  esac
  case "$variant" in
    shell-job)
      echo '  shelled:'
      echo '    runs-on: ubuntu-latest'
      echo "    defaults: { run: { shell: '$shell_name' } }"
      echo '    steps:'
      echo "      - run: scripts/$placed"
      needs='[policy, shelled]' ;;
    matrix-custom)
      echo '  matrixed:'
      echo '    runs-on: ${{ matrix.os }}'
      echo '    strategy:'
      echo '      matrix:'
      while IFS= read -r line; do printf '        %s\n' "$line"; done <<<"$matrix_lines"
      echo '    steps:'
      echo "      - if: $matrix_guard"
      echo "        run: scripts/$placed"
      needs='[policy, matrixed]' ;;
    matrix-os | matrix-os-unlisted)
      echo '  matrixed:'
      # shellcheck disable=SC2016 # the workflow text must contain the literal ${{ matrix.os }}
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

run_block='' shell_name='' matrix_lines='' matrix_guard=''
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
expect_fail 'a step limited to an OS the matrix does not list' "the matrix os values are ['ubuntu-latest', 'macos-latest'], expected it to contain 'windows-latest'"

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
# shellcheck disable=SC2016 # the backticks are literal Markdown in the expected message, not command substitution
expect_fail 'a workflow without a gate job' '.github/workflows/ci.yml: the workflow has no `gate` job'

# Shell constructs inside a run block. A command counts only when it runs unconditionally and its
# failure fails the step; a construct the check does not follow is refused with the reason.
# block_pass <case> <block>: the block places the test and the check passes.
block_pass() {
  run_block=$2
  make_repo "$work/repo" "$base" "$without_beta" block
  expect_pass "$1"
}
# block_fail <case> <block> <reason>: the block places the test and the check names the reason.
block_fail() {
  run_block=$2
  make_repo "$work/repo" "$base" "$without_beta" block
  expect_fail "$1" "${blocked_prefix}$3"
}
not_followed='which the check does not follow'

block_pass 'a command after ; on the same line' 'echo start; scripts/test-beta.py'
block_pass 'a command after a quoted ; in an earlier command' $'echo "a; b"\nscripts/test-beta.py'
block_pass 'a command that ends in a comment' 'scripts/test-beta.py # runs the lane'
block_pass 'a command whose failure exits the step' 'scripts/test-beta.py || exit 1'
block_pass 'a command after set +e was undone by set -e' $'set +e\ntrue\nset -e\nscripts/test-beta.py'
block_pass 'a command with redirections' 'scripts/test-beta.py > log 2>&1'
block_pass 'a command after a here-string' $'grep x <<<word\nscripts/test-beta.py'
block_pass 'a command after a here-document' $'cat <<EOF\nnote\nEOF\nscripts/test-beta.py'
block_pass 'a command after an exit that only runs on failure' $'scripts/test-alpha.sh || exit 3\nscripts/test-beta.py'

block_fail 'a command in a one-line if' 'if true; then scripts/test-beta.py; fi' "it is inside a shell \`if\` block, $not_followed"
block_fail 'a command in a multi-line if' $'if [ -n "$RUN" ]; then\n  scripts/test-beta.py\nfi' "it is inside a shell \`if\` block, $not_followed"
block_fail 'a command in the else branch' $'if true; then\n  echo skip\nelse\n  scripts/test-beta.py\nfi' "it is inside a shell \`if\` block, $not_followed"
block_fail 'a command that is an if condition' $'if scripts/test-beta.py; then\n  echo ok\nfi' "it is inside a shell \`if\` block, $not_followed"
block_fail 'a command in a for loop' 'for lane in a b; do scripts/test-beta.py; done' "it is inside a shell \`for\` block, $not_followed"
block_fail 'a command in a while loop' $'while false; do\n  scripts/test-beta.py\ndone' "it is inside a shell \`while\` block, $not_followed"
block_fail 'a command in a case arm' $'case "$X" in\n  a) scripts/test-beta.py ;;\nesac' "it is inside a shell \`case\` block, $not_followed"
block_fail 'a command in a function body' $'lane() { scripts/test-beta.py; }\nlane' "it is inside a shell \`function\` block, $not_followed"
block_fail 'a command in a brace group' '{ scripts/test-beta.py; }' "it is inside a shell \`{\` block, $not_followed"
block_fail 'a command in a subshell' '(scripts/test-beta.py)' "it is inside a shell \`(\` block, $not_followed"
block_fail 'a command guarded by &&' '[ -n "$RUN" ] && scripts/test-beta.py' "it is joined to the command before it by \`&&\`, $not_followed"
block_fail 'a command behind ||' 'false || scripts/test-beta.py' "it is joined to the command before it by \`||\`, $not_followed"
block_fail 'a command whose status feeds &&' 'scripts/test-beta.py && echo passed' "it is joined to the command after it by \`&&\`, $not_followed"
block_fail 'a command whose failure is handled by ||' 'scripts/test-beta.py || echo failed' "its failure is handled by \`|| echo failed\`, $not_followed"
block_fail 'a command piped into another' 'scripts/test-beta.py | tee log' "it is joined to the command after it by \`|\`, $not_followed"
block_fail 'a command fed by a pipe' 'echo input | scripts/test-beta.py' "it is joined to the command before it by \`|\`, $not_followed"
block_fail 'a command in the background' 'scripts/test-beta.py &' "it is joined to the command after it by \`&\`, $not_followed"
block_fail 'a command after set +e' $'set +e\nscripts/test-beta.py' 'it follows `set +e`, which turns off errexit'
block_fail 'a command after set +e on the same line' 'set +e; scripts/test-beta.py' 'it follows `set +e`, which turns off errexit'
block_fail 'a command after set +eu' $'set +eu\nscripts/test-beta.py' 'it follows `set +eu`, which turns off errexit'
block_fail 'a command after set +o errexit' $'set +o errexit\nscripts/test-beta.py' 'it follows `set +o errexit`, which turns off errexit'
block_fail 'a command after exit on the same line' 'exit 0; scripts/test-beta.py' 'it follows `exit 0` on an earlier line of the same run block'
block_fail 'a command after a conditional exit 0' $'[ -n "$SKIP" ] && exit 0\nscripts/test-beta.py' 'it follows `exit 0`, which a condition can reach to end the run block early'
block_fail 'a name in a here-document' $'cat <<EOF\nscripts/test-beta.py\nEOF' 'it appears in a here-document'
block_fail 'a name in a command substitution' 'echo $(scripts/test-beta.py)' '`echo $(scripts/test-beta.py)` names it but does not start with it as a command'

# Shell forms that look harmless but let a failing test exit the step with status 0, or never run it.
block_pass 'a command after an arithmetic command' $'(( n = 1 ))\nscripts/test-beta.py'
block_fail 'a failing exit handler that runs in the background' 'scripts/test-beta.py || exit 1 &' "its failure is handled by \`|| exit 1\`, $not_followed"
block_fail 'a failing exit handler that is piped' 'scripts/test-beta.py || exit 1 | cat' "its failure is handled by \`|| exit 1\`, $not_followed"
block_fail 'a name in a here-document with a backslash-quoted delimiter' $'cat <<\\EOF\nscripts/test-beta.py\nEOF' 'it appears in a here-document'
block_fail 'a here-document body between a || and the command' $'true <<EOF ||\nx\nEOF\nscripts/test-beta.py' "it is joined to the command before it by \`||\`, $not_followed"
block_fail 'a quoted ) inside a subshell' $'(\necho "a)"\nscripts/test-beta.py\n) || true' "it is inside a shell \`(\` block, $not_followed"
block_fail 'a || handler that exits with a multiple of 256' 'scripts/test-beta.py || exit 256' "its failure is handled by \`|| exit 256\`, $not_followed"
block_fail 'a command after a conditional exit 256' $'[ -n "$SKIP" ] && exit 256\nscripts/test-beta.py' 'it follows `exit 256`, which a condition can reach to end the run block early'
block_fail 'a command after a conditional bare exit' $'[ -n "$SKIP" ] && exit\nscripts/test-beta.py' 'it follows `exit`, which a condition can reach to end the run block early'
block_pass 'a command after a bare exit that handles a failure' $'true || exit\nscripts/test-beta.py'
block_fail 'a command after eval set +e' $'eval \'set +e\'\nscripts/test-beta.py' $'it follows `eval \'set +e\'`, which turns off errexit'
block_fail 'a command after command set +e' $'command set +e\nscripts/test-beta.py' 'it follows `command set +e`, which turns off errexit'
block_fail 'a command after builtin set +e' $'builtin set +e\nscripts/test-beta.py' 'it follows `builtin set +e`, which turns off errexit'

# The shell has to stop at a failing command, wherever it is declared.
shell_pass() {
  shell_name=$2
  make_repo "$work/repo" "$base" "$without_beta" "$3"
  expect_pass "$1"
}
shell_fail() {
  shell_name=$2
  make_repo "$work/repo" "$base" "$without_beta" "$3"
  expect_fail "$1" "${blocked_prefix}its step runs under \`shell: $2\`, expected bash or sh, which stop at a failing command"
}
shell_pass 'a step under shell: bash' bash shell-step
shell_pass 'a step under shell: sh' sh shell-step
shell_pass 'a job defaulting to shell: bash' bash shell-job
shell_pass 'a workflow defaulting to shell: bash' bash shell-workflow
shell_fail 'a step under shell: pwsh' pwsh shell-step
shell_fail 'a step under a custom shell without -e' 'bash {0}' shell-step
shell_fail 'a job defaulting to shell: python' python shell-job
shell_fail 'a workflow defaulting to shell: pwsh' pwsh shell-workflow

# A matrix is expanded the way GitHub does: the cross product of its lists, minus `exclude`, plus
# `include` (merged into the combinations it extends, or a new combination).
matrix_pass() {
  matrix_lines=$2 matrix_guard=$3
  make_repo "$work/repo" "$base" "$without_beta" matrix-custom
  expect_pass "$1"
}
matrix_fail() {
  matrix_lines=$2 matrix_guard=$3
  make_repo "$work/repo" "$base" "$without_beta" matrix-custom
  expect_fail "$1" "${blocked_prefix}a step of job 'matrixed' has \`if: $3\` but $4"
}

matrix_pass 'an OS added by an include entry' $'os: [ubuntu-latest]\ninclude:\n  - os: windows-latest' "matrix.os == 'windows-latest'"
matrix_pass 'an OS that only include entries name' $'include:\n  - os: windows-latest' "matrix.os == 'windows-latest'"
matrix_pass 'an OS added to a combination by include' $'node: [20]\ninclude:\n  - node: 20\n    os: macos-latest' "matrix.os == 'macos-latest'"
matrix_pass 'an OS with one of its combinations excluded' $'os: [ubuntu-latest, macos-latest]\narch: [x64, arm]\nexclude:\n  - os: ubuntu-latest\n    arch: arm' "matrix.os == 'ubuntu-latest'"
matrix_fail 'an OS that no include entry adds' $'os: [ubuntu-latest]\ninclude:\n  - os: macos-latest' "matrix.os == 'windows-latest'" "the matrix os values are ['ubuntu-latest', 'macos-latest'], expected it to contain 'windows-latest'"
matrix_fail 'an OS that exclude removes' $'os: [ubuntu-latest, macos-latest]\nexclude:\n  - os: ubuntu-latest' "matrix.os == 'ubuntu-latest'" "the matrix os values are ['macos-latest'], expected it to contain 'ubuntu-latest'"
matrix_fail 'an OS that every combination excludes' $'os: [ubuntu-latest]\narch: [x64]\nexclude:\n  - arch: x64' "matrix.os == 'ubuntu-latest'" "the matrix os values are [], expected it to contain 'ubuntu-latest'"
matrix_fail 'an os list that is an expression' 'os: ${{ fromJSON(needs.plan.outputs.os) }}' "matrix.os == 'ubuntu-latest'" "its matrix cannot be expanded: matrix.os is '\${{ fromJSON(needs.plan.outputs.os) }}', expected a list"
matrix_fail 'an include os that is an expression' $'os: [ubuntu-latest]\ninclude:\n  - os: ${{ inputs.os }}' "matrix.os == 'ubuntu-latest'" "its matrix cannot be expanded: a matrix entry sets os to '\${{ inputs.os }}', expected a literal"

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
