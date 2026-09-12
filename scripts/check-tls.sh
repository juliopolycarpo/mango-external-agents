#!/usr/bin/env bash
# TLS is `ring` everywhere. deny.toml bans aws-lc-rs and openssl across every target;
# this is the same rule as one readable line, run on the host's target in CI.
set -euo pipefail
cd "$(dirname "$0")/.."
for banned in aws-lc-rs aws-lc-sys openssl openssl-sys; do
  if cargo tree --workspace --all-features -e normal -i "$banned" >/dev/null 2>&1; then
    echo "expected no dependency on $banned, received:" >&2
    cargo tree --workspace --all-features -e normal -i "$banned" >&2
    exit 1
  fi
done
echo "TLS policy holds: no aws-lc-rs, no openssl"
