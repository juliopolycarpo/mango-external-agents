#!/usr/bin/env bash
# Named fake for the successful standalone Codex inventory test.
set -euo pipefail

case "$1" in
  --version)
    echo 'codex-cli 0.154.0'
    ;;
  app-server)
    mkdir -p "$4"
    printf '{}\n' > "$4/codex_app_server_protocol.v2.schemas.json"
    printf '{}\n' > "$4/codex_app_server_protocol.schemas.json"
    ;;
  *)
    printf 'expected --version or app-server, received %s\n' "$1" >&2
    exit 2
    ;;
esac
