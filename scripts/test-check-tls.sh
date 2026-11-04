#!/usr/bin/env bash
# Named Cargo failures must not look like a dependency graph free of banned TLS providers.
set -euo pipefail
cd "$(dirname "$0")/.."
work=$(mktemp -d "${TMPDIR:-/tmp}/check-tls-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/scripts"
cp scripts/check-tls.sh "$work/scripts/"
cat > "$work/bin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FAKE_CARGO_LOG"
case "$FAKE_CARGO_CASE" in
  failed) echo 'FakeCargo: dependency query failed' >&2; exit 101 ;;
  clean) printf 'mango-external-agents v0.7.1\nring v0.17.14\n' ;;
  *) printf '%s v1.0.0\n' "$FAKE_CARGO_CASE" ;;
esac
FAKE_CARGO
chmod +x "$work/bin/cargo"
run_case() {
  : > "$work/cargo.log"
  status=0
  PATH="$work/bin:$PATH" FAKE_CARGO_CASE=$1 FAKE_CARGO_LOG="$work/cargo.log" \
    bash "$work/scripts/check-tls.sh" > "$work/output" 2>&1 || status=$?
}
run_case failed
if [ "$status" -ne 101 ] || grep -q 'TLS policy holds' "$work/output"; then
  echo "expected Cargo query failure 101 without a TLS success claim, received exit $status" >&2
  cat "$work/output" >&2
  exit 1
fi
run_case clean
if [ "$status" -ne 0 ] || ! grep -q 'TLS policy holds' "$work/output"; then
  echo "expected a clean graph to pass, received exit $status" >&2
  exit 1
fi
if [ "$(wc -l < "$work/cargo.log" | tr -d ' ')" -ne 1 ] || ! grep -q -- '--locked' "$work/cargo.log"; then
  echo 'expected one locked dependency query, received:' >&2
  cat "$work/cargo.log" >&2
  exit 1
fi
for banned in aws-lc-rs aws-lc-sys openssl openssl-sys; do
  run_case "$banned"
  if [ "$status" -eq 0 ] || ! grep -q "$banned" "$work/output"; then
    echo "expected banned $banned to fail with evidence, received exit $status" >&2
    exit 1
  fi
done
echo 'TLS policy failure/clean/banned cases passed'
