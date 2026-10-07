#!/usr/bin/env bash
# Regression coverage for the feature powerset in scripts/check.sh: with cargo-hack installed the
# script runs the command CI runs and fails when it fails; without cargo-hack it warns once, names
# the skipped gate, and still returns the status of every other gate.
#
# The real check.sh runs in a scratch tree where every gate it calls is a named fake, so a case
# costs milliseconds and no cargo build. The CI command is read from ci.yml rather than repeated
# here, so loosening either side (a lower depth, a dropped flag, another package) fails this test.
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d "${TMPDIR:-/tmp}/check-powerset-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

fail() { echo "$*" >&2; exit 1; }

ci_steps=$(sed -n 's/^ *- run: \(cargo hack clippy --feature-powerset .*\)$/\1/p' .github/workflows/ci.yml)
ci_count=$(printf '%s' "$ci_steps" | grep -c . || true)
if [ "$ci_count" -ne 1 ]; then
  fail "expected exactly one 'cargo hack clippy --feature-powerset' step in .github/workflows/ci.yml, received $ci_count: $ci_steps"
fi
ci_args=${ci_steps#cargo }

mkdir -p "$work/scripts" "$work/bin"
cp scripts/check.sh "$work/scripts/check.sh"

# FakeGate: stands in for every scripts/*.sh that check.sh runs, whatever the list is today.
gates=$(grep -oE 'run scripts/[A-Za-z0-9._-]+\.sh' scripts/check.sh | sed 's/^run //' | sort -u)
for gate in $gates; do
  printf '#!/usr/bin/env bash\nexit 0\n' > "$work/$gate"
  chmod +x "$work/$gate"
done

# FakeCargo: logs each call, answers the `cargo hack --version` probe from FAKE_CARGO_HACK
# (installed|absent) the way cargo does, and fails any call whose arguments contain FAKE_CARGO_FAIL.
cat > "$work/bin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
echo "cargo $*" >> "$FAKE_CARGO_LOG"
if [ "${1-}" = hack ] && [ "${2-}" = --version ]; then
  if [ "${FAKE_CARGO_HACK-}" = installed ]; then
    echo "cargo-hack 0.0.0"
    exit 0
  fi
  echo "error: no such command: \`hack\`" >&2
  exit 101
fi
case "$*" in
  *"${FAKE_CARGO_FAIL:-@@never@@}"*)
    echo "FakeCargo: failing as asked: cargo $*" >&2
    exit 101
    ;;
esac
exit 0
FAKE_CARGO
# FakeDprint: the formatter check is not under test.
printf '#!/usr/bin/env bash\nexit 0\n' > "$work/bin/dprint"
chmod +x "$work/bin/cargo" "$work/bin/dprint"

# run_check <installed|absent> [argument fragment FakeCargo fails on]
# Sets status, stdout_text, stderr_text and cargo_log for the assertions below.
run_check() {
  : > "$work/cargo.log"
  status=0
  (
    cd "$work"
    PATH="$work/bin:$PATH" FAKE_CARGO_LOG="$work/cargo.log" FAKE_CARGO_HACK=$1 \
      FAKE_CARGO_FAIL=${2-} scripts/check.sh > "$work/stdout" 2> "$work/stderr"
  ) || status=$?
  stdout_text=$(cat "$work/stdout")
  stderr_text=$(cat "$work/stderr")
  cargo_log=$(cat "$work/cargo.log")
}

# cargo-hack installed and every gate green: the CI command runs, once, and the script passes.
run_check installed
if [ "$status" -ne 0 ]; then
  fail "expected scripts/check.sh to pass with cargo-hack installed and every gate green, received exit $status: $stderr_text"
fi
hack_runs=$(printf '%s\n' "$cargo_log" | grep -c '^cargo hack clippy ' || true)
if [ "$hack_runs" -ne 1 ] || ! printf '%s\n' "$cargo_log" | grep -Fxq "cargo $ci_args"; then
  fail "expected scripts/check.sh to run ci.yml's powerset once: 'cargo $ci_args' | received $hack_runs run(s) among: $(printf '%s\n' "$cargo_log" | grep '^cargo hack ' || echo none)"
fi

# One feature pair failing Clippy fails the script, which then never reports success.
run_check installed 'hack clippy'
if [ "$status" -eq 0 ]; then
  fail "expected scripts/check.sh to fail when a feature pair fails Clippy | received: exit 0"
fi
case "$stdout_text" in
  *'check passed'*) fail "expected no 'check passed' after a failing feature pair | received: $stdout_text" ;;
esac

# cargo-hack missing: one warning naming the skipped gate, the powerset never runs, the rest does.
run_check absent
if [ "$status" -ne 0 ]; then
  fail "expected scripts/check.sh to pass without cargo-hack when every other gate is green | received: exit $status: $stderr_text"
fi
warnings=$(printf '%s\n' "$stderr_text" | grep -ci 'cargo-hack' || true)
if [ "$warnings" -ne 1 ] || ! printf '%s\n' "$stderr_text" | grep -i 'cargo-hack' | grep -qi 'skipped.*feature powerset'; then
  fail "expected exactly one warning that names cargo-hack and the skipped feature powerset | received $warnings line(s): $stderr_text"
fi
if printf '%s\n' "$cargo_log" | grep -q '^cargo hack clippy '; then
  fail "expected no powerset run without cargo-hack | received: $(printf '%s\n' "$cargo_log" | grep '^cargo hack clippy ')"
fi
for later in 'nextest run' 'doc --no-deps' 'deny check'; do
  if ! printf '%s\n' "$cargo_log" | grep -q "^cargo $later"; then
    fail "expected the gates after the skipped powerset to run: cargo $later | received: $cargo_log"
  fi
done

# Without cargo-hack the script still returns the status of the other gates.
run_check absent 'nextest run'
if [ "$status" -eq 0 ]; then
  fail "expected scripts/check.sh to fail when a gate other than the powerset fails | received: exit 0"
fi

echo 'check.sh feature powerset checks passed'
