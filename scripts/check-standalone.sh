#!/usr/bin/env bash
# Compile and test the registry consumers outside every repository workspace.
#
#   current     tests/standalone-current: the published release the workspace last shipped (or the
#               one before it, until the post-release bump; see scripts/check-consumer-pin.sh).
#   historical  tests/standalone-historical: a fixed =0.3.1 control on its own, older minimum
#               compiler. It is not evidence for any later release or for this checkout.
#
# Each consumer is copied to a temporary directory and built from there, but rustup picks the
# compiler from the working directory, which is the repository root: the pinned toolchain in
# rust-toolchain.toml applies unless `RUSTUP_TOOLCHAIN` is set. A job that needs a consumer's own
# minimum sets `RUSTUP_TOOLCHAIN` and passes `--test-only`, because the minimal toolchain profile
# carries neither rustfmt nor clippy.
#
# The consumer's declared `rust-version` must be what the pinned release declares on the registry,
# so a consumer cannot claim a minimum its crates do not.
#
# Usage: scripts/check-standalone.sh [--test-only] [current|historical|all]   # default: all
set -euo pipefail
cd "$(dirname "$0")/.."

test_only=false
selection=all
for arg in "$@"; do
  case "$arg" in
    --test-only) test_only=true ;;
    current | historical | all) selection=$arg ;;
    *) echo "unknown argument: $arg (expected --test-only, current, historical or all)" >&2; exit 2 ;;
  esac
done

scratch=$(mktemp -d "${TMPDIR:-/tmp}/independent-agent-host.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
target_base="${CARGO_TARGET_DIR:-$PWD/target}"

check_consumer() {
  local name=$1 source="tests/standalone-$1" pin consumer
  cmp fixtures/claude/help/2.1.260.txt "$source/tests/fixtures/claude-help.txt"
  cmp fixtures/claude/transcripts/read-turn.jsonl "$source/tests/fixtures/claude-turn.jsonl"
  pin=$(sed -n 's/^mango-external-agents = { version = "=\([^"]*\)".*/\1/p' "$source/Cargo.toml" | sort -u)
  if [ -z "$pin" ] || [ "$(printf '%s\n' "$pin" | grep -c .)" -ne 1 ]; then
    echo "$source/Cargo.toml: expected one exact '=x.y.z' mango-external-agents pin, received '${pin//$'\n'/ }'" >&2
    return 1
  fi

  consumer="$scratch/$name"
  mkdir "$consumer"
  cp "$source"/{Cargo.toml,Cargo.lock,README.md} "$consumer/"
  cp -R "$source"/{src,tests} "$consumer/"
  export CARGO_TARGET_DIR="$target_base/standalone-$name"

  echo "▶ $name registry consumer (=$pin) outside workspace: $consumer"
  rustc --version
  cargo metadata --manifest-path "$consumer/Cargo.toml" --locked --format-version 1 > "$consumer/metadata.json"
  python3 - "$consumer/metadata.json" "$pin" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    metadata = json.load(source)
pin = sys.argv[2]
expected = {"mango-external-agents", "mango-agent-acp", "mango-agent-claude", "mango-agent-codex"}
packages = {package["name"]: package for package in metadata["packages"]}
for name in sorted(expected):
    package = packages[name]
    assert package["version"] == pin, f"{name}: expected {pin}, received {package['version']}"
    assert package["source"] and package["source"].startswith("registry+"), f"{name}: expected registry source, received {package['source']}"
    print(f"{name} = {package['version']} ({package['source']}), rust-version {package['rust_version']}")
for name in packages:
    assert not name.startswith("mangostudio") and name != "mango-protocol", f"expected no product dependency, received {name}"
    assert name not in {"aws-lc-rs", "openssl", "openssl-sys"}, f"expected ring-only TLS dependencies, received {name}"
root = packages["independent-agent-host"]
for name in sorted(expected):
    assert root["rust_version"] == packages[name]["rust_version"], f"expected consumer rust-version {packages[name]['rust_version']} (declared by {name} {pin}), received {root['rust_version']}"
assert metadata["workspace_members"] == [root["id"]], "expected only the independent consumer in its workspace"
for dependency in root["dependencies"]:
    if dependency["name"] in expected:
        assert dependency["req"] == f"={pin}" and not dependency.get("path"), f"expected exact registry dependency ={pin}, received {dependency}"
PY

  if [ "$test_only" = false ]; then
    cargo fmt --manifest-path "$consumer/Cargo.toml" -- --check
  fi
  cargo test --manifest-path "$consumer/Cargo.toml" --no-default-features --locked
  cargo test --manifest-path "$consumer/Cargo.toml" --all-features --locked
  if [ "$test_only" = false ]; then
    cargo clippy --manifest-path "$consumer/Cargo.toml" --all-targets --all-features --locked -- -D warnings
  fi
  echo "✓ $name registry consumer passed"
}

case "$selection" in
  current) check_consumer current ;;
  historical) check_consumer historical ;;
  all)
    check_consumer current
    check_consumer historical
    ;;
esac
echo "✓ independent registry consumers passed"
