#!/usr/bin/env bash
# Named Cargo failures must not look like a dependency graph free of banned TLS providers.
set -euo pipefail
cd "$(dirname "$0")/.."
work=$(mktemp -d "${TMPDIR:-/tmp}/check-tls-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/scripts"
cp scripts/check-tls.sh "$work/scripts/"
real_grep=$(command -v grep)
cat > "$work/bin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FAKE_CARGO_LOG"
case "$FAKE_CARGO_CASE" in
  failed) echo 'FakeCargo: dependency query failed' >&2; exit "${FAKE_CARGO_STATUS:-101}" ;;
  clean) printf 'mango-external-agents v0.7.1\nring v0.17.14\nopenssl-probe v0.2.1\naws-lc-rs-helper v1.0.0\n' ;;
  *) printf '%s v1.0.0\n' "$FAKE_CARGO_CASE" ;;
esac
FAKE_CARGO
cat > "$work/bin/grep" <<'FAKE_GREP'
#!/usr/bin/env bash
if [ "$FAKE_GREP_STATUS" -ne 0 ]; then
  echo 'FakeGrep: dependency classifier failed' >&2
  exit "$FAKE_GREP_STATUS"
fi
exec "$REAL_GREP" "$@"
FAKE_GREP
chmod +x "$work/bin/cargo" "$work/bin/grep"
run_case() {
  : > "$work/cargo.log"
  status=0
  PATH="$work/bin:$PATH" FAKE_CARGO_CASE=$1 FAKE_CARGO_LOG="$work/cargo.log" \
    FAKE_CARGO_STATUS=${3:-101} FAKE_GREP_STATUS=${2:-0} REAL_GREP="$real_grep" \
    bash "$work/scripts/check-tls.sh" > "$work/output" 2>&1 || status=$?
}
run_case failed
if [ "$status" -ne 101 ] || grep -q 'TLS policy holds' "$work/output"; then
  echo "expected Cargo query failure 101 without a TLS success claim, received exit $status" >&2
  cat "$work/output" >&2
  exit 1
fi
run_case failed 0 78
if [ "$status" -ne 78 ] || grep -q 'TLS policy holds' "$work/output"; then
  echo "expected Cargo query failure 78 without a TLS success claim, received exit $status" >&2
  cat "$work/output" >&2
  exit 1
fi
run_case clean
if [ "$status" -ne 0 ] || ! grep -q 'TLS policy holds' "$work/output"; then
  echo "expected a clean graph to pass, received exit $status" >&2
  exit 1
fi
if [ "$(cat "$work/cargo.log")" != 'tree --workspace --all-features --locked -e normal --prefix none --format {p} --color never' ]; then
  echo 'expected one locked all-feature normal graph query, received:' >&2
  cat "$work/cargo.log" >&2
  exit 1
fi
for banned in aws-lc-rs aws-lc-sys openssl openssl-sys; do
  run_case "$banned"
  if [ "$status" -ne 1 ] || ! grep -q "^$banned v1.0.0$" "$work/output" || grep -q 'TLS policy holds' "$work/output"; then
    echo "expected banned $banned to fail with evidence, received exit $status" >&2
    exit 1
  fi
done
for failed in 2 127; do
  run_case clean "$failed"
  if [ "$status" -ne "$failed" ] || ! grep -q "dependency matcher exit $failed" "$work/output" || grep -q 'TLS policy holds' "$work/output"; then
    echo "expected classifier failure $failed without a TLS success claim, received exit $status" >&2
    cat "$work/output" >&2
    exit 1
  fi
done
echo 'TLS policy query/clean/prefix/banned/classifier cases passed'
