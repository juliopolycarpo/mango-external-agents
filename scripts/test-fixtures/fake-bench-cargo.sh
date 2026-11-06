#!/usr/bin/env bash
# Records benchmark invocations without compiling or timing them.
set -euo pipefail
python3 - "$FAKE_BENCH_CARGO_LOG" "$@" <<'PY'
import json
import sys

with open(sys.argv[1], "a", encoding="utf-8") as log:
    log.write(json.dumps(sys.argv[2:]) + "\n")
PY
