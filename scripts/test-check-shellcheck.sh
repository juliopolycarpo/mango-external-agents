#!/usr/bin/env bash
# Regression coverage for scripts/check-shellcheck.sh and for how scripts/check.sh and ci.yml
# reach it. Each case runs the real gate in a scratch repository with a named fake shellcheck, so
# the test needs neither the tool nor a network. One extra case runs the real shellcheck when it is
# installed (always in CI) to prove a finding fails the gate and names the script.
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d "${TMPDIR:-/tmp}/check-shellcheck-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
gate="$PWD/scripts/check-shellcheck.sh"

fail() { echo "$*" >&2; exit 1; }

# FixtureRepo: two shell scripts (one in the fixtures directory), plus files that are not shell.
mkdir -p "$work/repo/scripts/test-fixtures" "$work/empty/scripts"
: > "$work/repo/scripts/alpha.sh"
: > "$work/repo/scripts/test-fixtures/fake-beta.sh"
: > "$work/repo/scripts/gamma.py"
: > "$work/repo/scripts/notes.txt"

# FakeShellcheck: answers --version like the real tool; otherwise logs one argument per line to
# FAKE_SHELLCHECK_LOG and exits FAKE_SHELLCHECK_STATUS.
mkdir -p "$work/fakebin" "$work/nobin"
cat > "$work/fakebin/shellcheck" <<'FAKE_SHELLCHECK'
#!/usr/bin/env bash
if [ "${1-}" = --version ]; then
  printf 'ShellCheck - shell script analysis tool\nversion: 9.9.9-fake\nlicense: GNU General Public License, version 3\n'
  exit 0
fi
printf '%s\n' "$@" > "$FAKE_SHELLCHECK_LOG"
echo "FakeShellcheck: reporting as asked" >&2
exit "${FAKE_SHELLCHECK_STATUS:-0}"
FAKE_SHELLCHECK
chmod +x "$work/fakebin/shellcheck"
# The gate's own tools, without shellcheck, for the missing-tool case.
for tool in find sort dirname; do ln -s "$(command -v "$tool")" "$work/nobin/$tool"; done

# run_gate <path-dir> <repo> [status]: sets status, stdout_text, stderr_text. $BASH runs the gate so
# the shebang's PATH lookup cannot pick up a different shell.
run_gate() {
  status=0
  : > "$work/shellcheck.log"
  PATH=$1 FAKE_SHELLCHECK_LOG="$work/shellcheck.log" FAKE_SHELLCHECK_STATUS=${3:-0} \
    "$BASH" "$gate" "$2" > "$work/stdout" 2> "$work/stderr" || status=$?
  stdout_text=$(cat "$work/stdout")
  stderr_text=$(cat "$work/stderr")
}

# Case 1: shellcheck present and happy: one invocation over exactly the .sh files, sorted.
run_gate "$work/fakebin:$PATH" "$work/repo"
[ "$status" -eq 0 ] || fail "expected the gate to pass when shellcheck reports nothing | received: exit $status: $stderr_text"
expected_args=$'scripts/alpha.sh\nscripts/test-fixtures/fake-beta.sh'
received_args=$(cat "$work/shellcheck.log")
[ "$received_args" = "$expected_args" ] || fail "expected shellcheck to receive only the .sh files under scripts/, sorted | received: $received_args"
grep -q 'shellcheck: 2 scripts clean' <<<"$stdout_text" || fail "expected the gate to report 2 scripts clean | received: $stdout_text"
grep -qx 'shellcheck version 9.9.9-fake' <<<"$stdout_text" || fail "expected the gate to print the version of the shellcheck that ran | received: $stdout_text"

# Case 2: shellcheck reports a finding: its status and output reach the caller.
run_gate "$work/fakebin:$PATH" "$work/repo" 1
[ "$status" -eq 1 ] || fail "expected the gate to exit 1 when shellcheck exits 1 | received: exit $status"
grep -q 'FakeShellcheck: reporting as asked' <<<"$stderr_text" || fail "expected the shellcheck report on stderr | received: $stderr_text"

# Case 3: shellcheck missing: the gate fails and names the tool instead of passing silently.
run_gate "$work/nobin" "$work/repo"
[ "$status" -eq 1 ] || fail "expected the gate to exit 1 without shellcheck on PATH | received: exit $status: $stderr_text"
grep -q 'expected shellcheck on PATH' <<<"$stderr_text" || fail "expected a message naming shellcheck | received: $stderr_text"

# Case 4: no scripts at all is a wrong root, not a clean run.
run_gate "$work/fakebin:$PATH" "$work/empty"
[ "$status" -eq 1 ] || fail "expected the gate to exit 1 for a root with no scripts | received: exit $status: $stderr_text"
grep -q 'expected shell scripts under scripts/' <<<"$stderr_text" || fail "expected a message naming scripts/ | received: $stderr_text"

# Case 5: the real tool fails a script with a finding and names it.
if command -v shellcheck >/dev/null 2>&1; then
  mkdir -p "$work/withshell/scripts"
  cp .shellcheckrc "$work/withshell/"
  # shellcheck disable=SC2016 # the fixture script must contain the literal $1, the finding under test
  printf '#!/usr/bin/env bash\nrm -rf $1/*\n' > "$work/withshell/scripts/bad.sh"
  run_gate "$PATH" "$work/withshell"
  [ "$status" -ne 0 ] || fail "expected the gate to fail on an unquoted expansion in scripts/bad.sh | received: exit 0"
  grep -q 'scripts/bad.sh' <<<"$stdout_text$stderr_text" || fail "expected the finding to name scripts/bad.sh | received: $stdout_text $stderr_text"
fi

# Wiring: check.sh runs the gate only when shellcheck is installed and warns otherwise; the Policy
# job installs the tool through the pinned install-action step and runs the gate unconditionally.
check_probe=$(grep -c '^if command -v shellcheck' scripts/check.sh || true)
check_run=$(grep -c '^  run scripts/check-shellcheck\.sh$' scripts/check.sh || true)
[ "$check_probe" -eq 1 ] && [ "$check_run" -eq 1 ] || fail "expected scripts/check.sh to probe shellcheck once and run scripts/check-shellcheck.sh once inside it | received $check_probe probe(s), $check_run run(s)"
grep -q 'warning: shellcheck not found' scripts/check.sh || fail "expected scripts/check.sh to warn that shellcheck is missing"
policy=$(sed -n '/^  policy:/,/^  msrv:/p' .github/workflows/ci.yml)
grep -qE '^ +tool: .*shellcheck' <<<"$policy" || fail "expected the Policy job's install-action step to list shellcheck"
grep -qxE ' +- run: scripts/check-shellcheck\.sh' <<<"$policy" || fail "expected the Policy job to run scripts/check-shellcheck.sh"

echo "check-shellcheck: all cases passed"
