#!/usr/bin/env bash
# Named fake for scripts/test-check-standalone.sh, installed as both `cargo` and `rustc`. It logs
# every call to FAKE_CARGO_LOG and compiles nothing.
#
# `cargo metadata` prints the metadata of a consumer that satisfies scripts/check-standalone.sh:
# the four crates from the registry at FAKE_CARGO_PIN, all declaring rust-version 1.98, and the
# consumer as the only workspace member. Every other call succeeds silently.
set -euo pipefail

printf '%s %s\n' "$(basename "$0")" "$*" >> "${FAKE_CARGO_LOG:?expected FAKE_CARGO_LOG}"

if [ "$(basename "$0")" = cargo ] && [ "${1:-}" = metadata ]; then
  python3 - "${FAKE_CARGO_PIN:?expected FAKE_CARGO_PIN}" <<'PY'
import json
import sys

pin = sys.argv[1]
crates = ["mango-external-agents", "mango-agent-acp", "mango-agent-claude", "mango-agent-codex"]
registry = "registry+https://github.com/rust-lang/crates.io-index"
root_id = "path+file:///consumer#independent-agent-host@0.1.0"
packages = [
    {"name": name, "version": pin, "source": registry, "rust_version": "1.98", "dependencies": []}
    for name in crates
]
packages.append(
    {
        "name": "independent-agent-host",
        "id": root_id,
        "version": "0.1.0",
        "source": None,
        "rust_version": "1.98",
        "dependencies": [{"name": name, "req": f"={pin}", "path": None} for name in crates],
    }
)
json.dump({"packages": packages, "workspace_members": [root_id]}, sys.stdout)
PY
fi
