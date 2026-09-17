#!/usr/bin/env bash
# Named fake that supplies a known-good Codex schema inventory to the standalone script test.
set -euo pipefail

cat "${FAKE_CODEX_SCHEMA:?expected FAKE_CODEX_SCHEMA}"
