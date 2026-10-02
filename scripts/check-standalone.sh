#!/usr/bin/env bash
# Compile and test the registry consumer outside every repository workspace.
# Usage: scripts/check-standalone.sh
set -euo pipefail
cd "$(dirname "$0")/.."

cmp fixtures/claude/help/2.1.260.txt tests/standalone/tests/fixtures/claude-help.txt
cmp fixtures/claude/transcripts/read-turn.jsonl tests/standalone/tests/fixtures/claude-turn.jsonl

consumer=$(mktemp -d "${TMPDIR:-/tmp}/independent-agent-host.XXXXXX")
trap 'rm -rf "$consumer"' EXIT
cp tests/standalone/{Cargo.toml,Cargo.lock,README.md} "$consumer/"
cp -R tests/standalone/{src,tests} "$consumer/"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/standalone"

echo "▶ registry consumer outside workspace: $consumer"
cargo metadata --manifest-path "$consumer/Cargo.toml" --locked --format-version 1 > "$consumer/metadata.json"
python3 - "$consumer/metadata.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    metadata = json.load(source)
expected = {"mango-external-agents", "mango-agent-acp", "mango-agent-claude", "mango-agent-codex"}
packages = {package["name"]: package for package in metadata["packages"]}
for name in sorted(expected):
    package = packages[name]
    assert package["version"] == "0.3.1", f"{name}: expected 0.3.1, received {package['version']}"
    assert package["source"] and package["source"].startswith("registry+"), f"{name}: expected registry source, received {package['source']}"
    print(f"{name} = {package['version']} ({package['source']})")
for name in packages:
    assert not name.startswith("mangostudio") and name != "mango-protocol", f"expected no product dependency, received {name}"
    assert name not in {"aws-lc-rs", "openssl", "openssl-sys"}, f"expected ring-only TLS dependencies, received {name}"
root = packages["independent-agent-host"]
assert metadata["workspace_members"] == [root["id"]], "expected only the independent consumer in its workspace"
for dependency in root["dependencies"]:
    if dependency["name"] in expected:
        assert dependency["req"] == "=0.3.1" and not dependency.get("path"), f"expected exact registry dependency, received {dependency}"
PY

cargo fmt --manifest-path "$consumer/Cargo.toml" -- --check
cargo test --manifest-path "$consumer/Cargo.toml" --no-default-features --locked
cargo test --manifest-path "$consumer/Cargo.toml" --all-features --locked
cargo clippy --manifest-path "$consumer/Cargo.toml" --all-targets --all-features --locked -- -D warnings
echo "✓ independent registry consumer passed"
