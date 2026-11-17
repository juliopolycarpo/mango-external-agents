#!/usr/bin/env bash
# Regression coverage for scripts/check-actionlint.sh and for how scripts/check.sh and ci.yml reach
# it. Each case runs the real gate in a scratch repository with a named FakeActionlint, so the
# case needs neither tool nor a network. The last cases run the real actionlint and shellcheck when
# both are installed (always in CI) to prove a finding in a `run:` block fails the gate and that a
# documented `# shellcheck disable=` directive clears it.
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d "${TMPDIR:-/tmp}/check-actionlint-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
gate="$PWD/scripts/check-actionlint.sh"

fail() { echo "$*" >&2; exit 1; }

# FixtureRepo: two workflows (one .yaml), plus files that are not workflows.
mkdir -p "$work/repo/.github/workflows" "$work/empty/.github/workflows"
: > "$work/repo/.github/workflows/alpha.yml"
: > "$work/repo/.github/workflows/beta.yaml"
: > "$work/repo/.github/workflows/notes.md"
: > "$work/repo/.github/dependabot.yml"

# FakeActionlint: answers -version like the real tool; otherwise logs one argument per line to
# FAKE_ACTIONLINT_LOG and exits FAKE_ACTIONLINT_STATUS.
# FakeShellcheck: answers --version only; the gate checks its presence, actionlint calls it.
mkdir -p "$work/fakebin" "$work/nobin" "$work/noshellbin"
cat > "$work/fakebin/actionlint" <<'FAKE_ACTIONLINT'
#!/usr/bin/env bash
if [ "${1-}" = -version ]; then
  printf '9.9.9-fake\ninstalled by FakeActionlint\nbuilt with go0 compiler for linux/amd64\n'
  exit 0
fi
printf '%s\n' "$@" > "$FAKE_ACTIONLINT_LOG"
echo "FakeActionlint: reporting as asked" >&2
exit "${FAKE_ACTIONLINT_STATUS:-0}"
FAKE_ACTIONLINT
cat > "$work/fakebin/shellcheck" <<'FAKE_SHELLCHECK'
#!/usr/bin/env bash
printf 'ShellCheck - shell script analysis tool\nversion: 8.8.8-fake\n'
FAKE_SHELLCHECK
chmod +x "$work/fakebin/actionlint" "$work/fakebin/shellcheck"
# The gate's own tools, without either linter, for the missing-tool cases.
for tool in find sort dirname sed; do ln -s "$(command -v "$tool")" "$work/nobin/$tool"; done
for tool in find sort dirname sed; do ln -s "$(command -v "$tool")" "$work/noshellbin/$tool"; done
# FakeActionlint is here too, with the bash its shebang looks up, so a gate that skipped the probe
# for the second tool would run it and fail the case on the assertions below.
ln -s "$work/fakebin/actionlint" "$work/noshellbin/actionlint"
ln -s "$BASH" "$work/noshellbin/bash"

# run_gate <path-dir> <repo> [status]: sets status, stdout_text, stderr_text. $BASH runs the gate so
# the shebang's PATH lookup cannot pick up a different shell.
run_gate() {
  status=0
  : > "$work/actionlint.log"
  PATH=$1 FAKE_ACTIONLINT_LOG="$work/actionlint.log" FAKE_ACTIONLINT_STATUS=${3:-0} \
    "$BASH" "$gate" "$2" > "$work/stdout" 2> "$work/stderr" || status=$?
  stdout_text=$(cat "$work/stdout")
  stderr_text=$(cat "$work/stderr")
}

# Case 1: both tools present and happy: one invocation over exactly the workflow files, sorted.
run_gate "$work/fakebin:$PATH" "$work/repo"
[ "$status" -eq 0 ] || fail "expected the gate to pass when actionlint reports nothing | received: exit $status: $stderr_text"
expected_args=$'.github/workflows/alpha.yml\n.github/workflows/beta.yaml'
received_args=$(cat "$work/actionlint.log")
[ "$received_args" = "$expected_args" ] || fail "expected actionlint to receive only the .yml and .yaml files under .github/workflows/, sorted | received: $received_args"
grep -q 'actionlint: 2 workflows clean' <<<"$stdout_text" || fail "expected the gate to report 2 workflows clean | received: $stdout_text"
grep -qx 'actionlint version 9.9.9-fake' <<<"$stdout_text" || fail "expected the gate to print the version of the actionlint that ran | received: $stdout_text"
grep -qx 'shellcheck version 8.8.8-fake' <<<"$stdout_text" || fail "expected the gate to print the version of the shellcheck actionlint calls | received: $stdout_text"

# Case 2: actionlint reports a finding: its status and output reach the caller.
run_gate "$work/fakebin:$PATH" "$work/repo" 1
[ "$status" -eq 1 ] || fail "expected the gate to exit 1 when actionlint exits 1 | received: exit $status"
grep -q 'FakeActionlint: reporting as asked' <<<"$stderr_text" || fail "expected the actionlint report on stderr | received: $stderr_text"

# Case 3: actionlint missing: the gate fails and names the tool instead of passing silently.
run_gate "$work/nobin" "$work/repo"
[ "$status" -eq 1 ] || fail "expected the gate to exit 1 without actionlint on PATH | received: exit $status: $stderr_text"
grep -q 'expected actionlint on PATH' <<<"$stderr_text" || fail "expected a message naming actionlint | received: $stderr_text"

