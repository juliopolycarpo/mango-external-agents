#!/usr/bin/env bash
# Exercise package admission without running Cargo or building archives.
set -euo pipefail
cd "$(dirname "$0")/.."
work=$(mktemp -d "${TMPDIR:-/tmp}/mango-package-admission.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
mkdir -p "$work/tree/scripts" "$work/tree/.github/workflows" "$work/bin"
cp scripts/test-packages.sh "$work/tree/scripts/"
printf '  CRATES: mango-external-agents\n' > "$work/tree/.github/workflows/release.yml"

# FakeGit accepts only the status query used by package admission.
cat > "$work/bin/git" <<'FAKE_GIT'
#!/usr/bin/env bash
set -euo pipefail
if [ "$*" != 'status --porcelain' ]; then
  echo "FakeGit: expected status --porcelain, received $*" >&2
  exit 3
fi
if [ "$FAKE_GIT_STATE" = dirty ]; then
  echo ' M tracked-file'
fi
FAKE_GIT
# FakeCargo records the invocation and stops before archive creation.
cat > "$work/bin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$FAKE_CARGO_LOG"
exit 73
FAKE_CARGO
chmod +x "$work/bin/git" "$work/bin/cargo"
export PATH="$work/bin:$PATH" RUSTUP_TOOLCHAIN=fake FAKE_CARGO_LOG="$work/cargo.log"

run_case() {
  local state=$1
  shift
  rm -f "$FAKE_CARGO_LOG"
  set +e
  FAKE_GIT_STATE="$state" "$work/tree/scripts/test-packages.sh" "$@" > "$work/output" 2>&1
  status=$?
  set -e
}

run_case dirty
if [ -e "$FAKE_CARGO_LOG" ]; then
  echo 'dirty default: expected zero Cargo calls, received a Cargo call' >&2
  exit 1
fi
if [ "$status" -eq 0 ] || ! grep -Fq 'expected a clean working tree, received dirty' "$work/output"; then
  echo "dirty default: expected clean working tree refusal, received status $status and:" >&2
  cat "$work/output" >&2
  exit 1
fi
run_case clean
if [ "$status" -ne 73 ] || [ ! -f "$FAKE_CARGO_LOG" ] || grep -Fxq -- '--allow-dirty' "$FAKE_CARGO_LOG"; then
  echo "clean default: expected Cargo without --allow-dirty and status 73, received status $status" >&2
  exit 1
fi
run_case dirty --allow-dirty
if [ "$status" -ne 73 ] || [ ! -f "$FAKE_CARGO_LOG" ] || ! grep -Fxq -- '--allow-dirty' "$FAKE_CARGO_LOG"; then
  echo "dirty opt-in: expected Cargo with --allow-dirty and status 73, received status $status" >&2
  exit 1
fi
run_case clean --invalid
if [ "$status" -eq 0 ] || [ -e "$FAKE_CARGO_LOG" ] || ! grep -Fq 'expected --allow-dirty, received --invalid' "$work/output"; then
  echo "invalid argument: expected rejection before Cargo, received status $status and:" >&2
  cat "$work/output" >&2
  exit 1
fi
echo 'package admission checks passed'
