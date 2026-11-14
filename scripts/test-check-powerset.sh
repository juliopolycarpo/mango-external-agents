#!/usr/bin/env bash
# Regression coverage for scripts/check-features.sh, the one driver behind the feature powerset
# (scripts/check.sh, ci.yml's `rust` job) and the minimum-Rust feature check (ci.yml's `msrv` job).
# With cargo-hack installed check.sh runs the powerset lane and fails when it fails; without
# cargo-hack it warns once, names the skipped gate, and still returns the status of every other gate.
#
# The real check.sh and the real driver run in a scratch tree where every other gate is a named
# fake, so a case costs milliseconds and no cargo build. The expected cargo commands are written
# out below from an independent list of the four library crates, so dropping a crate from the
# driver, loosening the depth, or dropping a flag fails this test. ci.yml and check.sh must reach
# those commands through the driver and carry no cargo-hack command of their own.
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d "${TMPDIR:-/tmp}/check-powerset-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

fail() { echo "$*" >&2; exit 1; }

# Independent of the driver's own list: every published library crate.
library_crates=(mango-external-agents mango-agent-claude mango-agent-codex mango-agent-acp)
packages=''
for crate in "${library_crates[@]}"; do packages="$packages -p $crate"; done
packages=${packages# }

# ci.yml reaches each lane through the driver, once, and holds no cargo-hack command of its own.
for lane_step in 'powerset --locked' 'msrv --locked'; do
  ci_count=$(grep -cxE " *- run: scripts/check-features\.sh $lane_step" .github/workflows/ci.yml || true)
  if [ "$ci_count" -ne 1 ]; then
    fail "expected exactly one 'scripts/check-features.sh $lane_step' step in .github/workflows/ci.yml | received $ci_count"
  fi
done
# check.sh may probe for the tool with `cargo hack --version`; nothing else may run it.
for file in .github/workflows/ci.yml scripts/check.sh; do
  stray=$(grep -v '^ *#' "$file" | grep 'cargo hack' | grep -v 'cargo hack --version' || true)
  if [ -n "$stray" ]; then
    fail "expected $file to run cargo hack only through scripts/check-features.sh | received: $stray"
  fi
done
# The workspace clippy with no default features selects test targets, so it needs the copy as well.
for file in .github/workflows/ci.yml .github/workflows/fresh-dependencies.yml scripts/check.sh; do
  bare=$(grep -v '^ *#' "$file" | grep -c 'clippy --workspace --all-targets --no-default-features' || true)
  wrapped=$(grep -v '^ *#' "$file" | grep -c 'run-without-self-dev-dependency.sh cargo clippy --workspace --all-targets --no-default-features' || true)
  if [ "$bare" -ne 1 ] || [ "$wrapped" -ne 1 ]; then
    fail "expected $file to run the workspace clippy with --no-default-features once, through scripts/run-without-self-dev-dependency.sh | received $bare run(s), $wrapped wrapped"
  fi
done
expected_powerset="cargo hack clippy --feature-powerset --depth 2 --all-targets --locked $packages -- -D warnings"
# The lanes that select test targets run inside the copy without the self dev-dependency; the
# wrapper's own log line is how this test sees that, since FakeWithoutSelf only logs and execs.
wrapper_prefix='without-self-dev-dependency:'

mkdir -p "$work/scripts" "$work/bin"
cp scripts/check.sh scripts/check-features.sh "$work/scripts/"
# The driver reads the declared minimum from the workspace manifest in its msrv lane.
cp Cargo.toml "$work/Cargo.toml"

# FakeWithoutSelf: stands in for scripts/run-without-self-dev-dependency.sh. It logs the wrapped
# command, then runs it in place; the real script has its own test.
cat > "$work/scripts/run-without-self-dev-dependency.sh" <<'FAKE_WITHOUT_SELF'
#!/usr/bin/env bash
echo "without-self-dev-dependency: $*" >> "$FAKE_CARGO_LOG"
exec "$@"
FAKE_WITHOUT_SELF
chmod +x "$work/scripts/run-without-self-dev-dependency.sh"

# FakeGate: stands in for every directly executed script that check.sh runs, except the driver
# (the unit under test, which stays real) and the runner (FakeWithoutSelf above).
gates=$(grep -oE 'run scripts/[A-Za-z0-9._-]+\.(sh|py)' scripts/check.sh | sed 's/^run //' | sort -u | grep -vxF -e scripts/check-features.sh -e scripts/run-without-self-dev-dependency.sh)
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
# FakeRustc: answers `rustc +<toolchain> --version` for any toolchain, as if it were installed.
printf '#!/usr/bin/env bash\necho "rustc 0.0.0 (FakeRustc)"\n' > "$work/bin/rustc"
# FakePython: Python-based gates are outside the powerset test.
printf '#!/usr/bin/env bash\nexit 0\n' > "$work/bin/python3"
# FakeDprint: the formatter check is not under test.
printf '#!/usr/bin/env bash\nexit 0\n' > "$work/bin/dprint"
chmod +x "$work/bin/cargo" "$work/bin/dprint" "$work/bin/python3" "$work/bin/rustc"

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
if [ "$hack_runs" -ne 1 ] || ! printf '%s\n' "$cargo_log" | grep -Fxq "$expected_powerset"; then
  fail "expected scripts/check.sh to run the powerset once: '$expected_powerset' | received $hack_runs run(s) among: $(printf '%s\n' "$cargo_log" | grep '^cargo hack clippy ' || echo none)"
fi
# The all-targets powerset must run inside the copy: in the real tree the self dev-dependency would
# widen every reduced-feature target to the test features.
if ! printf '%s\n' "$cargo_log" | grep -Fxq "$wrapper_prefix $expected_powerset"; then
  fail "expected the powerset to run through scripts/run-without-self-dev-dependency.sh: '$wrapper_prefix $expected_powerset' | received: $(printf '%s\n' "$cargo_log" | grep -F "$wrapper_prefix" || echo none)"
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

# The msrv lane through the driver: the declared minimum toolchain, one cargo-hack run over every
# library crate for the bare crate and each feature alone, then the two core lines that no
# per-feature run makes. Dropping a crate, or a flag, from the driver fails here.
toolchain=$(scripts/check-features.sh --print-toolchain)
case "$toolchain" in
  1.[0-9]*.0) ;;
  *) fail "expected --print-toolchain to print a full minimum toolchain like 1.97.0 | received: $toolchain" ;;
