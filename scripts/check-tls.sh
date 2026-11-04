#!/usr/bin/env bash
# TLS is `ring` everywhere. deny.toml bans aws-lc-rs and openssl across every target;
# this is the same rule as one readable line, run on the host's target in CI.
set -euo pipefail
cd "$(dirname "$0")/.."
# Query once, preserving Cargo failures instead of interpreting every nonzero inverse query as
# an absent package. The locked graph keeps this check aligned with the validated dependency set.
if graph=$(cargo tree --workspace --all-features --locked -e normal --prefix none --format '{p}'); then
  :
else
  status=$?
  echo "expected a readable locked dependency graph, received Cargo exit $status" >&2
  exit "$status"
fi
if banned=$(printf '%s\n' "$graph" | grep -E '^(aws-lc-rs|aws-lc-sys|openssl|openssl-sys)[[:space:]]'); then
  echo "expected no banned TLS dependency, received:" >&2
  printf '%s\n' "$banned" >&2
  exit 1
fi
echo "TLS policy holds: no aws-lc-rs, no openssl"
