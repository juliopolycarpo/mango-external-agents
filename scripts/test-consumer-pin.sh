#!/usr/bin/env bash
# Regression coverage for the registry consumer's pin rule: the pin may trail the workspace version
# by one release and no more, and a failure names both values.
set -euo pipefail
cd "$(dirname "$0")/.."

for pair in '0.4.1 0.4.1' '0.4.2 0.4.1' '0.5.0 0.4.9' '0.5.0 0.4.0' '1.0.0 0.9.3' '1.2.0 1.1.4' '0.5.0-rc.1 0.4.9' \
  '0.5.0-rc.2 0.5.0-rc.1' '0.5.0 0.5.0-rc.1'; do
  # shellcheck disable=SC2086
  if ! out=$(scripts/check-consumer-pin.sh $pair 2>&1); then
    echo "expected acceptance of workspace/pin '$pair', received: $out" >&2
    exit 1
  fi
done

for pair in '0.4.3 0.4.1' '0.4.1 0.4.2' '0.5.1 0.4.9' '0.6.0 0.4.9' '0.5.1 0.5.0-rc.1' '0.5.0 0.4.9-rc.1' '2.0.0 0.4.9' \
  '0.4.2 latest' 'banana 0.4.1'; do
  # shellcheck disable=SC2086
  if out=$(scripts/check-consumer-pin.sh $pair 2>&1); then
    echo "expected rejection of workspace/pin '$pair', received success: $out" >&2
    exit 1
  fi
done

# The diagnostic must carry the value it refused and the one it expected.
out=$(scripts/check-consumer-pin.sh 0.4.3 0.4.1 2>&1 || true)
case "$out" in
  *'expected pin =0.4.3 or the release just before it, received =0.4.1'*) ;;
  *)
    echo "expected a diagnostic naming pin 0.4.1 and workspace 0.4.3, received: $out" >&2
    exit 1
    ;;
esac

# Two different pins across the four crates are refused, naming both. Run the real script against a
# scratch tree so the manifest can be varied.
work=$(mktemp -d "${TMPDIR:-/tmp}/consumer-pin-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/scripts" "$work/tests/standalone-current"
cp scripts/check-consumer-pin.sh "$work/scripts/"
printf '[workspace.package]\nversion = "0.5.0"\n' > "$work/Cargo.toml"
printf '[dependencies]\nmango-external-agents = { version = "=0.5.0" }\nmango-agent-acp = { version = "=0.4.9" }\n' > "$work/tests/standalone-current/Cargo.toml"
if out=$("$work/scripts/check-consumer-pin.sh" 2>&1); then
  echo "expected rejection of two different pins, received success: $out" >&2
  exit 1
fi
case "$out" in *'received: 0.4.9 0.5.0'*) ;; *) echo "expected both pins named, received: $out" >&2; exit 1 ;; esac

# The committed manifests must satisfy the rule right now.
scripts/check-consumer-pin.sh >/dev/null
echo 'registry consumer pin checks passed'