esac
: > "$work/cargo.log"
status=0
(
  cd "$work"
  PATH="$work/bin:$PATH" FAKE_CARGO_LOG="$work/cargo.log" FAKE_CARGO_HACK=installed \
    scripts/check-features.sh msrv --locked > "$work/stdout" 2> "$work/stderr"
) || status=$?
if [ "$status" -ne 0 ]; then
  fail "expected scripts/check-features.sh msrv to pass with cargo-hack and every cargo call green | received exit $status: $(cat "$work/stderr")"
fi
cargo_log=$(cat "$work/cargo.log")
# Only the line that builds test targets runs in the copy; the library-only runs keep the real tree.
wrapped=$(printf '%s\n' "$cargo_log" | grep -F "$wrapper_prefix" || true)
expected_wrapped="$wrapper_prefix cargo +$toolchain check -p mango-external-agents --no-default-features --all-targets --locked"
if [ "$wrapped" != "$expected_wrapped" ]; then
  fail "expected the msrv lane to wrap only the bare-core all-targets check: '$expected_wrapped' | received: ${wrapped:-none}"
fi
for expected in \
  "cargo +$toolchain hack check --each-feature --skip default --exclude-all-features --locked $packages" \
  "cargo +$toolchain check -p mango-external-agents --all-features --locked" \
  "cargo +$toolchain check -p mango-external-agents --no-default-features --all-targets --locked"; do
  if ! printf '%s\n' "$cargo_log" | grep -Fxq "$expected"; then
    fail "expected the msrv lane to run: '$expected' | received: $(printf '%s\n' "$cargo_log" | grep -v 'hack --version')"
  fi
done

# The msrv lane without cargo-hack is an error, never a skip: only check.sh skips the powerset.
: > "$work/cargo.log"
status=0
(
  cd "$work"
  PATH="$work/bin:$PATH" FAKE_CARGO_LOG="$work/cargo.log" FAKE_CARGO_HACK=absent \
    scripts/check-features.sh msrv --locked > "$work/stdout" 2> "$work/stderr"
) || status=$?
if [ "$status" -eq 0 ] || ! grep -q 'expected cargo-hack on PATH for the msrv lane' "$work/stderr"; then
  fail "expected scripts/check-features.sh msrv to fail naming the missing cargo-hack | received exit $status: $(cat "$work/stderr")"
fi

# An unknown lane is a usage error that names the received value.
status=0
scripts/check-features.sh nightly > "$work/stdout" 2> "$work/stderr" || status=$?
if [ "$status" -ne 2 ] || ! grep -q 'received: nightly' "$work/stderr"; then
  fail "expected scripts/check-features.sh nightly to exit 2 naming 'nightly' | received exit $status: $(cat "$work/stderr")"
fi

echo 'check-features.sh lane checks passed'
