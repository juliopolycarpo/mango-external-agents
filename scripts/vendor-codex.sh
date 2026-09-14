#!/usr/bin/env bash
# Re-derives crates/mango-agent-codex/vendor/ from OpenAI's own schema generator.
#
# What is vendored is the vendor's *description* of the wire, not its Rust sources. The transitive
# closure of codex-app-server-protocol and codex-protocol is 32 in-repo crates and 177,655 lines
# behind 107 third-party crates — including native-tls, which deny.toml bans, plus sqlx,
# tree-sitter, opentelemetry, landlock and seccompiler. Copying that in to obtain a few dozen
# structs would import a TLS stack this workspace refuses. docs/harness-codex.md records the
# measurement.
#
# So: `codex app-server generate-json-schema` (a documented app-server subcommand) is run against
# the pinned build, and the inventory of every type this harness speaks — each definition's field
# names and enum values, plus the JSON-RPC method discriminators — is written to
# vendor/schema.json. A rename upstream shows up as a diff here rather than as a -32602 on a
# user's machine.
#
# Usage: scripts/vendor-codex.sh <version> [--out DIR]
#
# `--out` is for contract checks: it keeps a fresh inventory in a scratch directory so a CI run can
# compare it without modifying the checkout. Omitting it updates the checked-in vendor directory.
set -euo pipefail

usage() {
  echo "usage: scripts/vendor-codex.sh <version> [--out DIR]" >&2
}

parse_arguments() {
  if [ $# -eq 0 ]; then
    echo "expected one argument, the codex version to pin (e.g. 0.153.4), received 0" >&2
    return 2
  fi

  version="$1"
  vendor_dir='crates/mango-agent-codex/vendor'
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
      --out)
        if [ $# -lt 2 ]; then
          echo 'expected a directory after --out, received none' >&2
          return 2
        fi
        vendor_dir="$2"
        shift 2
        ;;
      *)
        printf 'expected --out DIR after the codex version, received %s\n' "$1" >&2
        return 2
        ;;
    esac
  done
}

generate_inventory() {
  local installed
  local workdir

  if ! command -v codex >/dev/null 2>&1; then
    echo "expected codex on PATH to generate the schema, received none" >&2
    return 2
  fi

  installed=$(codex --version | tr -d '\r' | awk '{print $NF}')
  if [ "$installed" != "$version" ]; then
    echo "expected codex $version on PATH, received $installed" >&2
    return 1
  fi

  workdir=$(mktemp -d)
  trap "rm -rf -- $(printf '%q' "$workdir")" RETURN
  codex app-server generate-json-schema --out "$workdir" >/dev/null

  mkdir -p "$vendor_dir"
  printf 'rust-v%s\n' "$version" > "$vendor_dir/PIN"
  python3 scripts/codex-schema-inventory.py \
    "$workdir/codex_app_server_protocol.v2.schemas.json" \
    "$workdir/codex_app_server_protocol.schemas.json" \
    "$version" \
    > "$vendor_dir/schema.json"

  trap - RETURN
  rm -rf -- "$workdir"
  echo "generated the codex $version schema inventory in $vendor_dir"
}

main() {
  cd "$(dirname "$0")/.."
  parse_arguments "$@"
  generate_inventory
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