# Case 4: shellcheck missing: actionlint would skip the run blocks without a word, so the gate fails
# and names shellcheck. The fake actionlint must not have run.
run_gate "$work/noshellbin" "$work/repo"
[ "$status" -eq 1 ] || fail "expected the gate to exit 1 without shellcheck on PATH | received: exit $status: $stderr_text"
grep -q 'expected shellcheck on PATH' <<<"$stderr_text" || fail "expected a message naming shellcheck | received: $stderr_text"
[ ! -s "$work/actionlint.log" ] || fail "expected actionlint not to run without shellcheck | received arguments: $(cat "$work/actionlint.log")"

# Case 5: no workflows at all is a wrong root, not a clean run.
run_gate "$work/fakebin:$PATH" "$work/empty"
[ "$status" -eq 1 ] || fail "expected the gate to exit 1 for a root with no workflows | received: exit $status: $stderr_text"
grep -q 'expected workflow files under .github/workflows/' <<<"$stderr_text" || fail "expected a message naming .github/workflows/ | received: $stderr_text"

# Cases 6 and 7: the real tools. An unquoted expansion in a run block fails the gate and names the
# workflow and the shellcheck code; the same block with a reasoned directive passes.
if command -v actionlint >/dev/null 2>&1 && command -v shellcheck >/dev/null 2>&1; then
  mkdir -p "$work/real/.github/workflows"
  cat > "$work/real/.github/workflows/bad.yml" <<'BAD_WORKFLOW'
name: bad
on: push
jobs:
  unquoted:
    runs-on: ubuntu-latest
    steps:
      - run: rm -rf $HOME/scratch/*
BAD_WORKFLOW
  run_gate "$PATH" "$work/real"
  [ "$status" -ne 0 ] || fail "expected the gate to fail on an unquoted expansion in a run block | received: exit 0"
  grep -q '.github/workflows/bad.yml' <<<"$stdout_text$stderr_text" || fail "expected the finding to name .github/workflows/bad.yml | received: $stdout_text $stderr_text"
  grep -q 'SC2086' <<<"$stdout_text$stderr_text" || fail "expected shellcheck's SC2086 through actionlint | received: $stdout_text $stderr_text"

  cat > "$work/real/.github/workflows/bad.yml" <<'ANNOTATED_WORKFLOW'
name: annotated
on: push
jobs:
  unquoted:
    runs-on: ubuntu-latest
    steps:
      - run: |
          # shellcheck disable=SC2086 # the glob is the point of this scratch fixture
          rm -rf $HOME/scratch/*
ANNOTATED_WORKFLOW
  run_gate "$PATH" "$work/real"
  [ "$status" -eq 0 ] || fail "expected a run block with a reasoned shellcheck directive to pass | received: exit $status: $stdout_text $stderr_text"
fi

# Wiring: check.sh runs the gate only when both tools are installed and warns otherwise; the Policy
# job installs actionlint through the pinned installer, before the gate, and runs the gate
# unconditionally.
check_probe=$(grep -c '^if command -v actionlint .*command -v shellcheck' scripts/check.sh || true)
check_run=$(grep -c '^  run scripts/check-actionlint\.sh$' scripts/check.sh || true)
[ "$check_probe" -eq 1 ] && [ "$check_run" -eq 1 ] || fail "expected scripts/check.sh to probe actionlint and shellcheck once and run scripts/check-actionlint.sh once inside it | received $check_probe probe(s), $check_run run(s)"
grep -q 'warning: actionlint or shellcheck not found' scripts/check.sh || fail "expected scripts/check.sh to warn that actionlint or shellcheck is missing"
policy=$(sed -n '/^  policy:/,/^  msrv:/p' .github/workflows/ci.yml)
grep -qE '^ +tool: .*shellcheck@[0-9]+\.[0-9]+\.[0-9]+' <<<"$policy" || fail "expected the Policy job's install-action step to install shellcheck at an exact version, which actionlint calls"
install_line=$(grep -nE '^ +- run: scripts/install-actionlint\.sh ' <<<"$policy" | head -n 1 | cut -d: -f1 || true)
gate_line=$(grep -nxE ' +- run: scripts/check-actionlint\.sh' <<<"$policy" | head -n 1 | cut -d: -f1 || true)
[ -n "$install_line" ] || fail "expected the Policy job to run scripts/install-actionlint.sh with a bin directory | received none"
[ -n "$gate_line" ] || fail "expected the Policy job to run scripts/check-actionlint.sh | received none"
[ "$install_line" -lt "$gate_line" ] || fail "expected the Policy job to install actionlint before the gate | received install at Policy line $install_line, gate at line $gate_line"
# The tests above run the real tools only when they are on PATH, so the install has to come first.
self_line=$(grep -nxE ' +- run: scripts/test-check-actionlint\.sh' <<<"$policy" | head -n 1 | cut -d: -f1 || true)
[ -n "$self_line" ] || fail "expected the Policy job to run scripts/test-check-actionlint.sh | received none"
[ "$install_line" -lt "$self_line" ] || fail "expected the Policy job to install actionlint before scripts/test-check-actionlint.sh so its real-tool cases run | received install at Policy line $install_line, test at line $self_line"

echo "check-actionlint: all cases passed"
